use log::{LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

/// Logs to stderr, each line led by the seconds since start.
struct Logger(Instant);

impl Log for Logger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, r: &Record) {
        let t = self.0.elapsed().as_secs_f64();
        let _ = writeln!(std::io::stderr().lock(), "{t:9.3} {:<5} {}", r.level(), r.args());
    }

    fn flush(&self) {}
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// The level comes from `verbosity` (0 = info), or from RXCOM_LOG (error, warn,
/// info, debug, trace) when that is set.
pub fn init(verbosity: u8) {
    let level = std::env::var("RXCOM_LOG").ok().and_then(|v| v.parse().ok()).unwrap_or(match verbosity {
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    });
    if log::set_logger(LOGGER.get_or_init(|| Logger(Instant::now()))).is_ok() {
        log::set_max_level(level);
    }
}
