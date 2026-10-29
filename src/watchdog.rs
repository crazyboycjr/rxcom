//! Aborts rxcom when one turn of its loop takes far too long: a spin, or a call
//! into a driver that never returns. The X server undoes the redirection when
//! rxcom's connection closes, so the desktop stays usable, uncomposited,
//! instead of frozen on the last frame.
//!
//! Waiting for the X server does not count. A stalled or grabbed server freezes
//! the desktop whatever rxcom does, and rxcom carries on once it answers.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// When the current turn started, in ms since START plus one; 0 while idle.
static BUSY_SINCE: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
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
                warned = 0;
                continue;
            }
            if !waiting_on_fd(main) {
                log::error!("stuck for {} s in one turn of the loop; aborting so the X server restores the screen", stuck / 1000);
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
