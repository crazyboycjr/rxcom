//! Watches the active virtual terminal, so that a return to the X server's VT
//! repaints everything. The kernel notifies `/sys/class/tty/tty0/active`
//! (poll for POLLPRI) at every switch; the X server sends no event for it.

use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileExt;

pub struct Vt {
    file: File,
    x_vt: u32,
    active: Option<u32>,
}

fn read_active(file: &File) -> Option<u32> {
    let mut buf = [0u8; 16];
    let n = file.read_at(&mut buf, 0).ok()?;
    std::str::from_utf8(&buf[..n]).ok()?.trim().strip_prefix("tty")?.parse().ok()
}

impl Vt {
    pub fn open(x_vt: u32) -> Option<Vt> {
        let file = File::open("/sys/class/tty/tty0/active").ok()?;
        let active = read_active(&file);
        Some(Vt { file, x_vt, active })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Rereads the active VT after poll reported a change; true when the X
    /// server's VT has just come back to the front.
    pub fn returned(&mut self) -> bool {
        let now = read_active(&self.file);
        let back = now == Some(self.x_vt) && self.active != now;
        if now != self.active {
            log::info!("VT {} is now active (X runs on VT {})", now.map_or("?".into(), |n| n.to_string()), self.x_vt);
        }
        self.active = now;
        back
    }
}
