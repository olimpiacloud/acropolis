use crate::AsyncSink;
use anyhow::{Result, anyhow, bail};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use futures::stream::FuturesUnordered;
use reqwest::header::{CONTENT_RANGE, HeaderMap, RANGE};
use reqwest::{Client, StatusCode};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub segment: u64,
    pub parallel: usize,
    pub stall: Duration,
    pub hedge_min: Duration,
    pub max_attempts: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            segment: 8 << 20,
            parallel: 6,
            stall: Duration::from_secs(4),
            hedge_min: Duration::from_millis(2500),
            max_attempts: 6,
        }
    }
}

pub const MIN_SEGMENTED: u64 = 12 << 20;

#[derive(Debug)]
pub struct NoRanges;

impl std::fmt::Display for NoRanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("server does not support range requests")
    }
}

impl std::error::Error for NoRanges {}

async fn fetch_range(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    start: u64,
    end: u64,
    stall: Duration,
) -> Result<Bytes> {
    acropolis_events::add_request();
    let resp = tokio::time::timeout(
        stall * 3,
        client
            .get(url)
            .headers(headers.clone())
            .header(RANGE, format!("bytes={start}-{end}"))
            .send(),
    )
    .await
    .map_err(|_| anyhow!("timed out waiting for response"))??;
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        StatusCode::OK => return Err(anyhow!(NoRanges)),
        s => bail!("range request {start}-{end} failed: {s}"),
    }
    let range = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !range.starts_with(&format!("bytes {start}-{end}/")) {
        bail!("range request {start}-{end} answered with Content-Range {range:?}");
    }
    let want = (end - start + 1) as usize;
    let mut buf = BytesMut::with_capacity(want);
    let mut stream = resp.bytes_stream();
    loop {
        match tokio::time::timeout(stall, stream.next()).await {
            Err(_) => bail!("stalled at {} of {want} bytes", buf.len()),
            Ok(None) => break,
            Ok(Some(chunk)) => {
                let chunk = chunk?;
                acropolis_events::add_downloaded(chunk.len() as u64);
                buf.extend_from_slice(&chunk);
                if buf.len() > want {
                    bail!("range response longer than the {want} bytes requested");
                }
            }
        }
    }
    if buf.len() != want {
        bail!("short range response: {} of {want} bytes", buf.len());
    }
    Ok(buf.freeze())
}

pub async fn download<S: AsyncSink>(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    size: u64,
    policy: Policy,
    sink: S,
) -> Result<u64> {
    download_sources(
        client,
        (url.to_string(), headers.clone()),
        async { None },
        size,
        policy,
        sink,
    )
    .await
}

pub async fn download_sources<S: AsyncSink, A: std::future::Future<Output = Option<(String, HeaderMap)>>>(
    client: &Client,
    primary: (String, HeaderMap),
    alternate: A,
    size: u64,
    policy: Policy,
    mut sink: S,
) -> Result<u64> {
    let url = primary.0.clone();
    let url = url.as_str();
    let mut sources: Vec<(String, HeaderMap)> = vec![primary];
    let alternate = futures::FutureExt::fuse(alternate);
    tokio::pin!(alternate);
    let n = size.div_ceil(policy.segment) as usize;
    let seg = |i: usize| -> (u64, u64) {
        let a = i as u64 * policy.segment;
        (a, (a + policy.segment).min(size) - 1)
    };
    let mut running: FuturesUnordered<_> = FuturesUnordered::new();
    // Per segment: attempts in flight, when the first started, and handles to cancel them once one wins.
    let mut in_flight: HashMap<usize, (usize, Instant, Vec<AbortHandle>)> = HashMap::new();
    let mut attempts: HashMap<usize, u32> = HashMap::new();
    let mut done: BTreeMap<usize, Bytes> = BTreeMap::new();
    let mut durations: Vec<Duration> = Vec::new();
    let mut next_launch = 0usize;
    let mut next_emit = 0usize;
    let mut written = 0u64;
    let launch = |i: usize,
                  sources: &[(String, HeaderMap)],
                  running: &mut FuturesUnordered<_>,
                  in_flight: &mut HashMap<usize, (usize, Instant, Vec<AbortHandle>)>,
                  attempts: &mut HashMap<usize, u32>| {
        let (a, b) = seg(i);
        let c = client.clone();
        let attempt = attempts.get(&i).copied().unwrap_or(0) as usize;
        let (u, h) = sources[attempt % sources.len()].clone();
        let stall = policy.stall;
        let started = Instant::now();
        let (abort, reg) = AbortHandle::new_pair();
        let e = in_flight.entry(i).or_insert((0, started, Vec::new()));
        e.0 += 1;
        e.2.push(abort);
        *attempts.entry(i).or_insert(0) += 1;
        running.push(Abortable::new(
            async move { (i, started, fetch_range(&c, &u, &h, a, b, stall).await) },
            reg,
        ));
    };
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    while next_emit < n {
        while next_launch < n && in_flight.len() < policy.parallel && done.len() < policy.parallel * 2 {
            if !done.contains_key(&next_launch) && !in_flight.contains_key(&next_launch) {
                launch(next_launch, &sources, &mut running, &mut in_flight, &mut attempts);
            }
            next_launch += 1;
        }
        tokio::select! {
            Some(src) = &mut alternate => {
                sources.push(src);
            }
            Some(r) = running.next() => {
                // Aborted: a duplicate of this segment already won.
                let Ok((i, started, res)) = r else { continue };
                match res {
                    Ok(bytes) => {
                        if let Some((_, _, losers)) = in_flight.remove(&i) {
                            losers.iter().for_each(AbortHandle::abort);
                        }
                        if i >= next_emit && !done.contains_key(&i) {
                            durations.push(started.elapsed());
                            done.insert(i, bytes);
                        }
                    }
                    Err(e) => {
                        if e.downcast_ref::<NoRanges>().is_some() {
                            if written == 0 {
                                return Err(e);
                            }
                            bail!("{url} stopped honoring range requests after {written} bytes");
                        }
                        let entry = in_flight.get_mut(&i).map(|x| { x.0 -= 1; x.0 });
                        if entry == Some(0) {
                            in_flight.remove(&i);
                            if !done.contains_key(&i) && i >= next_emit {
                                if attempts.get(&i).copied().unwrap_or(0) >= policy.max_attempts {
                                    return Err(e.context(format!("segment {i} of {url}")));
                                }
                                launch(i, &sources, &mut running, &mut in_flight, &mut attempts);
                            }
                        }
                    }
                }
                while let Some(b) = done.remove(&next_emit) {
                    written += b.len() as u64;
                    sink.push(b).await?;
                    next_emit += 1;
                }
            }
            _ = tick.tick() => {
                let mut sorted = durations.clone();
                sorted.sort();
                let median = sorted.get(sorted.len() / 2).copied().unwrap_or(policy.hedge_min);
                let threshold = (median * 5 / 2).max(policy.hedge_min);
                for i in next_emit..(next_emit + policy.parallel).min(n) {
                    if let Some(&(count, started, _)) = in_flight.get(&i)
                        && count == 1
                        && started.elapsed() > threshold
                        && attempts.get(&i).copied().unwrap_or(0) < policy.max_attempts
                    {
                        launch(i, &sources, &mut running, &mut in_flight, &mut attempts);
                        if let Some(x) = in_flight.get_mut(&i) {
                            x.1 = started;
                        }
                        break;
                    }
                }
            }
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Collect(Vec<u8>);
    impl AsyncSink for Collect {
        async fn push(&mut self, b: Bytes) -> Result<()> {
            self.0.extend_from_slice(&b);
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hedged_loser_is_dropped() {
        const SEG: usize = 1024;
        let body: Arc<Vec<u8>> = Arc::new((0..4 * SEG).map(|i| (i % 251) as u8).collect());
        // The first request for segment 0 stalls mid-body; the last segment answers only once that connection
        // closed (or after 3 s), so the download is still running when the loser must be cancelled.
        let (first, closed, closed_in_time) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/blob", listener.local_addr().unwrap());
        let (b, f, c, t) = (body.clone(), first.clone(), closed.clone(), closed_in_time.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let (body, first, closed, closed_in_time) = (b.clone(), f.clone(), c.clone(), t.clone());
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let mut byte = [0u8; 1];
                    while !req.ends_with(b"\r\n\r\n") {
                        if conn.read(&mut byte).unwrap_or(0) == 0 {
                            return;
                        }
                        req.push(byte[0]);
                    }
                    let req = String::from_utf8_lossy(&req).to_ascii_lowercase();
                    let (a, z) = req
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.trim().split_once('-'))
                        .map(|(a, z)| (a.parse::<usize>().unwrap(), z.parse::<usize>().unwrap()))
                        .unwrap();
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{z}/{}\r\nConnection: close\r\n\r\n",
                        z - a + 1,
                        body.len()
                    );
                    conn.write_all(head.as_bytes()).unwrap();
                    if a == 0 && first.fetch_add(1, Ordering::SeqCst) == 0 {
                        conn.write_all(&body[..10]).unwrap();
                        // The client sends nothing more, so this returns only when it closes the connection.
                        let _ = conn.read(&mut byte);
                        closed.store(true, Ordering::SeqCst);
                        return;
                    }
                    if a == 3 * SEG {
                        let start = Instant::now();
                        while !closed.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(3) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        closed_in_time.store(closed.load(Ordering::SeqCst), Ordering::SeqCst);
                    }
                    let _ = conn.write_all(&body[a..=z]);
                });
            }
        });
        let policy = Policy {
            segment: SEG as u64,
            parallel: 6,
            stall: Duration::from_secs(60),
            hedge_min: Duration::from_millis(300),
            max_attempts: 6,
        };
        let client = crate::segment_client().unwrap();
        let mut out = Collect(Vec::new());
        download(&client, &url, &HeaderMap::new(), body.len() as u64, policy, &mut out)
            .await
            .unwrap();
        assert!(out.0 == *body);
        assert!(first.load(Ordering::SeqCst) >= 2, "segment 0 was not duplicated");
        assert!(
            closed_in_time.load(Ordering::SeqCst),
            "the stalled request was not cancelled"
        );
    }
}
