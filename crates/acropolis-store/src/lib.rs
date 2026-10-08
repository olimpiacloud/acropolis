use base64::Engine;
use sha2::Digest;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("integrity mismatch for {what}: expected {expected}, got {actual}")]
    IntegrityMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    #[error("invalid integrity string {0:?}")]
    InvalidIntegrity(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Algo {
    Sha1,
    Sha256,
    Sha512,
}

impl Algo {
    pub fn name(self) -> &'static str {
        match self {
            Algo::Sha1 => "sha1",
            Algo::Sha256 => "sha256",
            Algo::Sha512 => "sha512",
        }
    }

    fn len(self) -> usize {
        match self {
            Algo::Sha1 => 20,
            Algo::Sha256 => 32,
            Algo::Sha512 => 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Integrity {
    pub algo: Algo,
    pub digest: Vec<u8>,
}

impl Integrity {
    pub fn new(algo: Algo, digest: Vec<u8>) -> Self {
        Integrity { algo, digest }
    }

    pub fn sha256(digest: [u8; 32]) -> Self {
        Integrity {
            algo: Algo::Sha256,
            digest: digest.to_vec(),
        }
    }

    pub fn parse_sri(s: &str) -> Result<Self> {
        let mut best: Option<Integrity> = None;
        for part in s.split_whitespace() {
            let (algo, b64) = part.split_once('-').ok_or_else(|| Error::InvalidIntegrity(s.into()))?;
            let algo = match algo {
                "sha512" => Algo::Sha512,
                "sha256" => Algo::Sha256,
                "sha1" => Algo::Sha1,
                _ => continue,
            };
            let b64 = b64.split('?').next().unwrap_or("");
            let digest = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|_| Error::InvalidIntegrity(s.into()))?;
            if digest.len() != algo.len() {
                return Err(Error::InvalidIntegrity(s.into()));
            }
            let cand = Integrity { algo, digest };
            if best.as_ref().map(|b| cand.algo > b.algo).unwrap_or(true) {
                best = Some(cand);
            }
        }
        best.ok_or_else(|| Error::InvalidIntegrity(s.into()))
    }

    pub fn parse_hex(algo: Algo, s: &str) -> Result<Self> {
        let digest = hex::decode(s.trim()).map_err(|_| Error::InvalidIntegrity(s.into()))?;
        if digest.len() != algo.len() {
            return Err(Error::InvalidIntegrity(s.into()));
        }
        Ok(Integrity { algo, digest })
    }

    pub fn parse_oci(s: &str) -> Result<Self> {
        let (algo, hexs) = s.split_once(':').ok_or_else(|| Error::InvalidIntegrity(s.into()))?;
        let algo = match algo {
            "sha256" => Algo::Sha256,
            "sha512" => Algo::Sha512,
            _ => return Err(Error::InvalidIntegrity(s.into())),
        };
        Self::parse_hex(algo, hexs)
    }

    pub fn hex(&self) -> String {
        hex::encode(&self.digest)
    }

    pub fn to_oci(&self) -> String {
        format!("{}:{}", self.algo.name(), self.hex())
    }

    pub fn to_sri(&self) -> String {
        format!(
            "{}-{}",
            self.algo.name(),
            base64::engine::general_purpose::STANDARD.encode(&self.digest)
        )
    }
}

impl fmt::Display for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sri())
    }
}

#[derive(Clone)]
pub enum Hasher {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
}

impl Hasher {
    pub fn new(algo: Algo) -> Self {
        match algo {
            Algo::Sha1 => Hasher::Sha1(sha1::Sha1::new()),
            Algo::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            Algo::Sha512 => Hasher::Sha512(sha2::Sha512::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha1(h) => h.update(data),
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    pub fn finish(self) -> Integrity {
        match self {
            Hasher::Sha1(h) => Integrity::new(Algo::Sha1, h.finalize().to_vec()),
            Hasher::Sha256(h) => Integrity::new(Algo::Sha256, h.finalize().to_vec()),
            Hasher::Sha512(h) => Integrity::new(Algo::Sha512, h.finalize().to_vec()),
        }
    }
}

pub fn sha256_bytes(data: &[u8]) -> Integrity {
    let mut h = Hasher::new(Algo::Sha256);
    h.update(data);
    h.finish()
}

pub fn hash_bytes(algo: Algo, data: &[u8]) -> Integrity {
    let mut h = Hasher::new(algo);
    h.update(data);
    h.finish()
}

pub struct Store {
    root: PathBuf,
    seq: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct StoredBlob {
    pub path: PathBuf,
    pub integrity: Integrity,
    pub sha256: Integrity,
    pub size: u64,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Store {
            root,
            seq: AtomicU64::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn blob_path(&self, i: &Integrity) -> PathBuf {
        let h = i.hex();
        self.root.join("blobs").join(i.algo.name()).join(&h[..2]).join(&h)
    }

    pub fn get(&self, i: &Integrity) -> Option<StoredBlob> {
        let path = self.blob_path(i);
        let meta = fs::metadata(&path).ok()?;
        if let Ok(f) = fs::File::options().write(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        let sha256 = if i.algo == Algo::Sha256 {
            i.clone()
        } else {
            let side = path.with_extension("sha256");
            let s = fs::read_to_string(side).ok()?;
            Integrity::parse_hex(Algo::Sha256, &s).ok()?
        };
        Some(StoredBlob {
            path,
            integrity: i.clone(),
            sha256,
            size: meta.len(),
        })
    }

    pub fn temp_path(&self, tag: &str) -> PathBuf {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        self.root
            .join("tmp")
            .join(format!("{}-{}-{}", std::process::id(), n, tag))
    }

    pub fn writer(&self, what: impl Into<String>, expected: Option<Integrity>) -> Result<BlobWriter<'_>> {
        let tmp = self.temp_path("blob");
        let file = File::create(&tmp)?;
        let extra = expected
            .as_ref()
            .filter(|e| e.algo != Algo::Sha256)
            .map(|e| Hasher::new(e.algo));
        Ok(BlobWriter {
            store: self,
            what: what.into(),
            file: Some(BufWriter::with_capacity(256 * 1024, file)),
            tmp,
            sha256: sha2::Sha256::new(),
            extra,
            expected,
            size: 0,
        })
    }

    pub fn put_bytes(&self, what: &str, data: &[u8], expected: Option<Integrity>) -> Result<StoredBlob> {
        let mut w = self.writer(what, expected)?;
        w.write_all(data)?;
        w.commit()
    }

    pub fn put_reader(&self, what: &str, r: &mut dyn Read, expected: Option<Integrity>) -> Result<StoredBlob> {
        let mut w = self.writer(what, expected)?;
        io::copy(r, &mut w)?;
        w.commit()
    }
}

pub struct BlobWriter<'a> {
    store: &'a Store,
    what: String,
    file: Option<BufWriter<File>>,
    tmp: PathBuf,
    sha256: sha2::Sha256,
    extra: Option<Hasher>,
    expected: Option<Integrity>,
    size: u64,
}

impl Write for BlobWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.as_mut().expect("writer open").write(buf)?;
        self.sha256.update(&buf[..n]);
        if let Some(h) = self.extra.as_mut() {
            h.update(&buf[..n]);
        }
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("writer open").flush()
    }
}

impl BlobWriter<'_> {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn commit(mut self) -> Result<StoredBlob> {
        let mut file = self.file.take().expect("writer open");
        file.flush()?;
        drop(file);
        let sha256 = Integrity::new(Algo::Sha256, std::mem::take(&mut self.sha256).finalize().to_vec());
        let actual = match self.extra.take() {
            Some(h) => h.finish(),
            None => sha256.clone(),
        };
        if let Some(exp) = &self.expected
            && *exp != actual
        {
            let _ = fs::remove_file(&self.tmp);
            return Err(Error::IntegrityMismatch {
                what: self.what.clone(),
                expected: exp.to_sri(),
                actual: actual.to_sri(),
            });
        }
        let key = actual.clone();
        let path = self.store.blob_path(&key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if key.algo != Algo::Sha256 {
            fs::write(path.with_extension("sha256"), sha256.hex())?;
        }
        fs::rename(&self.tmp, &path)?;
        acropolis_events::add_written(self.size);
        Ok(StoredBlob {
            path,
            integrity: key,
            sha256,
            size: self.size,
        })
    }
}

impl Drop for BlobWriter<'_> {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

pub struct HashingReader<R> {
    inner: R,
    hasher: Hasher,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R, algo: Algo) -> Self {
        HashingReader {
            inner,
            hasher: Hasher::new(algo),
            count: 0,
        }
    }

    pub fn finish(self) -> (Integrity, u64, R) {
        (self.hasher.finish(), self.count, self.inner)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

pub struct HashingWriter<W> {
    inner: W,
    hasher: sha2::Sha256,
    count: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: sha2::Sha256::new(),
            count: 0,
        }
    }

    pub fn finish(self) -> (Integrity, u64, W) {
        (
            Integrity::new(Algo::Sha256, self.hasher.finalize().to_vec()),
            self.count,
            self.inner,
        )
    }

    pub fn count(&self) -> u64 {
        self.count
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sri_roundtrip() {
        let i = hash_bytes(Algo::Sha512, b"hello");
        let s = i.to_sri();
        assert_eq!(Integrity::parse_sri(&s).unwrap(), i);
        let multi = format!(
            "sha1-{} {}",
            base64::engine::general_purpose::STANDARD.encode([0u8; 20]),
            s
        );
        assert_eq!(Integrity::parse_sri(&multi).unwrap().algo, Algo::Sha512);
    }

    #[test]
    fn store_verifies_and_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let good = hash_bytes(Algo::Sha512, b"payload");
        let blob = store.put_bytes("good", b"payload", Some(good.clone())).unwrap();
        assert_eq!(fs::read(&blob.path).unwrap(), b"payload");
        assert_eq!(blob.sha256, sha256_bytes(b"payload"));
        assert!(store.get(&good).is_some());
        let err = store.put_bytes("bad", b"tampered", Some(good.clone())).unwrap_err();
        assert!(matches!(err, Error::IntegrityMismatch { .. }));
        assert_eq!(fs::read_dir(store.tmp_dir()).unwrap().count(), 0);
    }
}
