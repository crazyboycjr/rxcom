//! Restarts rxcom when one turn of its loop takes far too long: a spin, or a
//! call into a driver that never returns. The stuck process aborts, and the X
//! server undoes its redirection when the connection closes, so the desktop is
//! usable meanwhile; a new rxcom takes over after a backoff, with the X
//! presenter if GL or Vulkan was being set up.
//!
//! Waiting for the X server does not count. A stalled or grabbed server freezes
//! the desktop whatever rxcom does, and rxcom carries on once it answers.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// When the current turn started, in ms since START plus one; 0 while idle.
static BUSY_SINCE: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();
static SETTING_UP: AtomicBool = AtomicBool::new(false);
/// After this many restarts in a row rxcom gives up.
const MAX_RESTARTS: u32 = 5;

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// How many times the watchdog has restarted rxcom so far.
pub fn restarts() -> u32 {
    std::env::var("RXCOM_RESTARTS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Whether the thread is blocked in poll(), which for rxcom's main thread
/// means waiting for a reply from the X server.
fn waiting_on_fd(tid: libc::pid_t) -> bool {
    let Ok(s) = std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall")) else { return false };
    let nr: i64 = s.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(-1);
    #[cfg(target_arch = "x86_64")]
    let polls = [libc::SYS_poll, libc::SYS_ppoll];
    #[cfg(not(target_arch = "x86_64"))]
    let polls = [libc::SYS_ppoll];
    polls.contains(&nr)
}

fn restart() {
    let n = restarts() + 1;
    if n > MAX_RESTARTS {
        log::error!("restarted {MAX_RESTARTS} times already; giving up");
        return;
    }
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if SETTING_UP.load(Ordering::Relaxed) {
        log::error!("stuck setting the presenter up; the X server presents from now on");
        args.extend(["--present".into(), "x".into()]);
    }
    let child = std::process::Command::new("/proc/self/exe").args(args).env("RXCOM_RESTARTS", n.to_string()).spawn();
    match child {
        Ok(c) => log::error!("restarting as pid {} (restart {n})", c.id()),
        Err(e) => log::error!("cannot restart: {e}"),
    }
}

/// Starts the watchdog for the calling thread, the main loop's.
pub fn start(limit: Duration) {
    now_ms();
    let main = unsafe { libc::gettid() };
    let spawned = std::thread::Builder::new().name("rxcom-watchdog".into()).spawn(move || {
        let mut warned = 0;
        loop {
            std::thread::sleep(limit / 10);
            let since = BUSY_SINCE.load(Ordering::Relaxed);
            let stuck = now_ms().saturating_sub(since);
            if since == 0 || stuck <= limit.as_millis() as u64 {
                continue;
            }
            if !waiting_on_fd(main) {
                log::error!(
                    "stuck for {} s in one turn of the loop; aborting so the X server restores the screen",
                    stuck / 1000
                );
                restart();
                std::process::abort();
            }
            if since != warned {
                log::warn!("the X server has not answered for {} s", stuck / 1000);
                warned = since;
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("no watchdog: {e}");
    }
}

pub fn busy() {
    BUSY_SINCE.store(now_ms(), Ordering::Relaxed);
}

pub fn idle() {
    BUSY_SINCE.store(0, Ordering::Relaxed);
}

/// Marks the GL or Vulkan presenter's set-up, for the duration of `f`.
pub fn setting_up<T>(f: impl FnOnce() -> T) -> T {
    SETTING_UP.store(true, Ordering::Relaxed);
    let t = f();
    SETTING_UP.store(false, Ordering::Relaxed);
    t
}
