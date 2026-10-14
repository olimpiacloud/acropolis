use crate::tar::PathGuard;
use acropolis_store::{BlobWriter, Integrity, Store};
use anyhow::Result;
use flate2::Compression as GzLevel;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;

pub const MT_OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MT_OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
pub const MT_OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

impl Compression {
    pub fn media_type(self) -> &'static str {
        match self {
            Compression::None => MT_OCI_LAYER_TAR,
            Compression::Gzip => MT_OCI_LAYER_GZIP,
            Compression::Zstd => MT_OCI_LAYER_ZSTD,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" | "tar" => Some(Compression::None),
            "gzip" | "gz" => Some(Compression::Gzip),
            "zstd" | "zst" => Some(Compression::Zstd),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub digest: Integrity,
    pub diff_id: Integrity,
    pub size: u64,
    pub uncompressed_size: u64,
    pub media_type: String,
    pub path: PathBuf,
    pub comment: String,
}

#[derive(Clone, Copy, Debug)]
pub struct LayerOptions {
    pub compression: Compression,
    pub level: i32,
    pub threads: u32,
}

impl Default for LayerOptions {
    fn default() -> Self {
        LayerOptions {
            compression: Compression::Gzip,
            level: 0,
            threads: 0,
        }
    }
}

/// `None` means the chunk goes out as is.
fn compress_chunk(data: &[u8], opts: &LayerOptions) -> io::Result<Option<Vec<u8>>> {
    match opts.compression {
        Compression::None => Ok(None),
        Compression::Gzip => {
            let level = if opts.level > 0 { opts.level as u32 } else { 6 };
            let mut enc = GzEncoder::new(Vec::with_capacity(data.len() / 3), GzLevel::new(level));
            enc.write_all(data)?;
            enc.finish().map(Some)
        }
        Compression::Zstd => {
            let level = if opts.level != 0 { opts.level } else { 3 };
            zstd::bulk::compress(data, level).map(Some)
        }
    }
}

/// Every layer goes through here: the tar stream is checked as it is written, so no layer can carry a path twice
/// (overlay2's `docker pull` refuses it with "duplicates of file paths not supported", `docker load` does not).
pub struct LayerBuilder<'a> {
    blob: BlobWriter<'a>,
    diff: sha2::Sha256,
    opts: LayerOptions,
    cur: Vec<u8>,
    pending: Vec<Vec<u8>>,
    batch: usize,
    uncompressed_size: u64,
    comment: String,
    guard: PathGuard,
}

const LAYER_CHUNK: usize = 1 << 20;

impl<'a> LayerBuilder<'a> {
    pub fn new(store: &'a Store, comment: impl Into<String>, opts: LayerOptions) -> Result<Self> {
        use sha2::Digest;
        let comment = comment.into();
        Ok(LayerBuilder {
            blob: store.writer(format!("layer {comment}"), None)?,
            diff: sha2::Sha256::new(),
            opts,
            cur: Vec::with_capacity(LAYER_CHUNK + (64 << 10)),
            pending: Vec::new(),
            batch: (rayon::current_num_threads() * 2).max(2),
            uncompressed_size: 0,
            comment,
            guard: PathGuard::default(),
        })
    }

    fn flush_batch(&mut self) -> io::Result<()> {
        use rayon::prelude::*;
        use sha2::Digest;
        if self.pending.is_empty() {
            return Ok(());
        }
        let opts = self.opts;
        let compressed: Vec<io::Result<Option<Vec<u8>>>> =
            self.pending.par_iter().map(|c| compress_chunk(c, &opts)).collect();
        for (raw, c) in self.pending.drain(..).zip(compressed) {
            self.diff.update(&raw);
            self.uncompressed_size += raw.len() as u64;
            self.blob.write_all(c?.as_deref().unwrap_or(&raw))?;
        }
        Ok(())
    }

    fn seal_chunk(&mut self) -> io::Result<()> {
        if self.cur.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.cur, Vec::with_capacity(LAYER_CHUNK + (64 << 10)));
        self.pending.push(chunk);
        if self.pending.len() >= self.batch {
            self.flush_batch()?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Layer> {
        use sha2::Digest;
        self.guard.finish()?;
        self.cur.extend_from_slice(&[0u8; 1024]);
        self.seal_chunk()?;
        self.flush_batch()?;
        let stored = self.blob.commit()?;
        Ok(Layer {
            digest: stored.sha256.clone(),
            diff_id: acropolis_store::Integrity::new(acropolis_store::Algo::Sha256, self.diff.finalize().to_vec()),
            size: stored.size,
            uncompressed_size: self.uncompressed_size,
            media_type: self.opts.compression.media_type().to_string(),
            path: stored.path,
            comment: self.comment,
        })
    }
}

impl Write for LayerBuilder<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.guard.feed(buf)?;
        let mut rest = buf;
        while !rest.is_empty() {
            let room = LAYER_CHUNK.saturating_sub(self.cur.len()).max(1);
            let n = room.min(rest.len());
            self.cur.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if self.cur.len() >= LAYER_CHUNK {
                self.seal_chunk()?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn from_fragments(
    store: &Store,
    comment: impl Into<String>,
    fragments: Vec<Vec<u8>>,
    opts: LayerOptions,
) -> Result<Layer> {
    let mut b = LayerBuilder::new(store, comment, opts)?;
    for f in fragments {
        b.write_all(&f)?;
    }
    b.finish()
}

/// Reads a layer blob of `media_type` and fails on the first path it has twice: the check `docker pull` makes on
/// overlay2, for images that did not come from this crate's writer (or to double-check it).
pub fn verify_unique_paths(media_type: &str, blob: impl io::Read) -> io::Result<()> {
    fn scan(mut r: impl io::Read) -> io::Result<()> {
        let mut guard = PathGuard::default();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                return guard.finish();
            }
            guard.feed(&buf[..n])?;
        }
    }
    if media_type.ends_with("+zstd") {
        scan(zstd::stream::read::Decoder::new(blob)?)
    } else if media_type.ends_with("gzip") {
        scan(flate2::read::GzDecoder::new(blob))
    } else {
        scan(blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_reads_compressed_blobs_and_finds_the_duplicate() {
        let mut w = crate::tar::TarWriter::new(Vec::new());
        w.dir("app/node_modules/debug", 0o755).unwrap();
        w.file_bytes("app/node_modules/debug/package.json", 0o644, b"{}")
            .unwrap();
        w.dir("app/node_modules/debug", 0o755).unwrap();
        let tar = w.finish().unwrap();
        let zst = zstd::bulk::compress(&tar, 3).unwrap();
        let err = verify_unique_paths(MT_OCI_LAYER_ZSTD, &zst[..]).unwrap_err();
        assert!(err.to_string().contains("app/node_modules/debug twice"), "{err}");
        let mut gz = GzEncoder::new(Vec::new(), GzLevel::default());
        gz.write_all(&tar[..tar.len() - 1536]).unwrap();
        gz.write_all(&[0; 1024]).unwrap();
        assert!(verify_unique_paths(MT_OCI_LAYER_GZIP, &gz.finish().unwrap()[..]).is_ok());
    }
}
