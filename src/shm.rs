//! Reading pixmap contents on the CPU, for X servers without DRI3 (Xvfb, Xvnc):
//! MIT-SHM GetImage into a memfd segment shared with the server, or plain
//! GetImage when MIT-SHM cannot pass file descriptors.

use anyhow::{Context, Result, bail};
use std::os::fd::{FromRawFd, OwnedFd};
use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::xcb_ffi::XCBConnection;

struct Segment {
    seg: u32,
    ptr: *mut u8,
    size: usize,
}

pub struct ImageReader {
    use_shm: bool,
    segment: Option<Segment>,
}

impl ImageReader {
    pub fn new(use_shm: bool) -> Self {
        ImageReader { use_shm, segment: None }
    }

    fn ensure_segment(&mut self, conn: &XCBConnection, size: usize) -> Result<&Segment> {
        if self.segment.as_ref().is_some_and(|s| s.size >= size) {
            return Ok(self.segment.as_ref().unwrap());
        }
        self.release(conn);
        let size = size.next_power_of_two().max(1 << 20);
        // SAFETY: plain libc calls; every result is checked before use.
        let (fd, ptr) = unsafe {
            let fd = libc::memfd_create(c"rxcom-shm".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), size as libc::off_t) < 0 {
                bail!("ftruncate: {}", std::io::Error::last_os_error());
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            );
            if ptr == libc::MAP_FAILED {
                bail!("mmap: {}", std::io::Error::last_os_error());
            }
            (fd, ptr as *mut u8)
        };
        let seg = conn.generate_id()?;
        // The server keeps its own reference; our fd closes once it is sent.
        conn.shm_attach_fd(seg, fd, false)?.check().context("MIT-SHM AttachFd")?;
        self.segment = Some(Segment { seg, ptr, size });
        Ok(self.segment.as_ref().unwrap())
    }

    /// Reads a 32-bpp ZPixmap image of the drawable and hands the tightly packed
    /// rows (width * 4 bytes each) to `consume`.
    pub fn read<R>(
        &mut self,
        conn: &XCBConnection,
        drawable: u32,
        width: u16,
        height: u16,
        consume: impl FnOnce(&[u8]) -> R,
    ) -> Result<R> {
        let len = width as usize * height as usize * 4;
        if self.use_shm {
            let s = self.ensure_segment(conn, len)?;
            let (seg, ptr) = (s.seg, s.ptr);
            let r = conn
                .shm_get_image(drawable, 0, 0, width, height, !0, ImageFormat::Z_PIXMAP.into(), seg, 0)?
                .reply()
                .context("ShmGetImage")?;
            if (r.size as usize) < len {
                bail!("ShmGetImage returned {} bytes, expected {len} (depth {})", r.size, r.depth);
            }
            // SAFETY: the segment is at least `len` bytes and the server finished
            // writing it before sending the reply.
            let data = unsafe { std::slice::from_raw_parts(ptr, len) };
            Ok(consume(data))
        } else {
            let r = conn
                .get_image(ImageFormat::Z_PIXMAP, drawable, 0, 0, width, height, !0)?
                .reply()
                .context("GetImage")?;
            if r.data.len() < len {
                bail!("GetImage returned {} bytes, expected {len}", r.data.len());
            }
            Ok(consume(&r.data[..len]))
        }
    }

    pub fn release(&mut self, conn: &XCBConnection) {
        if let Some(s) = self.segment.take() {
            let _ = conn.shm_detach(s.seg).map(|c| c.ignore_error());
            // SAFETY: the mapping came from mmap with this size and is unused now.
            unsafe { libc::munmap(s.ptr as *mut libc::c_void, s.size) };
        }
    }
}
