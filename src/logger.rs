use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

struct Logger {
    start: Instant,
}

impl Log for Logger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let t = self.start.elapsed().as_secs_f64();
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let _ = writeln!(std::io::stderr().lock(), "{t:9.3} {level} {}", record.args());
    }

    fn flush(&self) {}
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Logs to stderr. The level comes from `verbosity` (0 = info), or from RXCOM_LOG
/// (error, warn, info, debug, trace) when that is set.
pub fn init(verbosity: u8) {
    let mut level = match verbosity {
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    if let Ok(v) = std::env::var("RXCOM_LOG")
        && let Ok(l) = v.parse() {
            level = l;
        }
    let logger = LOGGER.get_or_init(|| Logger { start: Instant::now() });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
