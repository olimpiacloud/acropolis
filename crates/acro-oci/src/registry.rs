use crate::image::{self, ACCEPT_MANIFESTS, Descriptor, Index, Manifest, Platform};
use crate::reference::Reference;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LOCATION, WWW_AUTHENTICATE};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone)]
struct Basic {
    user: String,
    pass: String,
}

pub struct Registry {
    client: Client,
    noredirect: Client,
    seg: Client,
    tokens: Mutex<HashMap<(String, String), String>>,
    creds: HashMap<String, Basic>,
    mirrors: HashMap<String, String>,
    hub_down: std::sync::atomic::AtomicBool,
}

#[derive(Debug)]
pub struct Unreachable(pub String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is unreachable", self.0)
    }
}

impl std::error::Error for Unreachable {}

pub const HUB_FALLBACK: &str = "mirror.gcr.io";

const HEDGE_TAG_MS: u64 = 2500;
const HEDGE_DIGEST_MS: u64 = 1200;
const SLOW_REQUEST_MS: u128 = 1500;

pub struct ResolvedImage {
    pub reference: Reference,
    pub manifest: Manifest,
    pub manifest_digest: String,
    pub config_raw: Bytes,
    pub config: serde_json::Value,
}

impl Registry {
    pub fn new(client: Client) -> Self {
        let noredirect = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("http client");
        let seg = acro_fetch::segment_client().expect("http client");
        Registry {
            client,
            noredirect,
            seg,
            tokens: Mutex::new(HashMap::new()),
            creds: load_docker_creds(),
            mirrors: HashMap::new(),
            hub_down: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_mirror(mut self, registry: &str, mirror: &str) -> Self {
        self.mirrors.insert(registry.to_string(), mirror.to_string());
        self
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn host_for(&self, r: &Reference) -> String {
        if let Some(m) = self.mirrors.get(&r.registry) {
            return m.clone();
        }
        if r.registry == "docker.io" && self.hub_down.load(std::sync::atomic::Ordering::Relaxed) {
            return HUB_FALLBACK.to_string();
        }
        r.api_host().to_string()
    }

    fn mark_down(&self, host: &str) -> bool {
        if host == "registry-1.docker.io" && !self.hub_down.swap(true, std::sync::atomic::Ordering::Relaxed) {
            acro_events::log("registry", format!("registry-1.docker.io is unreachable or rate limited, falling back to {HUB_FALLBACK} (content is verified by digest)"));
            return true;
        }
        host == "registry-1.docker.io"
    }

    fn hedge_target(&self, r: &Reference) -> Option<Reference> {
        if r.registry != "docker.io" || self.mirrors.contains_key("docker.io") || self.hub_down.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        Some(Reference { registry: HUB_FALLBACK.to_string(), ..r.clone() })
    }

    async fn hedged<T, P, S, F>(&self, alt: Option<Reference>, delay_ms: u64, what: &str, primary: P, secondary: F) -> Result<T>
    where
        P: std::future::Future<Output = Result<T>>,
        S: std::future::Future<Output = Result<T>>,
        F: FnOnce(Reference) -> S,
    {
        tokio::pin!(primary);
        let Some(alt) = alt else { return primary.await };
        tokio::select! {
            res = &mut primary => return res,
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
        }
        acro_events::log("registry", format!("{what} from docker.io is slow after {delay_ms} ms, racing {HUB_FALLBACK}"));
        let secondary = secondary(alt);
        tokio::pin!(secondary);
        tokio::select! {
            res = &mut primary => match res {
                Ok(v) => Ok(v),
                Err(_) => secondary.await,
            },
            res = &mut secondary => match res {
                Ok(v) => {
                    acro_events::log("registry", format!("{what} served by {HUB_FALLBACK}"));
                    Ok(v)
                }
                Err(_) => primary.await,
            },
        }
    }

    fn base(&self, r: &Reference) -> String {
        let host = self.host_for(r);
        let scheme = if r.insecure() { "http" } else { "https" };
        format!("{scheme}://{host}")
    }

    fn cred_for(&self, r: &Reference) -> Option<Basic> {
        let keys: Vec<String> = if r.registry == "docker.io" {
            vec![
                "https://index.docker.io/v1/".into(),
                "index.docker.io".into(),
                "docker.io".into(),
                "registry-1.docker.io".into(),
            ]
        } else {
            vec![r.registry.clone(), format!("https://{}", r.registry), format!("http://{}", r.registry)]
        };
        keys.iter().find_map(|k| self.creds.get(k).cloned())
    }

    async fn send<F>(&self, r: &Reference, actions: &str, build: F) -> Result<Response>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        self.send_with(&self.client, r, actions, build).await
    }

    async fn send_with<F>(&self, client: &Client, r: &Reference, actions: &str, build: F) -> Result<Response>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        let key = (self.host_for(r), format!("repository:{}:{}", r.repository, actions));
        let mut attempt = 0;
        let mut authed = false;
        loop {
            attempt += 1;
            let mut token = self.tokens.lock().unwrap().get(&key).cloned();
            if token.is_none()
                && !authed
                && let Some(challenge) = known_challenge(&key.0)
            {
                let header = match self.authenticate(r, challenge, &key.1).await {
                    Ok(h) => h,
                    Err(e) => {
                        let net = e.downcast_ref::<reqwest::Error>().map(|re| re.is_connect() || re.is_timeout()).unwrap_or(false);
                        if net && self.mark_down(&key.0) {
                            return Err(anyhow!(Unreachable(key.0.clone())));
                        }
                        if net && key.0 == "registry-1.docker.io" {
                            return Err(anyhow!(Unreachable(key.0.clone())));
                        }
                        return Err(e);
                    }
                };
                self.tokens.lock().unwrap().insert(key.clone(), header.clone());
                token = Some(header);
                authed = true;
            }
            let mut req = build(client);
            if let Some(t) = &token {
                req = req.header(AUTHORIZATION, t.clone());
            }
            acro_events::add_request();
            let started = std::time::Instant::now();
            let res = req.send().await;
            let elapsed = started.elapsed().as_millis();
            if elapsed > SLOW_REQUEST_MS {
                acro_events::log("registry", format!("slow request to {} ({}): {elapsed} ms", key.0, key.1));
            }
            match res {
                Ok(resp) if resp.status() == StatusCode::UNAUTHORIZED && !authed => {
                    let challenge = resp
                        .headers()
                        .get(WWW_AUTHENTICATE)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let header = self.authenticate(r, &challenge, &key.1).await?;
                    self.tokens.lock().unwrap().insert(key.clone(), header);
                    authed = true;
                }
                Ok(resp) if resp.status() == StatusCode::TOO_MANY_REQUESTS && key.0 == "registry-1.docker.io" => {
                    self.mark_down(&key.0);
                    return Err(anyhow!(Unreachable(key.0.clone())));
                }
                Ok(resp)
                    if (resp.status().is_server_error() || resp.status() == StatusCode::TOO_MANY_REQUESTS)
                        && attempt < 4 =>
                {
                    tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                }
                Ok(resp) => return Ok(resp),
                Err(e) if (e.is_connect() || e.is_timeout()) && self.mark_down(&key.0) => {
                    return Err(anyhow!(Unreachable(key.0.clone())));
                }
                Err(e) if attempt < 4 && (e.is_connect() || e.is_timeout()) => {
                    tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
                }
                Err(e) => return Err(anyhow!(e)),
            }
        }
    }

    async fn authenticate(&self, r: &Reference, challenge: &str, scope: &str) -> Result<String> {
        let basic = self.cred_for(r);
        let lower = challenge.to_ascii_lowercase();
        if lower.starts_with("basic") {
            let b = basic.ok_or_else(|| anyhow!("{} requires credentials", r.registry))?;
            let enc = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", b.user, b.pass));
            return Ok(format!("Basic {enc}"));
        }
        let params = parse_challenge(challenge);
        let realm = params.get("realm").ok_or_else(|| anyhow!("no realm in challenge {challenge:?}"))?;
        let mut url = url::Url::parse(realm)?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(s) = params.get("service") {
                q.append_pair("service", s);
            }
            q.append_pair("scope", scope);
        }
        let mut req = self.client.get(url.as_str());
        if let Some(b) = basic {
            req = req.basic_auth(b.user, Some(b.pass));
        }
        acro_events::add_request();
        let resp = req.send().await?;
        if !resp.status().is_success() {
            bail!("token request to {realm} failed: {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        let token = v
            .get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("no token in response from {realm}"))?;
        Ok(format!("Bearer {token}"))
    }

    pub async fn get_manifest(&self, r: &Reference, reference: &str) -> Result<(Bytes, String, String)> {
        let delay = if reference.starts_with("sha256:") { HEDGE_DIGEST_MS } else { HEDGE_TAG_MS };
        let what = format!("manifest {}:{reference}", r.repository);
        let alt = self.hedge_target(r);
        self.hedged(alt, delay, &what, self.get_manifest_retry(r, reference), |rr| async move { self.get_manifest_retry(&rr, reference).await })
            .await
    }

    async fn get_manifest_retry(&self, r: &Reference, reference: &str) -> Result<(Bytes, String, String)> {
        match self.get_manifest_once(r, reference).await {
            Err(e) if e.downcast_ref::<Unreachable>().is_some() => self.get_manifest_once(r, reference).await,
            other => other,
        }
    }

    async fn get_manifest_once(&self, r: &Reference, reference: &str) -> Result<(Bytes, String, String)> {
        let url = format!("{}/v2/{}/manifests/{}", self.base(r), r.repository, reference);
        let resp = self
            .send(r, "pull", |c| c.get(&url).header(ACCEPT, ACCEPT_MANIFESTS))
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("GET manifest {r}: {status} {}", body.chars().take(200).collect::<String>());
        }
        let mt = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let body = resp.bytes().await?;
        acro_events::add_downloaded(body.len() as u64);
        let digest = acro_store::sha256_bytes(&body).to_oci();
        if reference.starts_with("sha256:") && digest != reference {
            bail!("manifest digest mismatch for {r}: expected {reference}, got {digest}");
        }
        let mt = if mt.is_empty() || mt == "application/json" || mt == "text/plain" {
            serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("mediaType").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| image::MT_OCI_MANIFEST.to_string())
        } else {
            mt
        };
        Ok((body, mt, digest))
    }

    pub async fn resolve(&self, r: &Reference, platform: &Platform) -> Result<ResolvedImage> {
        let (reference, manifest, digest) = self.resolve_manifest(r, platform).await?;
        self.with_config(reference, manifest, digest).await
    }

    pub async fn with_config(&self, reference: Reference, manifest: Manifest, digest: String) -> Result<ResolvedImage> {
        let config_raw = self.get_blob_bytes(&reference, &manifest.config.digest).await?;
        let config: serde_json::Value = serde_json::from_slice(&config_raw).context("parsing image config")?;
        Ok(ResolvedImage { reference, manifest, manifest_digest: digest, config_raw, config })
    }

    pub async fn resolve_manifest(&self, r: &Reference, platform: &Platform) -> Result<(Reference, Manifest, String)> {
        let first = r.reference().to_string();
        let (body, mt, digest) = self.get_manifest(r, &first).await?;
        let (body, digest) = if mt == image::MT_OCI_INDEX || mt == image::MT_DOCKER_LIST {
            let idx: Index = serde_json::from_slice(&body).context("parsing image index")?;
            let chosen = select_platform(&idx.manifests, platform)
                .ok_or_else(|| anyhow!("{r} has no manifest for {}/{}", platform.os, platform.architecture))?;
            let (b, _, d) = self.get_manifest(r, &chosen.digest).await?;
            (b, d)
        } else {
            (body, digest)
        };
        let manifest: Manifest = serde_json::from_slice(&body).context("parsing image manifest")?;
        Ok((r.with_digest(&digest), manifest, digest))
    }

    pub async fn get_blob(&self, r: &Reference, digest: &str) -> Result<Response> {
        match self.get_blob_once(r, digest).await {
            Err(e) if e.downcast_ref::<Unreachable>().is_some() => self.get_blob_once(r, digest).await,
            other => other,
        }
    }

    async fn get_blob_once(&self, r: &Reference, digest: &str) -> Result<Response> {
        let url = format!("{}/v2/{}/blobs/{}", self.base(r), r.repository, digest);
        let resp = self.send(r, "pull", |c| c.get(&url)).await?;
        if !resp.status().is_success() {
            bail!("GET blob {digest} from {r}: {}", resp.status());
        }
        Ok(resp)
    }

    pub async fn get_blob_bytes(&self, r: &Reference, digest: &str) -> Result<Bytes> {
        let what = format!("blob {}@{}", r.repository, &digest[..digest.len().min(19)]);
        let alt = self.hedge_target(r);
        self.hedged(alt, HEDGE_DIGEST_MS, &what, self.get_blob_bytes_once(r, digest), |rr| async move { self.get_blob_bytes_once(&rr, digest).await })
            .await
    }

    async fn get_blob_bytes_once(&self, r: &Reference, digest: &str) -> Result<Bytes> {
        let resp = self.get_blob(r, digest).await?;
        let b = resp.bytes().await?;
        acro_events::add_downloaded(b.len() as u64);
        let actual = acro_store::sha256_bytes(&b).to_oci();
        if actual != digest {
            bail!("blob digest mismatch from {r}: expected {digest}, got {actual}");
        }
        Ok(b)
    }

    pub async fn blob_exists(&self, r: &Reference, digest: &str) -> Result<bool> {
        let url = format!("{}/v2/{}/blobs/{}", self.base(r), r.repository, digest);
        let resp = self.send(r, "pull,push", |c| c.head(&url)).await?;
        Ok(resp.status().is_success())
    }

    async fn start_upload(&self, r: &Reference, query: &str) -> Result<Response> {
        let url = format!("{}/v2/{}/blobs/uploads/{}", self.base(r), r.repository, query);
        self.send(r, "pull,push", |c| c.post(&url).header(CONTENT_LENGTH, "0")).await
    }

    fn location(&self, r: &Reference, resp: &Response) -> Result<String> {
        let loc = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("upload response without Location"))?;
        if loc.starts_with("http://") || loc.starts_with("https://") {
            Ok(loc.to_string())
        } else {
            Ok(format!("{}{}", self.base(r), loc))
        }
    }

    pub async fn mount_blob(&self, r: &Reference, digest: &str, from: &str) -> Result<bool> {
        let resp = self.start_upload(r, &format!("?mount={digest}&from={from}")).await?;
        Ok(resp.status() == StatusCode::CREATED)
    }

    async fn put_upload(&self, r: &Reference, digest: &str, size: u64, body: impl Fn() -> reqwest::Body) -> Result<()> {
        let resp = self.start_upload(r, "").await?;
        if resp.status() != StatusCode::ACCEPTED && !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("starting upload to {r}: {status} {text}");
        }
        let loc = self.location(r, &resp)?;
        let sep = if loc.contains('?') { '&' } else { '?' };
        let url = format!("{loc}{sep}digest={digest}");
        let resp = self
            .send(r, "pull,push", |c| {
                c.request(Method::PUT, &url)
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .header(CONTENT_LENGTH, size)
                    .body(body())
            })
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("uploading {digest} to {r}: {status} {}", text.chars().take(300).collect::<String>());
        }
        acro_events::add_uploaded(size);
        Ok(())
    }

    pub async fn push_blob_bytes(&self, r: &Reference, digest: &str, data: Bytes) -> Result<bool> {
        if self.blob_exists(r, digest).await? {
            return Ok(false);
        }
        let size = data.len() as u64;
        self.put_upload(r, digest, size, || reqwest::Body::from(data.clone())).await?;
        Ok(true)
    }

    pub async fn push_blob_file(&self, r: &Reference, digest: &str, size: u64, path: &Path) -> Result<bool> {
        if self.blob_exists(r, digest).await? {
            return Ok(false);
        }
        let path = path.to_path_buf();
        let file = tokio::fs::File::open(&path).await?;
        let cell = Mutex::new(Some(file));
        self.put_upload(r, digest, size, || {
            let f = cell.lock().unwrap().take();
            match f {
                Some(f) => reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(f, 256 * 1024)),
                None => {
                    let p = path.clone();
                    let s = futures::stream::once(async move { tokio::fs::read(p).await.map(Bytes::from) });
                    reqwest::Body::wrap_stream(s)
                }
            }
        })
        .await?;
        Ok(true)
    }

    pub async fn copy_blob(&self, src: &Reference, dst: &Reference, desc: &Descriptor) -> Result<bool> {
        if self.blob_exists(dst, &desc.digest).await? {
            return Ok(false);
        }
        if src.registry == dst.registry && src.repository != dst.repository {
            if self.mount_blob(dst, &desc.digest, &src.repository).await.unwrap_or(false) {
                return Ok(false);
            }
        }
        if desc.size >= acro_fetch::segmented::MIN_SEGMENTED {
            match self.copy_blob_segmented(src, dst, desc).await {
                Ok(()) => return Ok(true),
                Err(e) if e.downcast_ref::<acro_fetch::segmented::NoRanges>().is_some() => {}
                Err(e) => return Err(e),
            }
        }
        let resp = self.get_blob(src, &desc.digest).await?;
        let cell = Mutex::new(Some(resp));
        let digest = desc.digest.clone();
        self.put_upload(dst, &desc.digest, desc.size, || {
            let r = cell.lock().unwrap().take();
            match r {
                Some(r) => {
                    let s = r.bytes_stream().map(|c| {
                        if let Ok(b) = &c {
                            acro_events::add_downloaded(b.len() as u64);
                        }
                        c
                    });
                    reqwest::Body::wrap_stream(s)
                }
                None => reqwest::Body::from(Vec::new()),
            }
        })
        .await
        .with_context(|| format!("copying {digest} from {src} to {dst}"))?;
        Ok(true)
    }

    pub async fn blob_location(&self, r: &Reference, digest: &str) -> Result<(String, reqwest::header::HeaderMap)> {
        match self.blob_location_once(r, digest).await {
            Err(e) if e.downcast_ref::<Unreachable>().is_some() => self.blob_location_once(r, digest).await,
            other => other,
        }
    }

    async fn blob_location_once(&self, r: &Reference, digest: &str) -> Result<(String, reqwest::header::HeaderMap)> {
        let url = format!("{}/v2/{}/blobs/{}", self.base(r), r.repository, digest);
        let resp = self.send_with(&self.noredirect, r, "pull", |c| c.get(&url)).await?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect without Location for {digest}"))?
                .to_string();
            let loc = if loc.starts_with("http") { loc } else { format!("{}{}", self.base(r), loc) };
            return Ok((loc, reqwest::header::HeaderMap::new()));
        }
        if !status.is_success() {
            bail!("GET blob {digest} from {r}: {status}");
        }
        drop(resp);
        let key = (self.host_for(r), format!("repository:{}:pull", r.repository));
        let mut h = reqwest::header::HeaderMap::new();
        if let Some(t) = self.tokens.lock().unwrap().get(&key) {
            h.insert(AUTHORIZATION, HeaderValue::from_str(t)?);
        }
        Ok((url, h))
    }

    async fn copy_blob_segmented(&self, src: &Reference, dst: &Reference, desc: &Descriptor) -> Result<()> {
        let (url, headers) = self.blob_location(src, &desc.digest).await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(8);
        struct Tx(tokio::sync::mpsc::Sender<Bytes>);
        impl acro_fetch::AsyncSink for Tx {
            async fn push(&mut self, b: Bytes) -> Result<()> {
                self.0.send(b).await.map_err(|_| anyhow!("upload side closed"))
            }
        }
        let size = desc.size;
        let seg = self.seg.clone();
        let download = tokio::spawn(async move {
            acro_fetch::segmented::download(&seg, &url, &headers, size, acro_fetch::segmented::Policy::default(), Tx(tx)).await
        });
        let cell = Mutex::new(Some(rx));
        let upload = self.put_upload(dst, &desc.digest, desc.size, || {
            let rx = cell.lock().unwrap().take();
            match rx {
                Some(rx) => {
                    let s = futures::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|b| (Ok::<Bytes, std::io::Error>(b), rx))
                    });
                    reqwest::Body::wrap_stream(s)
                }
                None => reqwest::Body::from(Vec::new()),
            }
        });
        let (up, down) = tokio::join!(upload, download);
        let n = down??;
        up.with_context(|| format!("copying {} from {src} to {dst}", desc.digest))?;
        if n != desc.size {
            bail!("copied {n} bytes of {} for {}", desc.size, desc.digest);
        }
        Ok(())
    }

    pub async fn put_manifest(&self, r: &Reference, reference: &str, body: Bytes, media_type: &str) -> Result<String> {
        let url = format!("{}/v2/{}/manifests/{}", self.base(r), r.repository, reference);
        let mt = HeaderValue::from_str(media_type)?;
        let resp = self
            .send(r, "pull,push", |c| c.put(&url).header(CONTENT_TYPE, mt.clone()).body(body.clone()))
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("PUT manifest {r}: {status} {}", text.chars().take(300).collect::<String>());
        }
        acro_events::add_uploaded(body.len() as u64);
        Ok(acro_store::sha256_bytes(&body).to_oci())
    }
}

fn known_challenge(host: &str) -> Option<&'static str> {
    match host {
        "registry-1.docker.io" => Some(r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#),
        "ghcr.io" => Some(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#),
        _ => None,
    }
}

pub fn select_platform<'a>(ms: &'a [Descriptor], p: &Platform) -> Option<&'a Descriptor> {
    let matching: Vec<&Descriptor> = ms
        .iter()
        .filter(|d| {
            d.platform
                .as_ref()
                .map(|q| q.os == p.os && q.architecture == p.architecture)
                .unwrap_or(false)
        })
        .collect();
    if let Some(v) = &p.variant {
        if let Some(d) = matching.iter().find(|d| d.platform.as_ref().and_then(|q| q.variant.as_ref()) == Some(v)) {
            return Some(d);
        }
    }
    matching.first().copied()
}

fn parse_challenge(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let rest = s.splitn(2, ' ').nth(1).unwrap_or("");
    let mut chars = rest.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(' ') | Some(',')) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if chars.next().is_none() {
            break;
        }
        let mut val = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                val.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                val.push(c);
                chars.next();
            }
        }
        out.insert(key.trim().to_ascii_lowercase(), val);
    }
    out
}

fn load_docker_creds() -> HashMap<String, Basic> {
    let mut out = HashMap::new();
    let dir = std::env::var("DOCKER_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".docker"));
    if let Ok(data) = std::fs::read(dir.join("config.json"))
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data)
        && let Some(auths) = v.get("auths").and_then(|a| a.as_object())
    {
        for (k, entry) in auths {
            if let Some(a) = entry.get("auth").and_then(|a| a.as_str())
                && let Ok(dec) = base64::engine::general_purpose::STANDARD.decode(a)
                && let Ok(s) = String::from_utf8(dec)
                && let Some((u, p)) = s.split_once(':')
            {
                out.insert(k.clone(), Basic { user: u.to_string(), pass: p.to_string() });
            }
        }
    }
    if let (Ok(host), Ok(user), Ok(pass)) = (
        std::env::var("ACRO_REGISTRY"),
        std::env::var("ACRO_REGISTRY_USER"),
        std::env::var("ACRO_REGISTRY_PASSWORD"),
    ) {
        out.insert(host, Basic { user, pass });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge() {
        let m = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/node:pull""#,
        );
        assert_eq!(m["realm"], "https://auth.docker.io/token");
        assert_eq!(m["service"], "registry.docker.io");
    }
}
