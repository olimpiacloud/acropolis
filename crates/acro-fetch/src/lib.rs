use acro_store::{Integrity, Store, StoredBlob};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc};

pub mod segmented;

const MAX_ATTEMPTS: u32 = 5;
const STALL: Duration = Duration::from_secs(6);

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

    async fn open(&self, url: &str, headers: &HeaderMap, offset: u64) -> Result<Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self.client.get(url).headers(headers.clone());
            if offset > 0 {
                req = req.header(RANGE, format!("bytes={offset}-"));
            }
            acro_events::add_request();
            let res = req.send().await;
            match res {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) if retryable(r.status()) && attempt < MAX_ATTEMPTS => {
                    let wait = retry_after(&r).unwrap_or_else(|| backoff(attempt));
                    tokio::time::sleep(wait).await;
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    bail!("GET {url}: {status} {}", body.chars().take(300).collect::<String>());
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let _ = e;
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(anyhow!(e).context(format!("GET {url}"))),
            }
        }
    }

    pub async fn bytes(&self, url: &str) -> Result<Bytes> {
        self.bytes_with(url, &HeaderMap::new()).await
    }

    pub async fn bytes_with(&self, url: &str, headers: &HeaderMap) -> Result<Bytes> {
        let _permit = self.limit.acquire().await?;
        let start = Instant::now();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let r = self.open(url, headers, 0).await?;
            match r.bytes().await {
                Ok(b) => {
                    acro_events::add_downloaded(b.len() as u64);
                    acro_events::emit(acro_events::Event::Downloaded {
                        what: url.to_string(),
                        bytes: b.len() as u64,
                        ms: start.elapsed().as_millis() as u64,
                    });
                    return Ok(b);
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let _ = e;
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(anyhow!(e).context(format!("GET {url}"))),
            }
        }
    }

    pub async fn json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));
        let b = self.bytes_with(url, &h).await?;
        serde_json::from_slice(&b).with_context(|| format!("parsing JSON from {url}"))
    }

    async fn stream_chunks<F>(&self, url: &str, headers: &HeaderMap, mut sink: F) -> Result<u64>
    where
        F: AsyncSink,
    {
        let mut offset = 0u64;
        let mut failures = 0;
        loop {
            let mut r = self.open(url, headers, offset).await?;
            if offset == 0 {
                let len = r.content_length().unwrap_or(0);
                let ranges = r
                    .headers()
                    .get(reqwest::header::ACCEPT_RANGES)
                    .map(|v| v.as_bytes() == b"bytes")
                    .unwrap_or(false);
                if len >= segmented::MIN_SEGMENTED && ranges {
                    let final_url = r.url().to_string();
                    let seg_headers = if final_url == url { headers.clone() } else { HeaderMap::new() };
                    drop(r);
                    match segmented::download(&self.seg_client, &final_url, &seg_headers, len, segmented::Policy::default(), &mut sink).await {
                        Ok(n) => return Ok(n),
                        Err(e) if e.downcast_ref::<segmented::NoRanges>().is_some() => {
                            r = self.open(url, headers, 0).await?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            let mut skip = 0u64;
            if offset > 0 && r.status() != StatusCode::PARTIAL_CONTENT {
                skip = offset;
            }
            let result: Result<()> = async {
                loop {
                    let chunk = match tokio::time::timeout(STALL, r.chunk()).await {
                        Ok(c) => c?,
                        Err(_) => return Err(anyhow!(Stalled)),
                    };
                    let Some(chunk) = chunk else { break };
                    acro_events::add_downloaded(chunk.len() as u64);
                    let chunk = if skip > 0 {
                        let s = (skip as usize).min(chunk.len());
                        skip -= s as u64;
                        chunk.slice(s..)
                    } else {
                        chunk
                    };
                    if chunk.is_empty() {
                        continue;
                    }
                    offset += chunk.len() as u64;
                    sink.push(chunk).await?;
                }
                Ok(())
            }
            .await;
            match result {
                Ok(()) => return Ok(offset),
                Err(e)
                    if (e.downcast_ref::<reqwest::Error>().is_some() || e.downcast_ref::<Stalled>().is_some())
                        && failures < MAX_ATTEMPTS =>
                {
                    failures += 1;
                    tokio::time::sleep(backoff(failures)).await;
                }
                Err(e) => return Err(e.context(format!("GET {url}"))),
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

fn retry_after(r: &Response) -> Option<Duration> {
    let v = r.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = v.trim().parse().ok()?;
    Some(Duration::from_secs(secs.min(30)))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(200u64 * (1 << attempt.min(5)))
}
