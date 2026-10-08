use std::io::Read;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let t = Instant::now();
    let f = std::fs::File::open(path).unwrap();
    let mut gz = flate2::read::GzDecoder::new(std::io::BufReader::with_capacity(1 << 20, f));
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = gz.read(&mut buf).unwrap();
        if k == 0 {
            break;
        }
        n += k as u64;
    }
    println!("decode only: {} MB in {:?}", n / 1_000_000, t.elapsed());
    let t = Instant::now();
    let f = std::fs::File::open(path).unwrap();
    let mut gz = flate2::read::GzDecoder::new(std::io::BufReader::with_capacity(1 << 20, f));
    let dest = std::path::Path::new(&args[2]);
    let s = acropolis_toolchain::extract_tar_filtered(&mut gz, dest, 2, &|_| true).unwrap();
    println!("decode+extract: {} files {} MB in {:?}", s.files, s.bytes / 1_000_000, t.elapsed());
}
