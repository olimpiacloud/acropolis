use anyhow::{Result, bail};
use std::io::Read;

pub struct ZipEntry<'a> {
    pub name: String,
    pub method: u16,
    pub data: &'a [u8],
    pub size: u64,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn entries(buf: &[u8]) -> Result<Vec<ZipEntry<'_>>> {
    if buf.len() < 22 {
        bail!("zip too small");
    }
    let start = buf.len().saturating_sub(22 + 65535);
    let mut eocd = None;
    let mut i = buf.len() - 22;
    loop {
        if u32le(buf, i) == 0x06054b50 {
            eocd = Some(i);
            break;
        }
        if i == start {
            break;
        }
        i -= 1;
    }
    let Some(e) = eocd else {
        bail!("zip end of central directory not found")
    };
    let count = u16le(buf, e + 10) as usize;
    let cd_off = u32le(buf, e + 16) as usize;
    if cd_off == 0xffffffff || count == 0xffff {
        bail!("zip64 archives are not supported");
    }
    let mut out = Vec::with_capacity(count);
    let mut p = cd_off;
    for _ in 0..count {
        if p + 46 > buf.len() || u32le(buf, p) != 0x02014b50 {
            bail!("corrupt zip central directory");
        }
        let method = u16le(buf, p + 10);
        let csize = u32le(buf, p + 20) as usize;
        let usize_ = u32le(buf, p + 24) as u64;
        let nlen = u16le(buf, p + 28) as usize;
        let elen = u16le(buf, p + 30) as usize;
        let clen = u16le(buf, p + 32) as usize;
        let local = u32le(buf, p + 42) as usize;
        let name = String::from_utf8_lossy(&buf[p + 46..p + 46 + nlen]).into_owned();
        p += 46 + nlen + elen + clen;
        if local + 30 > buf.len() || u32le(buf, local) != 0x04034b50 {
            bail!("corrupt zip local header for {name}");
        }
        let lnlen = u16le(buf, local + 26) as usize;
        let lelen = u16le(buf, local + 28) as usize;
        let data_start = local + 30 + lnlen + lelen;
        if data_start + csize > buf.len() {
            bail!("truncated zip entry {name}");
        }
        out.push(ZipEntry {
            name,
            method,
            data: &buf[data_start..data_start + csize],
            size: usize_,
        });
    }
    Ok(out)
}

impl ZipEntry<'_> {
    pub fn read_all(&self) -> Result<Vec<u8>> {
        match self.method {
            0 => Ok(self.data.to_vec()),
            8 => {
                let mut out = Vec::with_capacity(self.size as usize);
                flate2::read::DeflateDecoder::new(self.data).read_to_end(&mut out)?;
                if out.len() as u64 != self.size {
                    bail!("zip entry {} size mismatch", self.name);
                }
                Ok(out)
            }
            m => bail!("unsupported zip compression method {m} for {}", self.name),
        }
    }
}
