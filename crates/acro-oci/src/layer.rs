use crate::tar::TarWriter;
use acro_store::{BlobWriter, HashingWriter, Integrity, Store};
use anyhow::Result;
use flate2::Compression as GzLevel;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::io::{self, BufWriter, Write};
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

pub enum Compressor<W: Write> {
    Plain(W),
    Gzip(GzEncoder<W>),
    Zstd(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> Write for Compressor<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Compressor::Plain(w) => w.write(buf),
            Compressor::Gzip(w) => w.write(buf),
            Compressor::Zstd(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Compressor::Plain(w) => w.flush(),
            Compressor::Gzip(w) => w.flush(),
            Compressor::Zstd(w) => w.flush(),
        }
    }
}

impl<W: Write> Compressor<W> {
    fn finish(self) -> io::Result<W> {
        match self {
            Compressor::Plain(w) => Ok(w),
            Compressor::Gzip(w) => w.finish(),
            Compressor::Zstd(w) => w.finish(),
        }
    }
}

pub struct LayerWriter<'s> {
    tar: TarWriter<BufWriter<HashingWriter<Compressor<BlobWriter<'s>>>>>,
    compression: Compression,
    comment: String,
}

#[derive(Clone, Copy, Debug)]
pub struct LayerOptions {
    pub compression: Compression,
    pub level: i32,
    pub threads: u32,
}

impl Default for LayerOptions {
    fn default() -> Self {
        LayerOptions { compression: Compression::Gzip, level: 0, threads: 0 }
    }
}

impl<'s> LayerWriter<'s> {
    pub fn new(store: &'s Store, comment: impl Into<String>, opts: LayerOptions) -> Result<Self> {
        let comment = comment.into();
        let blob = store.writer(format!("layer {comment}"), None)?;
        let comp = match opts.compression {
            Compression::None => Compressor::Plain(blob),
            Compression::Gzip => {
                let level = if opts.level > 0 { opts.level as u32 } else { 6 };
                Compressor::Gzip(GzEncoder::new(blob, GzLevel::new(level)))
            }
            Compression::Zstd => {
                let level = if opts.level != 0 { opts.level } else { 3 };
                let mut enc = zstd::stream::write::Encoder::new(blob, level)?;
                if opts.threads > 1 {
                    enc.multithread(opts.threads)?;
                }
                enc.include_checksum(false)?;
                enc.include_contentsize(false)?;
                Compressor::Zstd(enc)
            }
        };
        let hashing = HashingWriter::new(comp);
        Ok(LayerWriter {
            tar: TarWriter::new(BufWriter::with_capacity(256 * 1024, hashing)),
            compression: opts.compression,
            comment,
        })
    }

    pub fn tar(&mut self) -> &mut TarWriter<BufWriter<HashingWriter<Compressor<BlobWriter<'s>>>>> {
        &mut self.tar
    }

    pub fn finish(self) -> Result<Layer> {
        let buf = self.tar.finish()?;
        let hashing = buf.into_inner().map_err(|e| e.into_error())?;
        let (diff_id, uncompressed_size, comp) = hashing.finish();
        let blob = comp.finish()?;
        let stored = blob.commit()?;
        Ok(Layer {
            digest: stored.sha256.clone(),
            diff_id,
            size: stored.size,
            uncompressed_size,
            media_type: self.compression.media_type().to_string(),
            path: stored.path,
            comment: self.comment,
        })
    }
}

fn compress_chunk(data: &[u8], opts: &LayerOptions) -> io::Result<Vec<u8>> {
    match opts.compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Gzip => {
            let level = if opts.level > 0 { opts.level as u32 } else { 6 };
            let mut enc = GzEncoder::new(Vec::with_capacity(data.len() / 3), GzLevel::new(level));
            enc.write_all(data)?;
            enc.finish()
        }
        Compression::Zstd => {
            let level = if opts.level != 0 { opts.level } else { 3 };
            zstd::bulk::compress(data, level)
        }
    }
}

pub fn from_fragments(
    store: &Store,
    comment: impl Into<String>,
    fragments: Vec<Vec<u8>>,
    opts: LayerOptions,
) -> Result<Layer> {
    use rayon::prelude::*;
    use sha2::Digest;
    const CHUNK: usize = 1 << 20;
    let comment = comment.into();
    let mut chunks: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut cur: Vec<Vec<u8>> = Vec::new();
    let mut cur_len = 0;
    for f in fragments.into_iter().chain(std::iter::once(vec![0u8; 1024])) {
        if f.is_empty() {
            continue;
        }
        cur_len += f.len();
        cur.push(f);
        if cur_len >= CHUNK {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    let compressed: Vec<io::Result<(Vec<u8>, sha2::Sha256, u64)>> = chunks
        .par_iter()
        .map(|parts| {
            let joined: Vec<u8> = parts.concat();
            let c = compress_chunk(&joined, &opts)?;
            Ok((c, sha2::Sha256::new(), joined.len() as u64))
        })
        .collect();
    let mut diff = sha2::Sha256::new();
    for parts in &chunks {
        for p in parts {
            diff.update(p);
        }
    }
    let mut blob = store.writer(format!("layer {comment}"), None)?;
    let mut uncompressed_size = 0;
    for c in compressed {
        let (bytes, _, n) = c?;
        blob.write_all(&bytes)?;
        uncompressed_size += n;
    }
    let stored = blob.commit()?;
    Ok(Layer {
        digest: stored.sha256.clone(),
        diff_id: acro_store::Integrity::new(acro_store::Algo::Sha256, diff.finalize().to_vec()),
        size: stored.size,
        uncompressed_size,
        media_type: opts.compression.media_type().to_string(),
        path: stored.path,
        comment,
    })
}
