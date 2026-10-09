use acropolis_store::{Integrity, Store, StoredBlob};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc};

pub mod segmented;

const MAX_ATTEMPTS: u32 = 5;
const STALL: Duration = Duration::from_secs(6);
const MAX_BUFFERED: u64 = 512 << 20;
const HEDGE_AFTER: Duration = Duration::from_millis(1500);

enum Probe {
    Small(Bytes),
    Large { len: u64, url: String },
    Stream(Response),
}

#[derive(Debug)]
struct Fatal;

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request failed permanently")
    }
}

impl std::error::Error for Fatal {}

#[derive(Debug)]
struct Retryable;

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request failed, retrying")
    }
}

impl std::error::Error for Retryable {}

#[derive(Clone)]
pub struct Fetcher {
    client: Client,
    seg_client: Client,
    store: Arc<Store>,
    limit: Arc<Semaphore>,
    inflight: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

/// Installs rustls' ring provider as the process default (reqwest is built with `rustls-no-provider`, so
/// building a `reqwest::Client` before this panics). Idempotent; call it before every `Client::builder()`.
pub fn ensure_tls() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err only if another provider is already installed, which serves as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn segment_client() -> Result<Client> {
    ensure_tls();
    Ok(Client::builder()
        .user_agent(concat!("acropolis/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect_policy())
        .http1_only()
        .pool_max_idle_per_host(16)
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .build()?)
}

impl Fetcher {
    pub fn new(store: Arc<Store>, concurrency: usize) -> Result<Self> {
        ensure_tls();
        let client = Client::builder()
            .user_agent(concat!("acropolis/", env!("CARGO_PKG_VERSION")))
            .redirect(redirect_policy())
            .pool_max_idle_per_host(64)
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()?;
        Ok(Fetcher {
            client,
            seg_client: segment_client()?,
            store,
            limit: Arc::new(Semaphore::new(concurrency)),
            inflight: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn segment_client(&self) -> &Client {
        &self.seg_client
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub async fn bytes(&self, url: &str) -> Result<Bytes> {
        self.bytes_with(url, &HeaderMap::new()).await
    }

    pub async fn bytes_with(&self, url: &str, headers: &HeaderMap) -> Result<Bytes> {
        let _permit = self.limit.acquire().await?;
        let start = Instant::now();
        let mut out = bytes::BytesMut::new();
        struct Collect<'a>(&'a mut bytes::BytesMut);
        impl AsyncSink for Collect<'_> {
            fn reserve(&mut self, len: u64) {
                if len <= MAX_BUFFERED {
                    self.0.reserve(len as usize);
                }
            }
            async fn push(&mut self, b: Bytes) -> Result<()> {
                if (self.0.len() + b.len()) as u64 > MAX_BUFFERED {
                    bail!("response is larger than {} MB", MAX_BUFFERED >> 20);
                }
                self.0.extend_from_slice(&b);
                Ok(())
            }
        }
        let n = self.stream_chunks(url, headers, Collect(&mut out)).await?;
        acropolis_events::emit(acropolis_events::Event::Downloaded {
            what: url.to_string(),
            bytes: n,
            ms: start.elapsed().as_millis() as u64,
        });
        Ok(out.freeze())
    }

    pub async fn json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));
        let b = self.bytes_with(url, &h).await?;
        serde_json::from_slice(&b).with_context(|| format!("parsing JSON from {url}"))
    }

    async fn attempt(&self, client: &Client, url: &str, headers: &HeaderMap, segment: bool) -> Result<Probe> {
        acropolis_events::add_request();
        let r = match tokio::time::timeout(STALL * 2, client.get(url).headers(headers.clone()).send()).await {
            Err(_) => return Err(anyhow!(Stalled)),
            Ok(r) => r?,
        };
        let status = r.status();
        if !status.is_success() {
            let retry = retryable(status);
            let mut r = r;
            let mut raw = Vec::new();
            while raw.len() < 4096 {
                match tokio::time::timeout(STALL, r.chunk()).await {
                    Ok(Ok(Some(c))) => raw.extend_from_slice(&c),
                    _ => break,
                }
            }
            let body = String::from_utf8_lossy(&raw).into_owned();
            let err = anyhow!("GET {url}: {status} {}", body.chars().take(300).collect::<String>());
            return Err(if retry {
                err.context(Retryable)
            } else {
                err.context(Fatal)
            });
        }
        let len = r.content_length().unwrap_or(0);
        let ranges = r
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .map(|v| v.as_bytes() == b"bytes")
            .unwrap_or(false);
        if len >= segmented::MIN_SEGMENTED {
            if ranges && segment {
                return Ok(Probe::Large {
                    len,
                    url: r.url().to_string(),
                });
            }
            return Ok(Probe::Stream(r));
        }
        let mut r = r;
        let mut buf = bytes::BytesMut::with_capacity(len as usize);
        loop {
            let chunk = match tokio::time::timeout(STALL, r.chunk()).await {
                Ok(c) => c?,
                Err(_) => return Err(anyhow!(Stalled)),
            };
            let Some(chunk) = chunk else { break };
            acropolis_events::add_downloaded(chunk.len() as u64);
            buf.extend_from_slice(&chunk);
            if buf.len() as u64 > MAX_BUFFERED {
                return Err(anyhow!(
                    "GET {url}: response without a length grew past {} MB",
                    MAX_BUFFERED >> 20
                )
                .context(Fatal));
            }
        }
        if len > 0 && buf.len() as u64 != len {
            return Err(anyhow!("short body: {} of {len} bytes", buf.len()).context(Retryable));
        }
        Ok(Probe::Small(buf.freeze()))
    }

    async fn race(&self, url: &str, headers: &HeaderMap, segment: bool) -> Result<Probe> {
        let primary = self.attempt(&self.client, url, headers, segment);
        let hedge = async {
            tokio::time::sleep(HEDGE_AFTER).await;
            self.attempt(&self.seg_client, url, headers, segment).await
        };
        tokio::pin!(primary);
        tokio::pin!(hedge);
        let (mut p_done, mut h_done) = (false, false);
        let mut last: Option<anyhow::Error>;
        loop {
            tokio::select! {
                r = &mut primary, if !p_done => match r {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        if e.downcast_ref::<Fatal>().is_some() {
                            return Err(e);
                        }
                        p_done = true;
                        last = Some(e);
                        if h_done { break; }
                    }
                },
                r = &mut hedge, if !h_done => match r {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        if e.downcast_ref::<Fatal>().is_some() {
                            return Err(e);
                        }
                        h_done = true;
                        last = Some(e);
                        if p_done { break; }
                    }
                },
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("request failed")))
    }

    async fn stream_chunks<F>(&self, url: &str, headers: &HeaderMap, mut sink: F) -> Result<u64>
    where
        F: AsyncSink,
    {
        let mut failures = 0;
        let mut segment = true;
        loop {
            let probe = match self.race(url, headers, segment).await {
                Ok(p) => p,
                Err(e) if e.downcast_ref::<Fatal>().is_none() && failures < MAX_ATTEMPTS => {
                    failures += 1;
                    tokio::time::sleep(backoff(failures)).await;
                    continue;
                }
                Err(e) => return Err(e.context(format!("GET {url}"))),
            };
            match probe {
                Probe::Small(b) => {
                    let n = b.len() as u64;
                    if n > 0 {
                        sink.push(b).await?;
                    }
                    return Ok(n);
                }
                Probe::Large { len, url: final_url } => {
                    let seg_headers = if final_url == url {
                        headers.clone()
                    } else {
                        HeaderMap::new()
                    };
                    sink.reserve(len);
                    match segmented::download(
                        &self.seg_client,
                        &final_url,
                        &seg_headers,
                        len,
                        segmented::Policy::default(),
                        &mut sink,
                    )
                    .await
                    {
                        Err(e) if e.downcast_ref::<segmented::NoRanges>().is_some() => {
                            segment = false;
                            continue;
                        }
                        r => return r.with_context(|| format!("GET {url}")),
                    }
                }
                Probe::Stream(mut r) => {
                    sink.reserve(r.content_length().unwrap_or(0));
                    let mut offset = 0u64;
                    loop {
                        let chunk = match tokio::time::timeout(STALL * 3, r.chunk()).await {
                            Ok(c) => c.with_context(|| format!("GET {url}"))?,
                            Err(_) => {
                                bail!("GET {url}: stalled after {offset} bytes and the server does not support ranges")
                            }
                        };
                        let Some(chunk) = chunk else { break };
                        acropolis_events::add_downloaded(chunk.len() as u64);
                        offset += chunk.len() as u64;
                        sink.push(chunk).await?;
                    }
                    return Ok(offset);
                }
            }
        }
    }

    pub async fn blob(&self, what: &str, url: &str, expected: Option<Integrity>) -> Result<StoredBlob> {
        self.blob_with(what, url, &HeaderMap::new(), expected).await
    }

    pub async fn blob_with(
        &self,
        what: &str,
        url: &str,
        headers: &HeaderMap,
        expected: Option<Integrity>,
    ) -> Result<StoredBlob> {
        if let Some(exp) = &expected
            && let Some(b) = self.store.get(exp)
        {
            return Ok(b);
        }
        let key = expected.as_ref().map(|e| e.to_sri()).unwrap_or_else(|| url.to_string());
        let lock = self.inflight.lock().unwrap().entry(key.clone()).or_default().clone();
        let _guard = lock.lock().await;
        if let Some(exp) = &expected
            && let Some(b) = self.store.get(exp)
        {
            return Ok(b);
        }
        let _permit = self.limit.acquire().await?;
        let start = Instant::now();
        let mut w = self.store.writer(what, expected)?;
        struct FileSink<'a, 'b>(&'a mut acropolis_store::BlobWriter<'b>);
        impl AsyncSink for FileSink<'_, '_> {
            async fn push(&mut self, b: Bytes) -> Result<()> {
                self.0.write_all(&b)?;
                Ok(())
            }
        }
        let n = self.stream_chunks(url, headers, FileSink(&mut w)).await?;
        let blob = w.commit()?;
        acropolis_events::emit(acropolis_events::Event::Downloaded {
            what: what.to_string(),
            bytes: n,
            ms: start.elapsed().as_millis() as u64,
        });
        Ok(blob)
    }

    pub async fn blob_streaming<T, F>(
        &self,
        what: &str,
        url: &str,
        headers: &HeaderMap,
        expected: Option<Integrity>,
        consumer: F,
    ) -> Result<(T, StoredBlob)>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn Read) -> Result<T> + Send + 'static,
    {
        if let Some(exp) = &expected
            && let Some(b) = self.store.get(exp)
        {
            let path = b.path.clone();
            let t = tokio::task::spawn_blocking(move || -> Result<T> {
                let mut f = std::io::BufReader::with_capacity(256 * 1024, std::fs::File::open(path)?);
                consumer(&mut f)
            })
            .await??;
            return Ok((t, b));
        }
        let _permit = self.limit.acquire().await?;
        let start = Instant::now();
        let (tx, rx) = mpsc::channel::<Bytes>(256);
        let handle = tokio::task::spawn_blocking(move || -> Result<T> {
            let mut reader = ChannelReader { rx, cur: Bytes::new() };
            let out = consumer(&mut reader)?;
            let mut sink = [0u8; 64 * 1024];
            while reader.read(&mut sink)? > 0 {}
            Ok(out)
        });
        let mut w = self.store.writer(what, expected)?;
        struct TeeSink<'a, 'b> {
            file: &'a mut acropolis_store::BlobWriter<'b>,
            tx: Option<mpsc::Sender<Bytes>>,
        }
        impl AsyncSink for TeeSink<'_, '_> {
            async fn push(&mut self, b: Bytes) -> Result<()> {
                self.file.write_all(&b)?;
                if let Some(tx) = &self.tx
                    && tx.send(b).await.is_err()
                {
                    self.tx = None;
                }
                Ok(())
            }
        }
        let mut sink = TeeSink {
            file: &mut w,
            tx: Some(tx),
        };
        let res = self.stream_chunks(url, headers, &mut sink).await;
        drop(sink);
        let n = match res {
            Ok(n) => n,
            Err(e) => {
                drop(w);
                let _ = handle.await;
                return Err(e);
            }
        };
        let blob = w.commit();
        let out = handle.await?;
        let blob = blob?;
        let out = out.with_context(|| format!("processing {what}"))?;
        acropolis_events::emit(acropolis_events::Event::Downloaded {
            what: what.to_string(),
            bytes: n,
            ms: start.elapsed().as_millis() as u64,
        });
        Ok((out, blob))
    }
}

#[derive(Debug)]
pub struct Stalled;

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("download stalled")
    }
}

impl std::error::Error for Stalled {}

pub trait AsyncSink {
    fn push(&mut self, b: Bytes) -> impl Future<Output = Result<()>>;

    /// Hint that the whole body is `len` bytes, so buffering sinks can allocate once.
    fn reserve(&mut self, _len: u64) {}
}

impl<S: AsyncSink> AsyncSink for &mut S {
    async fn push(&mut self, b: Bytes) -> Result<()> {
        (**self).push(b).await
    }

    fn reserve(&mut self, len: u64) {
        (**self).reserve(len)
    }
}

/// Loopback, private, link-local, CGNAT (100.64/10), unique-local, unspecified, and IPv6 forms embedding such
/// an IPv4 address (mapped, compatible, NAT64 64:ff9b::/96).
pub fn private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || o[0] == 0 || o[0] == 100 && (o[1] & 0xc0) == 64
        }
        std::net::IpAddr::V6(ip) => {
            let s = ip.segments();
            let embedded = if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                Some(std::net::Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])))
            } else {
                ip.to_ipv4()
            };
            ip.is_loopback()
                || ip.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || embedded.is_some_and(|v4| private_ip(v4.into()))
        }
    }
}

/// `private_ip` literals (bracketed IPv6 accepted) and names that resolve locally by convention.
pub fn private_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h.ends_with(".internal") {
        return true;
    }
    h.parse::<std::net::IpAddr>().is_ok_and(private_ip)
}

const MAX_REDIRECTS: usize = 10;

/// Redirects come from servers named by the (untrusted) app, so they may not hop onto private hosts or downgrade
/// https to http. A request that already targeted a private host (operator mirror) is not restricted.
fn redirect_error(previous: &[reqwest::Url], next: &reqwest::Url) -> Option<&'static str> {
    if previous.len() >= MAX_REDIRECTS {
        return Some("too many redirects");
    }
    if previous
        .first()
        .is_some_and(|u| private_host(u.host_str().unwrap_or("")))
    {
        return None;
    }
    if private_host(next.host_str().unwrap_or("")) {
        return Some("redirect to a private address");
    }
    if next.scheme() == "http" && previous.iter().any(|u| u.scheme() == "https") {
        return Some("redirect from https to plain http");
    }
    None
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| match redirect_error(attempt.previous(), attempt.url()) {
        Some(e) => attempt.error(e),
        None => attempt.follow(),
    })
}

pub struct ChannelReader {
    rx: mpsc::Receiver<Bytes>,
    cur: Bytes,
}

impl ChannelReader {
    pub fn new(rx: mpsc::Receiver<Bytes>) -> Self {
        ChannelReader { rx, cur: Bytes::new() }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                Some(b) => self.cur = b,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.cur.len());
        buf[..n].copy_from_slice(&self.cur[..n]);
        self.cur = self.cur.slice(n..);
        Ok(n)
    }
}

fn retryable(s: StatusCode) -> bool {
    s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() || s == StatusCode::REQUEST_TIMEOUT
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(200u64 * (1 << attempt.min(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves `body` with `Accept-Ranges: bytes`. Range requests get a 200 with the whole body,
    /// or with `misplaced` a 206 whose bytes and `Content-Range` start at offset 0.
    fn serve(body: Vec<u8>, misplaced: bool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(body);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut req = Vec::new();
                    let mut b = [0u8; 1];
                    while !req.ends_with(b"\r\n\r\n") {
                        if conn.read(&mut b).unwrap_or(0) == 0 {
                            return;
                        }
                        req.push(b[0]);
                    }
                    let req = String::from_utf8_lossy(&req).to_ascii_lowercase();
                    let range = req.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| {
                        let (a, b) = r.trim().split_once('-')?;
                        Some(b.parse::<usize>().ok()? - a.parse::<usize>().ok()? + 1)
                    });
                    let (status, part, extra) = match range {
                        Some(n) if misplaced => (
                            "206 Partial Content",
                            &body[..n],
                            format!("Content-Range: bytes 0-{}/{}\r\n", n - 1, body.len()),
                        ),
                        _ => ("200 OK", &body[..], String::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{extra}Connection: close\r\n\r\n",
                        part.len()
                    );
                    let _ = conn.write_all(head.as_bytes());
                    let _ = conn.write_all(part);
                });
            }
        });
        format!("http://{addr}/blob")
    }

    fn fetcher(dir: &std::path::Path) -> Fetcher {
        Fetcher::new(Arc::new(Store::open(dir).unwrap()), 4).unwrap()
    }

    fn large_body() -> Vec<u8> {
        (0..segmented::MIN_SEGMENTED + 12345).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_ignoring_range_falls_back_to_one_stream() {
        let dir = tempfile::tempdir().unwrap();
        let body = large_body();
        let url = serve(body.clone(), false);
        let got = fetcher(dir.path()).bytes(&url).await.unwrap();
        assert!(got[..] == body[..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn range_answered_for_another_offset_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(large_body(), true);
        assert!(fetcher(dir.path()).bytes(&url).await.is_err());
    }

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "::",
            "::1",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::10.0.0.1",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!private_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(private_host("[::1]") && private_host("LOCALHOST") && private_host("metadata.google.internal"));
        assert!(!private_host("registry.npmjs.org"));
    }

    #[test]
    fn redirect_decisions() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        let public = [u("https://registry.npmjs.org/a.tgz")];
        assert_eq!(redirect_error(&public, &u("https://cdn.example.com/a.tgz")), None);
        for next in [
            "https://127.0.0.1/a",
            "https://[::ffff:10.0.0.1]/a",
            "http://169.254.169.254/latest/meta-data",
            "https://localhost/a",
        ] {
            assert_eq!(
                redirect_error(&public, &u(next)),
                Some("redirect to a private address"),
                "{next}"
            );
        }
        assert_eq!(
            redirect_error(&public, &u("http://cdn.example.com/a.tgz")),
            Some("redirect from https to plain http")
        );
        assert_eq!(
            redirect_error(&[u("http://example.com/a")], &u("http://cdn.example.com/a")),
            None
        );
        // An operator mirror on a private address may redirect anywhere.
        let mirror = [u("https://10.0.0.5/npm/a.tgz")];
        assert_eq!(redirect_error(&mirror, &u("http://10.0.0.6/a.tgz")), None);
        let chain: Vec<_> = (0..MAX_REDIRECTS)
            .map(|i| u(&format!("https://h{i}.example.com/")))
            .collect();
        assert_eq!(
            redirect_error(&chain, &u("https://cdn.example.com/")),
            Some("too many redirects")
        );
    }
}
