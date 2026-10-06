use crate::AsyncSink;
use anyhow::{Result, anyhow, bail};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use reqwest::header::{HeaderMap, RANGE};
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

async fn fetch_range(client: &Client, url: &str, headers: &HeaderMap, start: u64, end: u64, stall: Duration) -> Result<Bytes> {
    acro_events::add_request();
    let resp = tokio::time::timeout(
        stall * 3,
        client.get(url).headers(headers.clone()).header(RANGE, format!("bytes={start}-{end}")).send(),
    )
    .await
    .map_err(|_| anyhow!("timed out waiting for response"))??;
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        StatusCode::OK => return Err(anyhow!(NoRanges)),
        s => bail!("range request {start}-{end} failed: {s}"),
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
                acro_events::add_downloaded(chunk.len() as u64);
                buf.extend_from_slice(&chunk);
            }
        }
    }
    if buf.len() != want {
        bail!("short range response: {} of {want} bytes", buf.len());
    }
    Ok(buf.freeze())
}

pub async fn download<S: AsyncSink>(client: &Client, url: &str, headers: &HeaderMap, size: u64, policy: Policy, sink: S) -> Result<u64> {
    download_sources(client, &[(url.to_string(), headers.clone())], size, policy, sink).await
}

pub async fn download_sources<S: AsyncSink>(
    client: &Client,
    sources: &[(String, HeaderMap)],
    size: u64,
    policy: Policy,
    mut sink: S,
) -> Result<u64> {
    let url = sources.first().map(|s| s.0.as_str()).ok_or_else(|| anyhow!("no download source"))?;
    let n = size.div_ceil(policy.segment) as usize;
    let seg = |i: usize| -> (u64, u64) {
        let a = i as u64 * policy.segment;
        (a, (a + policy.segment).min(size) - 1)
    };
    let mut running: FuturesUnordered<_> = FuturesUnordered::new();
    let mut in_flight: HashMap<usize, (usize, Instant)> = HashMap::new();
    let mut attempts: HashMap<usize, u32> = HashMap::new();
    let mut done: BTreeMap<usize, Bytes> = BTreeMap::new();
    let mut durations: Vec<Duration> = Vec::new();
    let mut next_launch = 0usize;
    let mut next_emit = 0usize;
    let mut written = 0u64;
    let launch = |i: usize, running: &mut FuturesUnordered<_>, in_flight: &mut HashMap<usize, (usize, Instant)>, attempts: &mut HashMap<usize, u32>| {
        let (a, b) = seg(i);
        let c = client.clone();
        let attempt = attempts.get(&i).copied().unwrap_or(0) as usize;
        let (u, h) = sources[attempt % sources.len()].clone();
        let stall = policy.stall;
        let started = Instant::now();
        let e = in_flight.entry(i).or_insert((0, started));
        e.0 += 1;
        *attempts.entry(i).or_insert(0) += 1;
        running.push(async move { (i, started, fetch_range(&c, &u, &h, a, b, stall).await) });
    };
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    while next_emit < n {
        while next_launch < n && in_flight.len() < policy.parallel && done.len() < policy.parallel * 2 {
            if !done.contains_key(&next_launch) && !in_flight.contains_key(&next_launch) {
                launch(next_launch, &mut running, &mut in_flight, &mut attempts);
            }
            next_launch += 1;
        }
        tokio::select! {
            Some((i, started, res)) = running.next() => {
                match res {
                    Ok(bytes) => {
                        in_flight.remove(&i);
                        if i >= next_emit && !done.contains_key(&i) {
                            durations.push(started.elapsed());
                            done.insert(i, bytes);
                        }
                    }
                    Err(e) => {
                        if e.downcast_ref::<NoRanges>().is_some() {
                            return Err(e);
                        }
                        let entry = in_flight.get_mut(&i).map(|x| { x.0 -= 1; x.0 });
                        if entry == Some(0) {
                            in_flight.remove(&i);
                            if !done.contains_key(&i) && i >= next_emit {
                                if attempts.get(&i).copied().unwrap_or(0) >= policy.max_attempts {
                                    return Err(e.context(format!("segment {i} of {url}")));
                                }
                                launch(i, &mut running, &mut in_flight, &mut attempts);
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
                    if let Some((count, started)) = in_flight.get(&i).copied()
                        && count == 1
                        && started.elapsed() > threshold
                        && attempts.get(&i).copied().unwrap_or(0) < policy.max_attempts
                    {
                        launch(i, &mut running, &mut in_flight, &mut attempts);
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
