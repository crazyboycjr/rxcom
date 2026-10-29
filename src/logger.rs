use log::{LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Logs to stderr, each line led by the local time of day, so a log lines up
/// with what someone saw on screen.
struct Logger;

impl Log for Logger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, r: &Record) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let secs = now.as_secs() as libc::time_t;
        // SAFETY: localtime_r only writes the struct it is given.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        let (h, m, s, ms) = (tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis());
        let _ = writeln!(std::io::stderr().lock(), "{h:02}:{m:02}:{s:02}.{ms:03} {:<5} {}", r.level(), r.args());
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

/// The level comes from `verbosity` (0 = info), or from RXCOM_LOG (error, warn,
/// info, debug, trace) when that is set.
pub fn init(verbosity: u8) {
    let level = std::env::var("RXCOM_LOG").ok().and_then(|v| v.parse().ok()).unwrap_or(match verbosity {
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    });
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}
