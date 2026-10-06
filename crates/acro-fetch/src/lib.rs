use acro_store::{Integrity, Store, StoredBlob};
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
}

pub fn segment_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("acropolis/", env!("CARGO_PKG_VERSION")))
        .http1_only()
        .pool_max_idle_per_host(16)
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .build()?)
}

impl Fetcher {
    pub fn new(store: Arc<Store>, concurrency: usize) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("acropolis/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(64)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()?;
        Ok(Fetcher { client, seg_client: segment_client()?, store, limit: Arc::new(Semaphore::new(concurrency)) })
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
            async fn push(&mut self, b: Bytes) -> Result<()> {
                self.0.extend_from_slice(&b);
                Ok(())
            }
        }
        let n = self.stream_chunks(url, headers, Collect(&mut out)).await?;
        acro_events::emit(acro_events::Event::Downloaded { what: url.to_string(), bytes: n, ms: start.elapsed().as_millis() as u64 });
        Ok(out.freeze())
    }

    pub async fn json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));
        let b = self.bytes_with(url, &h).await?;
        serde_json::from_slice(&b).with_context(|| format!("parsing JSON from {url}"))
    }

    async fn attempt(&self, client: &Client, url: &str, headers: &HeaderMap) -> Result<Probe> {
        acro_events::add_request();
        let r = match tokio::time::timeout(STALL * 2, client.get(url).headers(headers.clone()).send()).await {
            Err(_) => return Err(anyhow!(Stalled)),
            Ok(r) => r?,
        };
        let status = r.status();
        if !status.is_success() {
            let retry = retryable(status);
            let body = r.text().await.unwrap_or_default();
            let err = anyhow!("GET {url}: {status} {}", body.chars().take(300).collect::<String>());
            return Err(if retry { err.context(Retryable) } else { err.context(Fatal) });
        }
        let len = r.content_length().unwrap_or(0);
        let ranges = r.headers().get(reqwest::header::ACCEPT_RANGES).map(|v| v.as_bytes() == b"bytes").unwrap_or(false);
        if len >= segmented::MIN_SEGMENTED {
            if ranges {
                return Ok(Probe::Large { len, url: r.url().to_string() });
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
            acro_events::add_downloaded(chunk.len() as u64);
            buf.extend_from_slice(&chunk);
        }
        if len > 0 && buf.len() as u64 != len {
            return Err(anyhow!("short body: {} of {len} bytes", buf.len()).context(Retryable));
        }
        Ok(Probe::Small(buf.freeze()))
    }

    async fn race(&self, url: &str, headers: &HeaderMap) -> Result<Probe> {
        let primary = self.attempt(&self.client, url, headers);
        let hedge = async {
            tokio::time::sleep(HEDGE_AFTER).await;
            self.attempt(&self.seg_client, url, headers).await
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
        loop {
            let probe = match self.race(url, headers).await {
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
                    let seg_headers = if final_url == url { headers.clone() } else { HeaderMap::new() };
                    return segmented::download(&self.seg_client, &final_url, &seg_headers, len, segmented::Policy::default(), &mut sink)
                        .await
                        .with_context(|| format!("GET {url}"));
                }
                Probe::Stream(mut r) => {
                    let mut offset = 0u64;
                    loop {
                        let chunk = match tokio::time::timeout(STALL * 3, r.chunk()).await {
                            Ok(c) => c.with_context(|| format!("GET {url}"))?,
                            Err(_) => bail!("GET {url}: stalled after {offset} bytes and the server does not support ranges"),
                        };
                        let Some(chunk) = chunk else { break };
                        acro_events::add_downloaded(chunk.len() as u64);
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
        let _permit = self.limit.acquire().await?;
        let start = Instant::now();
        let mut w = self.store.writer(what, expected)?;
        struct FileSink<'a, 'b>(&'a mut acro_store::BlobWriter<'b>);
        impl AsyncSink for FileSink<'_, '_> {
            async fn push(&mut self, b: Bytes) -> Result<()> {
                self.0.write_all(&b)?;
                Ok(())
            }
        }
        let n = self.stream_chunks(url, headers, FileSink(&mut w)).await?;
        let blob = w.commit()?;
        acro_events::emit(acro_events::Event::Downloaded {
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
            file: &'a mut acro_store::BlobWriter<'b>,
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
        let mut sink = TeeSink { file: &mut w, tx: Some(tx) };
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
        acro_events::emit(acro_events::Event::Downloaded {
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
    fn push(&mut self, b: Bytes) -> impl std::future::Future<Output = Result<()>>;
}

impl<S: AsyncSink> AsyncSink for &mut S {
    async fn push(&mut self, b: Bytes) -> Result<()> {
        (**self).push(b).await
    }
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
