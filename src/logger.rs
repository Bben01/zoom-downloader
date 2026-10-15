use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub type Sink = Box<dyn Fn(Level, String) + Send + Sync>;

struct AppLogger {
    file: Mutex<Option<std::fs::File>>,
    sink: RwLock<Option<Sink>>,
}

static LOGGER: OnceLock<AppLogger> = OnceLock::new();

/// Logs to a file (and the in-app activity view once a sink is attached).
pub fn init(path: Option<PathBuf>) {
    let file = path.and_then(|p| {
        std::fs::create_dir_all(p.parent()?).ok()?;
        if std::fs::metadata(&p).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
            let _ = std::fs::rename(&p, p.with_extension("old.log"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(&p).ok()
    });
    let logger = LOGGER.get_or_init(|| AppLogger { file: Mutex::new(file), sink: RwLock::new(None) });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

pub fn set_sink(sink: Sink) {
    if let Some(l) = LOGGER.get() {
        *l.sink.write().unwrap() = Some(sink);
    }
}

impl Log for AppLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        if m.target().starts_with(env!("CARGO_CRATE_NAME")) {
            m.level() <= Level::Info
        } else {
            m.level() <= Level::Warn
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        let line = format!(
            "{} {:<5} {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            message
        );
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
        #[cfg(debug_assertions)]
        eprint!("{line}");
        if let Some(sink) = self.sink.read().unwrap().as_ref() {
            sink(record.level(), message);
        }
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = f.flush();
        }
    }
}
