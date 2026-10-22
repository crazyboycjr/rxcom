//! Component test of the DRI3 path on a real X server with DRI3 1.2. It never
//! takes the compositing selection, never redirects the root's children and
//! never presents: it creates an override-redirect top-level far off-screen
//! with a child it redirects itself, draws known pixels into the child, imports
//! the child's pixmap into Vulkan as a dma-buf, renders it into an offscreen
//! image and compares what it reads back.
//!
//! `--sync` adds a synchronization probe: queue full-window fills with X, then
//! sample at once, with and without an X Sync fence, and check whether the
//! dma-buf carries an implicit fence. Careful: on 2026-10-22 the heavy setting
//! (300 fills of 2048x2048 per iteration, 40 iterations) stalled the NVIDIA X
//! server for about five minutes while a training job saturated the GPU. Do not
//! point this at a desktop someone is using.
//!
//!   DISPLAY=:N cargo run --example dri3_import [-- --sync --iterations N --fills K]

use anyhow::{Context, Result, bail};
use rxcom::gpu::render::{Quad, Renderer, render_offscreen};
use rxcom::gpu::texture::{DmaBuf, PIXMAP_FORMAT, Texture};
use rxcom::gpu::{Gpu, GpuOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;
use x11rb::connection::Connection;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::dri3::ConnectionExt as _;
use x11rb::protocol::sync::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ColormapAlloc, ConnectionExt as _, CreateGCAux, CreateWindowAux, ImageFormat, Rectangle, VisualClass,
    WindowClass,
};
use x11rb::xcb_ffi::XCBConnection;

struct TestWindow {
    top: u32,
    child: u32,
    gc: u32,
    depth: u8,
    w: u16,
    h: u16,
}

fn create(conn: &XCBConnection, screen: usize, w: u16, h: u16, depth: u8) -> Result<TestWindow> {
    let s = &conn.setup().roots[screen];
    let top = conn.generate_id()?;
    // Far off-screen and override-redirect, so the window manager ignores it and
    // nothing ever shows on the user's screen.
    conn.create_window(
        s.root_depth,
        top,
        s.root,
        -6000,
        -6000,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        s.root_visual,
        &CreateWindowAux::new().background_pixel(s.black_pixel).override_redirect(1),
    )?;
    let child = conn.generate_id()?;
    let mut aux = CreateWindowAux::new().background_pixel(0).border_pixel(0);
    let mut visual = s.root_visual;
    if depth != s.root_depth {
        visual = s
            .allowed_depths
            .iter()
            .filter(|d| d.depth == depth)
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.class == VisualClass::TRUE_COLOR)
            .context("no visual of that depth")?
            .visual_id;
        let cmap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, cmap, s.root, visual)?;
        aux = aux.colormap(cmap);
    }
    conn.create_window(depth, child, top, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &aux)?;
    conn.composite_redirect_window(child, Redirect::MANUAL)?.check()?;
    conn.map_window(child)?;
    conn.map_window(top)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, child, &CreateGCAux::new())?;
    conn.get_input_focus()?.reply()?;
    Ok(TestWindow { top, child, gc, depth, w, h })
}

fn pattern(x: u32, y: u32, depth: u8) -> u32 {
    let rgb = ((x & 0xff) << 16) | ((y & 0xff) << 8) | ((x * 7 + y * 13) & 0xff);
    if depth == 32 { (((x + 3 * y) & 0xff) << 24) | rgb } else { 0xff00_0000 | rgb }
}

fn put_pattern(conn: &XCBConnection, t: &TestWindow) -> Result<()> {
    let rows_per = (60000 / (t.w as usize * 4)).max(1) as u16;
    let mut y0 = 0;
    while y0 < t.h {
        let rows = rows_per.min(t.h - y0);
        let mut data = Vec::with_capacity(t.w as usize * rows as usize * 4);
        for y in y0..y0 + rows {
            for x in 0..t.w {
                data.extend_from_slice(&pattern(x as u32, y as u32, t.depth).to_le_bytes());
            }
        }
        conn.put_image(ImageFormat::Z_PIXMAP, t.child, t.gc, t.w, rows, 0, y0 as i16, 0, t.depth, &data)?;
        y0 += rows;
    }
    conn.get_input_focus()?.reply()?;
    Ok(())
}

/// Names the child's pixmap and exports it. Returns the pixmap, the import
/// description, and a duplicate of the dma-buf fd for sync-file probing.
fn export(conn: &XCBConnection, t: &TestWindow) -> Result<(u32, DmaBuf, OwnedFd)> {
    let pixmap = conn.generate_id()?;
    conn.composite_name_window_pixmap(t.child, pixmap)?.check()?;
    let mut r = conn.dri3_buffers_from_pixmap(pixmap)?.reply().context("DRI3 BuffersFromPixmap")?;
    if r.buffers.len() != 1 {
        bail!("{} planes", r.buffers.len());
    }
    let fd = r.buffers.remove(0);
    let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    println!(
        "  exported {}x{} depth {} bpp {}: modifier 0x{:016x} stride {} offset {} (dma-buf {} bytes)",
        r.width, r.height, r.depth, r.bpp, r.modifier, r.strides[0], r.offsets[0], size
    );
    let dup = unsafe { OwnedFd::from_raw_fd(libc::dup(fd.as_raw_fd())) };
    let buf = DmaBuf {
        fd,
        width: r.width as u32,
        height: r.height as u32,
        stride: r.strides[0],
        offset: r.offsets[0],
        modifier: r.modifier,
        depth: r.depth,
        bpp: r.bpp,
    };
    Ok((pixmap, buf, dup))
}

fn quad(tex: &Texture, x: f32, y: f32) -> Quad<'_> {
    Quad { tex, x, y, width: tex.width as f32, height: tex.height as f32, tiled: false, opacity: 1.0 }
}

/// Samples single texels through the compositor's pipeline (one 1x1 render each).
fn sample(gpu: &Gpu, r: &Renderer, tex: &Texture, points: &[(u32, u32)]) -> Result<Vec<u32>> {
    points
        .iter()
        .map(|&(x, y)| {
            let px = render_offscreen(gpu, r, 1, 1, [0.0; 4], &[quad(tex, -(x as f32), -(y as f32))])?;
            Ok(u32::from_le_bytes(px[..4].try_into().unwrap()))
        })
        .collect()
}

const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
const DMA_BUF_SYNC_READ: u32 = 1;

/// Exports the dma-buf's implicit write fence as a sync file. Some(true) when it
/// is still pending, Some(false) when already signalled, None when unsupported.
fn implicit_fence_pending(fd: &OwnedFd) -> Option<bool> {
    #[repr(C)]
    struct ExportSyncFile {
        flags: u32,
        fd: i32,
    }
    let mut arg = ExportSyncFile { flags: DMA_BUF_SYNC_READ, fd: -1 };
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut arg) };
    if r != 0 {
        return None;
    }
    let mut p = libc::pollfd { fd: arg.fd, events: libc::POLLIN, revents: 0 };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    unsafe { libc::close(arg.fd) };
    Some(n == 0)
}

fn main() -> Result<()> {
    let mut iterations = 10;
    let mut fills = 30;
    let mut sync = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--iterations" => iterations = args.next().context("value")?.parse()?,
            "--fills" => fills = args.next().context("value")?.parse()?,
            "--sync" => sync = true,
            _ => bail!("unknown option {a}"),
        }
    }
    rxcom::logger::init(0);
    if std::env::var_os("DISPLAY").is_none() {
        bail!("DISPLAY is not set");
    }
    let (conn, screen) = XCBConnection::connect(None)?;
    let v = conn.dri3_query_version(1, 2)?.reply()?;
    println!("DRI3 {}.{} on {}", v.major_version, v.minor_version, std::env::var("DISPLAY")?);
    if (v.major_version, v.minor_version) < (1, 2) {
        bail!("DRI3 1.2 is needed for BuffersFromPixmap");
    }
    let gpu = Gpu::new(&GpuOptions {
        present: None,
        want_dmabuf: true,
        device_filter: std::env::var("RXCOM_GPU").ok().as_deref(),
        validation: std::env::var_os("RXCOM_VALIDATION").is_some(),
    })?;
    if gpu.mem_fd.is_none() {
        bail!("{} cannot import dma-bufs", gpu.name);
    }
    let renderer = Renderer::new(&gpu, PIXMAP_FORMAT)?;

    println!("\n1. Import correctness: draw a pattern with X, sample it through Vulkan");
    let mut all_ok = true;
    // Kept alive so later pixmaps in the same buffer object reuse its import.
    let mut keep = Vec::new();
    for (w, h, depth) in [(256u16, 256u16, 24u8), (333, 211, 24), (1024, 768, 24), (300, 200, 32), (1920, 1080, 24)] {
        let t = create(&conn, screen, w, h, depth)?;
        put_pattern(&conn, &t)?;
        let (pixmap, buf, _dup) = export(&conn, &t)?;
        let start = Instant::now();
        let tex = gpu.import_dmabuf(buf)?;
        let import_ms = start.elapsed().as_secs_f64() * 1e3;
        let px = render_offscreen(&gpu, &renderer, w as u32, h as u32, [0.0; 4], &[quad(&tex, 0.0, 0.0)])?;
        let mut bad = 0;
        let mut first_bad = None;
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let i = ((y * w as u32 + x) * 4) as usize;
                let got = u32::from_le_bytes(px[i..i + 4].try_into().unwrap());
                let want = pattern(x, y, depth);
                if got != want {
                    bad += 1;
                    first_bad.get_or_insert((x, y, got, want));
                }
            }
        }
        all_ok &= bad == 0;
        println!(
            "  {w}x{h} depth {depth}: import {import_ms:.2} ms, {bad} of {} pixels differ{}",
            w as u32 * h as u32,
            first_bad.map_or(String::new(), |(x, y, g, e)| format!(" (first at {x},{y}: got {g:08x}, want {e:08x})"))
        );
        keep.push((tex, pixmap, t.top));
    }
    for (tex, pixmap, top) in keep {
        tex.destroy(&gpu);
        conn.free_pixmap(pixmap)?;
        conn.destroy_window(top)?;
    }

    if !sync {
        renderer.destroy(&gpu);
        println!("\nimport correctness: {}", if all_ok { "PASS" } else { "FAIL" });
        return Ok(());
    }
    println!("\n2. Synchronization: {fills} full-window fills per iteration, then sample at once");
    let t = create(&conn, screen, 2048, 2048, 24)?;
    let (pixmap, buf, dmabuf_fd) = export(&conn, &t)?;
    let tex = gpu.import_dmabuf(buf)?;
    let fence = conn.generate_id()?;
    conn.sync_create_fence(t.child, fence, false)?.check()?;
    let points = [(0, 0), (1024, 1024), (2047, 2047)];
    let full = [Rectangle { x: 0, y: 0, width: 2048, height: 2048 }];
    let mut colour = 0x10_2030u32;
    for mode in ["round trip only", "X Sync fence", "round trip only"] {
        let (mut stale, mut pending, mut probed) = (0, 0, 0);
        let mut wait_ms = 0.0;
        for i in 0..iterations {
            for k in 0..fills {
                colour = (colour.wrapping_mul(1_103_515_245).wrapping_add(12345 + k)) & 0xff_ffff;
                conn.change_gc(t.gc, &x11rb::protocol::xproto::ChangeGCAux::new().foreground(colour))?;
                conn.poly_fill_rectangle(t.child, t.gc, &full)?;
            }
            let want = 0xff00_0000 | colour;
            let start = Instant::now();
            conn.get_input_focus()?.reply()?;
            if mode == "X Sync fence" {
                conn.sync_trigger_fence(fence)?;
                conn.sync_await_fence(&[fence])?;
                conn.sync_reset_fence(fence)?;
                conn.get_input_focus()?.reply()?;
            }
            wait_ms += start.elapsed().as_secs_f64() * 1e3;
            if let Some(p) = implicit_fence_pending(&dmabuf_fd) {
                probed += 1;
                pending += p as u32;
            }
            let got = sample(&gpu, &renderer, &tex, &points)?;
            if got.iter().any(|&g| g != want) {
                stale += 1;
                if stale <= 2 {
                    println!("    iteration {i}: want {want:08x}, sampled {got:08x?}");
                }
            }
        }
        println!(
            "  {mode}: {stale} of {iterations} samples stale; X wait {:.2} ms per iteration; \
             implicit dma-buf fence pending in {pending} of {probed} probes",
            wait_ms / iterations as f64
        );
    }
    tex.destroy(&gpu);
    conn.sync_destroy_fence(fence)?;
    conn.free_pixmap(pixmap)?;
    conn.destroy_window(t.top)?;
    conn.get_input_focus()?.reply()?;
    renderer.destroy(&gpu);
    drop(gpu);
    println!("\nimport correctness: {}", if all_ok { "PASS" } else { "FAIL" });
    Ok(())
}
