use std::io::{self, Read, Write};

const BLOCK: usize = 512;
/// PAX and GNU long-name records are a few hundred bytes; the size field is attacker-controlled.
const MAX_META: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Hardlink,
}

pub struct TarWriter<W: Write> {
    out: W,
    entries: u64,
    uid: u32,
    gid: u32,
}

fn octal(field: &mut [u8], value: u64) -> bool {
    let width = field.len() - 1;
    let s = format!("{value:0width$o}");
    if s.len() > width {
        return false;
    }
    field[..width].copy_from_slice(s.as_bytes());
    field[width] = 0;
    true
}

fn split_ustar(path: &str) -> Option<(&str, &str)> {
    let bytes = path.as_bytes();
    if bytes.len() <= 100 {
        return Some(("", path));
    }
    if bytes.len() > 256 {
        return None;
    }
    let mut i = bytes.len().min(156);
    while i > 0 {
        i -= 1;
        if bytes[i] == b'/' && i <= 155 && bytes.len() - i - 1 <= 100 && bytes.len() - i - 1 > 0 {
            return Some((&path[..i], &path[i + 1..]));
        }
    }
    None
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let candidate = len.to_string().len() + body.len();
        if candidate == len {
            break;
        }
        len = candidate;
    }
    format!("{len}{body}").into_bytes()
}

impl<W: Write> TarWriter<W> {
    pub fn new(out: W) -> Self {
        TarWriter {
            out,
            entries: 0,
            uid: 0,
            gid: 0,
        }
    }

    pub fn entries(&self) -> u64 {
        self.entries
    }

    pub fn set_owner(&mut self, uid: u32, gid: u32) {
        self.uid = uid;
        self.gid = gid;
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    fn header(&mut self, kind: Kind, path: &str, mode: u32, size: u64, link: &str) -> io::Result<()> {
        let mut pax = Vec::new();
        let (prefix, name) = match split_ustar(path) {
            Some(p) => p,
            None => {
                pax.extend(pax_record("path", path));
                ("", truncate(path, 100))
            }
        };
        let linkname = if link.len() > 100 {
            pax.extend(pax_record("linkpath", link));
            truncate(link, 100)
        } else {
            link
        };
        let size_fits = size < 0o77777777777;
        if !size_fits {
            pax.extend(pax_record("size", &size.to_string()));
        }
        if !pax.is_empty() {
            let mut h = [0u8; BLOCK];
            fill(&mut h, 'x', "././@PaxHeader", 0o644, pax.len() as u64, "", "", (0, 0));
            self.out.write_all(&h)?;
            self.out.write_all(&pax)?;
            pad(&mut self.out, pax.len() as u64)?;
        }
        let mut h = [0u8; BLOCK];
        let flag = match kind {
            Kind::File => '0',
            Kind::Dir => '5',
            Kind::Symlink => '2',
            Kind::Hardlink => '1',
        };
        fill(
            &mut h,
            flag,
            name,
            mode,
            if size_fits { size } else { 0 },
            linkname,
            prefix,
            (self.uid, self.gid),
        );
        self.out.write_all(&h)?;
        self.entries += 1;
        Ok(())
    }

    pub fn dir(&mut self, path: &str, mode: u32) -> io::Result<()> {
        let p = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        self.header(Kind::Dir, &p, mode, 0, "")
    }

    pub fn symlink(&mut self, path: &str, target: &str) -> io::Result<()> {
        self.header(Kind::Symlink, path, 0o777, 0, target)
    }

    pub fn hardlink(&mut self, path: &str, target: &str) -> io::Result<()> {
        self.header(Kind::Hardlink, path, 0o644, 0, target)
    }

    pub fn hardlink_mode(&mut self, path: &str, target: &str, mode: u32) -> io::Result<()> {
        self.header(Kind::Hardlink, path, mode, 0, target)
    }

    pub fn file_bytes(&mut self, path: &str, mode: u32, data: &[u8]) -> io::Result<()> {
        self.header(Kind::File, path, mode, data.len() as u64, "")?;
        self.out.write_all(data)?;
        pad(&mut self.out, data.len() as u64)
    }

    pub fn file_reader(&mut self, path: &str, mode: u32, size: u64, r: &mut dyn Read) -> io::Result<()> {
        self.header(Kind::File, path, mode, size, "")?;
        let copied = io::copy(&mut r.take(size), &mut self.out)?;
        if copied != size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{path}: expected {size} bytes, got {copied}"),
            ));
        }
        pad(&mut self.out, size)
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        Ok(self.out)
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn pad<W: Write>(out: &mut W, size: u64) -> io::Result<()> {
    let rem = (size % BLOCK as u64) as usize;
    if rem != 0 {
        out.write_all(&[0u8; BLOCK][..BLOCK - rem])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fill(
    h: &mut [u8; BLOCK],
    flag: char,
    name: &str,
    mode: u32,
    size: u64,
    link: &str,
    prefix: &str,
    owner: (u32, u32),
) {
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], (mode & 0o7777) as u64);
    octal(&mut h[108..116], owner.0 as u64);
    octal(&mut h[116..124], owner.1 as u64);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = flag as u8;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
}

pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub link: String,
    pub uid: u32,
    pub gid: u32,
}

pub struct TarReader<R: Read> {
    inner: R,
    remaining: u64,
    padding: u64,
    done: bool,
}

impl<R: Read> TarReader<R> {
    pub fn new(inner: R) -> Self {
        TarReader {
            inner,
            remaining: 0,
            padding: 0,
            done: false,
        }
    }

    fn skip_rest(&mut self) -> io::Result<()> {
        let n = self.remaining + self.padding;
        if n > 0 {
            io::copy(&mut (&mut self.inner).take(n), &mut io::sink())?;
        }
        self.remaining = 0;
        self.padding = 0;
        Ok(())
    }

    pub fn next_entry(&mut self) -> io::Result<Option<Entry>> {
        if self.done {
            return Ok(None);
        }
        self.skip_rest()?;
        let mut pax_path: Option<String> = None;
        let mut pax_link: Option<String> = None;
        let mut pax_size: Option<u64> = None;
        let mut gnu_long_name: Option<String> = None;
        let mut gnu_long_link: Option<String> = None;
        loop {
            let mut h = [0u8; BLOCK];
            if !read_full(&mut self.inner, &mut h)? {
                self.done = true;
                return Ok(None);
            }
            if h.iter().all(|&b| b == 0) {
                self.done = true;
                return Ok(None);
            }
            let flag = h[156];
            let size = parse_num(&h[124..136]);
            let size = pax_size.take().unwrap_or(size);
            match flag {
                b'x' | b'g' | b'L' | b'K' => {
                    if size > MAX_META {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("tar metadata entry of {size} bytes"),
                        ));
                    }
                    let mut data = vec![0u8; size as usize];
                    self.inner.read_exact(&mut data)?;
                    let p = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
                    io::copy(&mut (&mut self.inner).take(p), &mut io::sink())?;
                    match flag {
                        b'x' => {
                            for (k, v) in parse_pax(&data) {
                                match k.as_str() {
                                    "path" => pax_path = Some(v),
                                    "linkpath" => pax_link = Some(v),
                                    "size" => pax_size = v.parse().ok(),
                                    _ => {}
                                }
                            }
                        }
                        b'L' => gnu_long_name = Some(cstr(&data)),
                        b'K' => gnu_long_link = Some(cstr(&data)),
                        _ => {}
                    }
                    continue;
                }
                _ => {}
            }
            let name = cstr(&h[0..100]);
            let prefix = if &h[257..262] == b"ustar" {
                cstr(&h[345..500])
            } else {
                String::new()
            };
            let path = pax_path.take().or(gnu_long_name.take()).unwrap_or_else(|| {
                if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                }
            });
            let link = pax_link
                .take()
                .or(gnu_long_link.take())
                .unwrap_or_else(|| cstr(&h[157..257]));
            let mode = parse_num(&h[100..108]) as u32;
            let uid = parse_num(&h[108..116]) as u32;
            let gid = parse_num(&h[116..124]) as u32;
            let kind = match flag {
                b'0' | 0 | b'7' => Kind::File,
                b'5' => Kind::Dir,
                b'2' => Kind::Symlink,
                b'1' => Kind::Hardlink,
                _ => {
                    self.remaining = size;
                    self.padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
                    self.skip_rest()?;
                    continue;
                }
            };
            let kind = if kind == Kind::File && path.ends_with('/') {
                Kind::Dir
            } else {
                kind
            };
            let data_size = if kind == Kind::File { size } else { 0 };
            self.remaining = data_size;
            self.padding = (BLOCK as u64 - data_size % BLOCK as u64) % BLOCK as u64;
            return Ok(Some(Entry {
                path,
                kind,
                mode,
                size: data_size,
                link,
                uid,
                gid,
            }));
        }
    }

    pub fn data(&mut self) -> DataReader<'_, R> {
        DataReader { tr: self }
    }

    pub fn read_data(&mut self) -> io::Result<Vec<u8>> {
        let mut v = Vec::with_capacity(self.remaining.min(MAX_META) as usize);
        self.data().read_to_end(&mut v)?;
        Ok(v)
    }
}

pub struct DataReader<'a, R: Read> {
    tr: &'a mut TarReader<R>,
}

impl<R: Read> Read for DataReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.tr.remaining == 0 {
            return Ok(0);
        }
        let max = (buf.len() as u64).min(self.tr.remaining) as usize;
        let n = self.tr.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated tar entry"));
        }
        self.tr.remaining -= n as u64;
        Ok(n)
    }
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut off = 0;
    while off < buf.len() {
        let n = r.read(&mut buf[off..])?;
        if n == 0 {
            if off == 0 {
                return Ok(false);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated tar header"));
        }
        off += n;
    }
    Ok(true)
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn parse_num(b: &[u8]) -> u64 {
    if b[0] & 0x80 != 0 {
        let mut v: u64 = (b[0] & 0x7f) as u64;
        for &c in &b[1..] {
            v = (v << 8) | c as u64;
        }
        return v;
    }
    let s = cstr(b);
    u64::from_str_radix(s.trim().trim_matches(char::from(0)), 8).unwrap_or(0)
}

fn parse_pax(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let Some(sp) = rest.iter().position(|&c| c == b' ') else {
            break;
        };
        let Ok(len) = std::str::from_utf8(&rest[..sp]).unwrap_or("").parse::<usize>() else {
            break;
        };
        if len == 0 || len > rest.len() {
            break;
        }
        let rec = &rest[sp + 1..len];
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(eq) = rec.iter().position(|&c| c == b'=') {
            out.push((
                String::from_utf8_lossy(&rec[..eq]).into_owned(),
                String::from_utf8_lossy(&rec[eq + 1..]).into_owned(),
            ));
        }
        rest = &rest[len..];
    }
    out
}

pub fn normalize_mode(mode: u32, kind: Kind) -> u32 {
    match kind {
        Kind::Dir => 0o755,
        Kind::Symlink => 0o777,
        _ => {
            if mode & 0o111 != 0 {
                0o755
            } else {
                0o644
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header whose base-256 size field declares 1 TiB.
    fn header_of_one_tib(flag: char) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        fill(&mut h, flag, "entry", 0o644, 0, "", "", (0, 0));
        h[124..136].fill(0);
        h[124] = 0x80;
        h[130] = 1;
        h
    }

    #[test]
    fn huge_declared_sizes_fail_without_allocating_them() {
        let meta = header_of_one_tib('x');
        assert!(TarReader::new(&meta[..]).next_entry().is_err());
        let file = header_of_one_tib('0');
        let mut r = TarReader::new(&file[..]);
        assert_eq!(r.next_entry().unwrap().unwrap().size, 1 << 40);
        assert!(r.read_data().is_err());
    }

    #[test]
    fn roundtrip_long_paths() {
        let long = format!("a/{}/{}", "b".repeat(120), "c".repeat(90));
        let very_long = format!("x/{}", "y".repeat(300));
        let mut w = TarWriter::new(Vec::new());
        w.dir("app", 0o755).unwrap();
        w.file_bytes("app/hello.txt", 0o644, b"hi").unwrap();
        w.file_bytes(&long, 0o755, b"long").unwrap();
        w.file_bytes(&very_long, 0o644, b"pax").unwrap();
        w.symlink("app/link", &"t".repeat(150)).unwrap();
        let buf = w.finish().unwrap();
        let mut r = TarReader::new(&buf[..]);
        let mut seen = Vec::new();
        while let Some(e) = r.next_entry().unwrap() {
            let data = r.read_data().unwrap();
            seen.push((e.path, e.kind, e.mode, data, e.link));
        }
        assert_eq!(seen.len(), 5);
        assert_eq!(seen[0].0, "app/");
        assert_eq!(seen[1].3, b"hi");
        assert_eq!(seen[2].0, long);
        assert_eq!(seen[2].2, 0o755);
        assert_eq!(seen[3].0, very_long);
        assert_eq!(seen[3].3, b"pax");
        assert_eq!(seen[4].4, "t".repeat(150));
    }

    #[test]
    fn deterministic() {
        let build = || {
            let mut w = TarWriter::new(Vec::new());
            w.file_bytes("a", 0o644, b"x").unwrap();
            w.finish().unwrap()
        };
        assert_eq!(build(), build());
    }
}
