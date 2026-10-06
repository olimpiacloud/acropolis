use serde::Serialize;
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    BuildStarted { app: String },
    PlanReady { hash: String, steps: usize },
    StepStarted { id: String, name: String },
    StepFinished { id: String, name: String, ms: u64 },
    StepFailed { id: String, name: String, error: String },
    Log { step: String, line: String },
    Downloaded { what: String, bytes: u64, ms: u64 },
    Uploaded { what: String, bytes: u64, skipped: bool },
    ImagePushed { reference: String, digest: String },
    BuildFinished { ms: u64, image: Option<String>, digest: Option<String> },
    BuildFailed { ms: u64, class: String, exit_code: i32, error: String },
    Stats(Stats),
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Stats {
    pub bytes_downloaded: u64,
    pub bytes_uploaded: u64,
    pub bytes_written: u64,
    pub requests: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Human = 0,
    Json = 1,
    Quiet = 2,
}

struct Emitter {
    start: Instant,
    mode: AtomicU8,
    out: Mutex<()>,
}

static EMITTER: OnceLock<Emitter> = OnceLock::new();
static DOWNLOADED: AtomicU64 = AtomicU64::new(0);
static UPLOADED: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);

fn emitter() -> &'static Emitter {
    EMITTER.get_or_init(|| Emitter {
        start: Instant::now(),
        mode: AtomicU8::new(Mode::Human as u8),
        out: Mutex::new(()),
    })
}

pub fn init(mode: Mode) {
    emitter().mode.store(mode as u8, Ordering::Relaxed);
}

pub fn mode() -> Mode {
    match emitter().mode.load(Ordering::Relaxed) {
        1 => Mode::Json,
        2 => Mode::Quiet,
        _ => Mode::Human,
    }
}

pub fn elapsed_ms() -> u64 {
    emitter().start.elapsed().as_millis() as u64
}

pub fn emit(event: Event) {
    let e = emitter();
    match mode() {
        Mode::Quiet => {}
        Mode::Json => {
            #[derive(Serialize)]
            struct Line<'a> {
                t: u64,
                #[serde(flatten)]
                event: &'a Event,
            }
            let line = serde_json::to_string(&Line { t: elapsed_ms(), event: &event }).unwrap_or_default();
            let _g = e.out.lock();
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{line}");
        }
        Mode::Human => {
            let t = elapsed_ms() as f64 / 1000.0;
            let text = match &event {
                Event::BuildStarted { app } => format!("build {app}"),
                Event::PlanReady { hash, steps } => format!("plan {} ({steps} steps)", &hash[..hash.len().min(12)]),
                Event::StepStarted { name, .. } => format!("> {name}"),
                Event::StepFinished { name, ms, .. } => format!("✓ {name} ({:.2}s)", *ms as f64 / 1000.0),
                Event::StepFailed { name, error, .. } => format!("✗ {name}: {error}"),
                Event::Log { step, line } => format!("  [{step}] {line}"),
                Event::Downloaded { .. } => return,
                Event::Uploaded { .. } => return,
                Event::ImagePushed { reference, digest } => format!("pushed {reference}@{digest}"),
                Event::BuildFinished { ms, .. } => format!("done in {:.2}s", *ms as f64 / 1000.0),
                Event::BuildFailed { .. } => return,
                Event::Stats(s) => format!(
                    "downloaded {:.1} MB, uploaded {:.1} MB, written {:.1} MB, {} requests",
                    s.bytes_downloaded as f64 / 1e6,
                    s.bytes_uploaded as f64 / 1e6,
                    s.bytes_written as f64 / 1e6,
                    s.requests
                ),
            };
            let _g = e.out.lock();
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{t:>7.2}s {text}");
        }
    }
}

pub fn add_downloaded(n: u64) {
    DOWNLOADED.fetch_add(n, Ordering::Relaxed);
}

pub fn add_uploaded(n: u64) {
    UPLOADED.fetch_add(n, Ordering::Relaxed);
}

pub fn add_written(n: u64) {
    WRITTEN.fetch_add(n, Ordering::Relaxed);
}

pub fn add_request() {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
}

pub fn stats() -> Stats {
    Stats {
        bytes_downloaded: DOWNLOADED.load(Ordering::Relaxed),
        bytes_uploaded: UPLOADED.load(Ordering::Relaxed),
        bytes_written: WRITTEN.load(Ordering::Relaxed),
        requests: REQUESTS.load(Ordering::Relaxed),
    }
}

pub struct StepGuard {
    id: String,
    name: String,
    start: Instant,
    done: bool,
}

pub fn step(id: impl Into<String>, name: impl Into<String>) -> StepGuard {
    let g = StepGuard { id: id.into(), name: name.into(), start: Instant::now(), done: false };
    emit(Event::StepStarted { id: g.id.clone(), name: g.name.clone() });
    g
}

impl StepGuard {
    pub fn finish(mut self) {
        self.done = true;
        emit(Event::StepFinished {
            id: self.id.clone(),
            name: self.name.clone(),
            ms: self.start.elapsed().as_millis() as u64,
        });
    }

    pub fn fail(mut self, error: &dyn std::fmt::Display) {
        self.done = true;
        emit(Event::StepFailed { id: self.id.clone(), name: self.name.clone(), error: error.to_string() });
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for StepGuard {
    fn drop(&mut self) {
        if !self.done {
            emit(Event::StepFailed { id: self.id.clone(), name: self.name.clone(), error: "aborted".into() });
        }
    }
}

pub fn log(step: &str, line: impl Into<String>) {
    emit(Event::Log { step: step.to_string(), line: line.into() });
}
