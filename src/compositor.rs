//! The compositor: X events in, frames out.

use crate::gpu::present::Swapchain;
use crate::gpu::render::{Quad, Renderer};
use crate::gpu::texture::{DmaBuf, Texture};
use crate::gpu::{Buffer, Gpu, GpuOptions, image_barrier};
use crate::shm::ImageReader;
use crate::windows::{Geometry, Source, Win, WinList};
use crate::x::{X, error_is_benign};
use crate::zoom::Zoom;
use anyhow::{Context, Result};
use ash::vk;
use std::collections::VecDeque;
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, GetGeometryReply, GetWindowAttributesReply, MapState, Place, Property, Window,
    WindowClass,
};
use x11rb::xcb_ffi::XCBConnection;

pub struct Options {
    pub display: Option<String>,
    pub gpu: Option<String>,
    pub dri3: bool,
    pub sync_fence: bool,
    pub validation: bool,
    pub max_fps: f64,
    pub background: [f32; 4],
}

const FRAMES: usize = 2;
/// How often the pointer is polled while zoomed and idle.
const POINTER_POLL: Duration = Duration::from_millis(8);

struct Frame {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    acquired: vk::Semaphore,
    staging: Option<Buffer>,
    /// The last frame number submitted in this slot (0: none).
    number: u64,
}

enum Garbage {
    Texture(Texture),
    Buffer(Buffer),
}

struct Background {
    pixmap: u32,
    tex: Texture,
    source: Source,
    dirty: bool,
}

pub struct Compositor {
    x: X,
    /// The libxcb connection the Vulkan surface presents through. A separate one,
    /// so the driver's presentation traffic never interleaves with our events.
    wsi: XCBConnection,
    gpu: ManuallyDrop<Gpu>,
    renderer: Renderer,
    swapchain: Option<Swapchain>,
    /// The scene composed 1:1, sampled by the zoom pass.
    scene: Option<Texture>,
    frames: Vec<Frame>,
    frame_no: u64,
    completed: u64,
    garbage: VecDeque<(u64, Garbage)>,
    wins: WinList,
    reader: ImageReader,
    background: Option<Background>,
    clear: [f32; 4],
    zoom: Zoom,
    pointer: (i16, i16),
    fence: Option<u32>,
    use_dri3: bool,
    dirty: bool,
    quit: bool,
    last_frame: Option<Instant>,
    min_interval: Duration,
    warned_dri3: bool,
}

fn geometry(g: &GetGeometryReply) -> Geometry {
    Geometry { x: g.x, y: g.y, width: g.width, height: g.height, border: g.border_width }
}

impl Compositor {
    pub fn new(opts: &Options) -> Result<Compositor> {
        let mut x = X::connect(opts.display.as_deref())?;
        x.become_compositor()?;
        let mut c = match Self::with_x(x, opts) {
            Ok(c) => c,
            Err((mut x, e)) => {
                x.release_compositor();
                return Err(e);
            }
        };
        // From here on Drop undoes everything, including the X state.
        c.init()?;
        Ok(c)
    }

    fn with_x(x: X, opts: &Options) -> std::result::Result<Compositor, (X, anyhow::Error)> {
        let setup = (|| -> Result<_> {
            let name = opts.display.as_deref().map(CString::new).transpose()?;
            let (wsi, _) = XCBConnection::connect(name.as_deref()).context("second X connection for Vulkan")?;
            let use_dri3 = opts.dri3 && x.features.dri3;
            let gpu = Gpu::new(&GpuOptions {
                present: Some((wsi.get_raw_xcb_connection(), x.overlay)),
                want_dmabuf: use_dri3,
                device_filter: opts.gpu.as_deref(),
                validation: opts.validation,
            })?;
            Ok((wsi, gpu))
        })();
        let (wsi, gpu) = match setup {
            Ok(v) => v,
            Err(e) => return Err((x, e)),
        };
        let use_dri3 = opts.dri3 && x.features.dri3 && gpu.mem_fd.is_some();
        let gpu = ManuallyDrop::new(gpu);
        let rest = (|| -> Result<_> {
            let extent = vk::Extent2D { width: x.width as u32, height: x.height as u32 };
            let swapchain = Swapchain::new(&gpu, extent, None)?;
            let renderer = Renderer::new(&gpu, swapchain.format)?;
            let mut frames = Vec::new();
            for _ in 0..FRAMES {
                let cmd = unsafe {
                    gpu.device.allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default()
                            .command_pool(gpu.cmd_pool)
                            .level(vk::CommandBufferLevel::PRIMARY)
                            .command_buffer_count(1),
                    )?[0]
                };
                let fence = unsafe {
                    gpu.device
                        .create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)?
                };
                let acquired = unsafe { gpu.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
                frames.push(Frame { cmd, fence, acquired, staging: None, number: 0 });
            }
            Ok((swapchain, renderer, frames))
        })();
        let (swapchain, renderer, frames) = match rest {
            Ok(v) => v,
            Err(e) => {
                ManuallyDrop::into_inner(gpu);
                return Err((x, e));
            }
        };
        let fence = if use_dri3 && opts.sync_fence && x.features.sync {
            x.create_fence().ok()
        } else {
            None
        };
        log::info!(
            "window contents via {}{}",
            if use_dri3 { "DRI3 dma-buf import" } else if x.features.shm_fd { "MIT-SHM copies" } else { "GetImage copies" },
            if fence.is_some() { ", X Sync fence before sampling" } else { "" }
        );
        let reader = ImageReader::new(x.features.shm_fd);
        Ok(Compositor {
            x,
            wsi,
            gpu,
            renderer,
            swapchain: Some(swapchain),
            scene: None,
            frames,
            frame_no: 1,
            completed: 0,
            garbage: VecDeque::new(),
            wins: WinList::new(),
            reader,
            background: None,
            clear: opts.background,
            zoom: Zoom::new(),
            pointer: (0, 0),
            fence,
            use_dri3,
            dirty: true,
            quit: false,
            last_frame: None,
            min_interval: Duration::from_secs_f64(1.0 / opts.max_fps.max(1.0)),
            warned_dri3: false,
        })
    }

    fn init(&mut self) -> Result<()> {
        self.create_scene()?;
        for (w, attrs, geom) in self.x.scan()? {
            self.add_window(w, &attrs, &geom);
        }
        log::info!("{} top-level windows", self.wins.len());
        self.refresh_background();
        let z = Zoom::from_property(self.x.root_cardinal(self.x.atoms.picom_zoom, AtomEnum::CARDINAL));
        self.zoom.set_target(z, Instant::now());
        self.x.conn.flush()?;
        Ok(())
    }

    fn create_scene(&mut self) -> Result<()> {
        if let Some(old) = self.scene.take() {
            self.defer(Garbage::Texture(old));
        }
        let sc = self.swapchain.as_ref().unwrap();
        self.scene = Some(self.gpu.create_texture(
            sc.extent.width,
            sc.extent.height,
            sc.format,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            false,
        )?);
        Ok(())
    }

    fn defer(&mut self, g: Garbage) {
        self.garbage.push_back((self.frame_no, g));
    }

    fn collect_garbage(&mut self) {
        while let Some((n, _)) = self.garbage.front() {
            if *n > self.completed + 1 {
                break;
            }
            match self.garbage.pop_front().unwrap().1 {
                Garbage::Texture(t) => t.destroy(&self.gpu),
                Garbage::Buffer(b) => b.destroy(&self.gpu),
            }
        }
    }

    fn add_window(&mut self, id: Window, attrs: &GetWindowAttributesReply, geom: &GetGeometryReply) {
        let mut w = Win::new(id, geometry(geom), geom.depth);
        w.input_only = attrs.class == WindowClass::INPUT_ONLY;
        w.override_redirect = attrs.override_redirect;
        w.ignored = id == self.x.overlay || id == self.x.owner;
        w.mapped = attrs.map_state != MapState::UNMAPPED;
        if !w.input_only && !w.ignored {
            w.damage = self.x.create_damage(id).ok();
        }
        if w.mapped {
            w.pixmap_stale = true;
            self.dirty = true;
        }
        log::debug!(
            "add 0x{id:x} {}x{}+{}+{} depth {}{}{}",
            w.geom.width,
            w.geom.height,
            w.geom.x,
            w.geom.y,
            w.depth,
            if w.mapped { " mapped" } else { "" },
            if w.ignored { " (ignored)" } else { "" }
        );
        self.wins.insert_top(w);
    }

    /// Frees what an unmapped window no longer needs; the damage object stays.
    fn release_contents(&mut self, id: Window) {
        let Some(w) = self.wins.get_mut(id) else { return };
        let pixmap = w.pixmap.take();
        let tex = w.tex.take();
        w.source = None;
        if let Some(p) = pixmap {
            let _ = self.x.conn.free_pixmap(p).map(|c| c.ignore_error());
        }
        if let Some(t) = tex {
            self.defer(Garbage::Texture(t));
        }
    }

    fn forget_window(&mut self, id: Window, destroyed: bool) {
        self.release_contents(id);
        if let Some(w) = self.wins.remove(id) {
            if let (Some(d), false) = (w.damage, destroyed) {
                // A destroyed window took its damage object with it.
                let _ = self.x.conn.damage_destroy(d).map(|c| c.ignore_error());
            }
            if w.drawable() {
                self.dirty = true;
            }
            log::debug!("forget 0x{id:x}");
        }
    }

    fn handle(&mut self, ev: Event) -> Result<()> {
        let root = self.x.root;
        match ev {
            Event::CreateNotify(e) if e.parent == root => {
                if let Some((a, g)) = self.x.query_window(e.window) {
                    self.add_window(e.window, &a, &g);
                }
            }
            Event::DestroyNotify(e) => self.forget_window(e.window, true),
            Event::MapNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    w.mapped = true;
                    w.pixmap_stale = true;
                    self.dirty |= w.drawable();
                }
            }
            Event::UnmapNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    self.dirty |= w.drawable();
                    w.mapped = false;
                    self.release_contents(e.window);
                }
            }
            Event::ConfigureNotify(e) if e.window == root => {
                log::info!("screen resized to {}x{}", e.width, e.height);
                self.x.width = e.width;
                self.x.height = e.height;
                self.recreate_swapchain()?;
            }
            Event::ConfigureNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    let g = Geometry { x: e.x, y: e.y, width: e.width, height: e.height, border: e.border_width };
                    if g.outer_size() != w.geom.outer_size() && w.mapped {
                        w.pixmap_stale = true;
                    }
                    w.geom = g;
                    w.override_redirect = e.override_redirect;
                    self.dirty |= w.drawable();
                    if !self.wins.restack(e.window, e.above_sibling) {
                        log::debug!("0x{:x} restacked above unknown 0x{:x}", e.window, e.above_sibling);
                    }
                }
            }
            Event::ReparentNotify(e) => {
                if e.parent == root {
                    if let Some((a, g)) = self.x.query_window(e.window) {
                        self.add_window(e.window, &a, &g);
                    }
                } else {
                    self.forget_window(e.window, false);
                }
            }
            Event::CirculateNotify(e) => {
                if e.place == Place::ON_TOP {
                    self.wins.raise(e.window);
                } else {
                    self.wins.lower(e.window);
                }
                self.dirty = true;
            }
            Event::GravityNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    w.geom.x = e.x;
                    w.geom.y = e.y;
                    self.dirty |= w.drawable();
                }
            }
            Event::DamageNotify(e) => {
                self.x.conn.damage_subtract(e.damage, x11rb::NONE, x11rb::NONE)?;
                if let Some(w) = self.wins.get_mut(e.drawable) {
                    w.content_dirty = true;
                    self.dirty |= w.drawable();
                }
            }
            Event::PropertyNotify(e) if e.window == root => {
                let a = &self.x.atoms;
                if e.atom == a.picom_zoom {
                    let v = if e.state == Property::DELETE {
                        None
                    } else {
                        self.x.root_cardinal(e.atom, AtomEnum::CARDINAL)
                    };
                    let z = Zoom::from_property(v);
                    log::debug!("zoom target {z:.3}");
                    self.zoom.set_target(z, Instant::now());
                    self.dirty = true;
                } else if e.atom == a.xrootpmap_id || e.atom == a.esetroot_pmap_id {
                    self.refresh_background();
                }
            }
            Event::SelectionClear(e) if e.selection == self.x.atoms.cm_selection => {
                log::warn!("another compositor took over the screen; exiting");
                // It owns the selection now; we must not touch the overlay or redirection.
                self.x.owner = x11rb::NONE;
                self.quit = true;
            }
            Event::Error(e) => {
                if error_is_benign(&e) {
                    log::debug!("X error {:?} (request {}.{})", e.error_kind, e.major_opcode, e.minor_opcode);
                } else {
                    log::warn!("X error {:?} (request {}.{})", e.error_kind, e.major_opcode, e.minor_opcode);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn refresh_background(&mut self) {
        self.dirty = true;
        let pixmap = self.x.root_pixmap();
        if let (Some(bg), Some(p)) = (self.background.as_mut(), pixmap) {
            if bg.pixmap == p {
                bg.dirty = bg.source == Source::Shm;
                return;
            }
        }
        if let Some(bg) = self.background.take() {
            self.defer(Garbage::Texture(bg.tex));
        }
        let Some(pixmap) = pixmap else {
            log::info!("no root pixmap; solid background");
            return;
        };
        let Ok(g) = self.x.conn.get_geometry(pixmap).map_err(anyhow::Error::from).and_then(|c| Ok(c.reply()?)) else {
            log::warn!("root pixmap 0x{pixmap:x} is gone");
            return;
        };
        match self.import_pixmap(pixmap, g.width as u32 + 2 * g.border_width as u32, g.height as u32, g.depth) {
            Ok((tex, source)) => {
                log::info!("background pixmap 0x{pixmap:x} {}x{} via {source:?}", tex.width, tex.height);
                self.background = Some(Background { pixmap, tex, source, dirty: source == Source::Shm });
            }
            Err(e) => log::warn!("cannot use the root pixmap: {e:#}"),
        }
    }

    /// Imports a pixmap through DRI3 when possible, else makes a texture that
    /// CPU copies fill (the caller schedules the first copy).
    fn import_pixmap(&mut self, pixmap: u32, width: u32, height: u32, depth: u8) -> Result<(Texture, Source)> {
        if self.use_dri3 {
            let r = self.x.buffers_from_pixmap(pixmap).and_then(|mut r| {
                if r.buffers.len() != 1 {
                    anyhow::bail!("{} planes", r.buffers.len());
                }
                self.gpu.import_dmabuf(DmaBuf {
                    fd: r.buffers.remove(0),
                    width: r.width as u32,
                    height: r.height as u32,
                    stride: r.strides[0],
                    offset: r.offsets[0],
                    modifier: r.modifier,
                    depth: r.depth,
                    bpp: r.bpp,
                })
            });
            match r {
                Ok(t) => return Ok((t, Source::DmaBuf)),
                Err(e) => {
                    if !self.warned_dri3 {
                        log::warn!("DRI3 import failed, copying through MIT-SHM instead: {e:#}");
                        self.warned_dri3 = true;
                    }
                }
            }
        }
        if !(depth == 24 || depth == 32) {
            anyhow::bail!("unsupported depth {depth}");
        }
        Ok((self.gpu.create_upload_texture(width, height, depth)?, Source::Shm))
    }

    /// Names pixmaps for windows that were just mapped or resized.
    fn refresh_pixmaps(&mut self) {
        let stale: Vec<Window> = self.wins.iter().filter(|w| w.drawable() && w.pixmap_stale).map(|w| w.id).collect();
        for id in stale {
            self.release_contents(id);
            let w = self.wins.get_mut(id).unwrap();
            w.pixmap_stale = false;
            let depth = w.depth;
            let Ok(pixmap) = self.x.conn.generate_id() else { continue };
            let named = self.x.conn.composite_name_window_pixmap(id, pixmap).map(|c| c.check());
            if !matches!(named, Ok(Ok(()))) {
                // Unmapped or destroyed meanwhile; its events are on the way.
                continue;
            }
            // The window may have been resized again since the event we know of.
            let size = self.x.conn.get_geometry(pixmap).ok().and_then(|c| c.reply().ok());
            let Some(size) = size else { continue };
            let result = self.import_pixmap(pixmap, size.width as u32, size.height as u32, depth);
            let w = self.wins.get_mut(id).unwrap();
            w.pixmap = Some(pixmap);
            match result {
                Ok((tex, source)) => {
                    w.content_dirty = true;
                    w.source = Some(source);
                    w.tex = Some(tex);
                }
                Err(e) => log::warn!("window 0x{id:x}: {e:#}"),
            }
        }
    }

    /// Copies damaged SHM-backed contents into the frame's staging buffer.
    /// Returns (window or None for the background, buffer offset) per copy.
    fn gather_uploads(&mut self, slot: usize) -> Result<Vec<(Option<Window>, u64)>> {
        let align = |n: u64| (n + 255) & !255;
        let mut jobs: Vec<(Option<Window>, u32, u32, u32)> = Vec::new();
        if let Some(bg) = &self.background {
            if bg.dirty {
                jobs.push((None, bg.pixmap, bg.tex.width, bg.tex.height));
            }
        }
        for w in self.wins.iter() {
            if w.drawable() && w.content_dirty && w.source == Some(Source::Shm) {
                if let (Some(p), Some(t)) = (w.pixmap, &w.tex) {
                    jobs.push((Some(w.id), p, t.width, t.height));
                }
            }
        }
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let total: u64 = jobs.iter().map(|j| align(j.2 as u64 * j.3 as u64 * 4)).sum();
        if self.frames[slot].staging.as_ref().is_none_or(|b| b.size < total) {
            if let Some(old) = self.frames[slot].staging.take() {
                self.defer(Garbage::Buffer(old));
            }
            let size = total.next_power_of_two().max(1 << 22);
            self.frames[slot].staging = Some(self.gpu.host_buffer(size, vk::BufferUsageFlags::TRANSFER_SRC)?);
        }
        let staging = self.frames[slot].staging.as_ref().unwrap();
        let mut out = Vec::new();
        let mut offset = 0u64;
        for (who, pixmap, w, h) in jobs {
            let len = w as usize * h as usize * 4;
            let dst = unsafe { staging.ptr.add(offset as usize) };
            let r = self.reader.read(&self.x.conn, pixmap, w as u16, h as u16, |data| unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), dst, len);
            });
            match r {
                Ok(()) => {
                    out.push((who, offset));
                    offset += align(len as u64);
                }
                Err(e) => log::debug!("copy of pixmap 0x{pixmap:x} failed: {e:#}"),
            }
        }
        Ok(out)
    }

    fn recreate_swapchain(&mut self) -> Result<()> {
        unsafe { self.gpu.device.device_wait_idle()? };
        let extent = vk::Extent2D { width: self.x.width as u32, height: self.x.height as u32 };
        let old = self.swapchain.take();
        self.swapchain = Some(Swapchain::new(&self.gpu, extent, old)?);
        self.create_scene()?;
        self.dirty = true;
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        let now = Instant::now();
        self.zoom.advance(now);
        let zoomed = self.zoom.active();
        if zoomed {
            if let Some(p) = self.x.pointer() {
                self.pointer = p;
            }
        }

        let slot = (self.frame_no % FRAMES as u64) as usize;
        let fence = self.frames[slot].fence;
        unsafe { self.gpu.device.wait_for_fences(&[fence], true, u64::MAX)? };
        self.completed = self.completed.max(self.frames[slot].number);
        self.collect_garbage();

        self.refresh_pixmaps();
        let uploads = self.gather_uploads(slot)?;
        let dmabuf_damaged = self
            .wins
            .iter()
            .any(|w| w.drawable() && w.content_dirty && w.source == Some(Source::DmaBuf));
        if let (Some(f), true) = (self.fence, dmabuf_damaged) {
            self.x.fence_sync(f)?;
        }
        for w in self.wins.iter_mut() {
            w.content_dirty = false;
        }
        if let Some(bg) = self.background.as_mut() {
            bg.dirty = false;
        }

        let sc = self.swapchain.as_ref().unwrap();
        let swfn = self.gpu.swapchain_fn.as_ref().unwrap();
        let acquired = self.frames[slot].acquired;
        let index = match unsafe { swfn.acquire_next_image(sc.swapchain, u64::MAX, acquired, vk::Fence::null()) } {
            Ok((i, _)) => i,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                // Pixels copied above are lost with this frame, so copy them again.
                self.mark_all_dirty();
                return self.recreate_swapchain();
            }
            Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
        };

        let gpu = &*self.gpu;
        let dev = &gpu.device;
        let cmd = self.frames[slot].cmd;
        unsafe {
            dev.reset_fences(&[fence])?;
            dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        let staging = self.frames[slot].staging.as_ref();
        for (who, offset) in uploads {
            let tex = match who {
                None => self.background.as_mut().map(|b| &mut b.tex),
                Some(id) => self.wins.get_mut(id).and_then(|w| w.tex.as_mut()),
            };
            if let (Some(tex), Some(buf)) = (tex, staging) {
                gpu.record_upload(cmd, tex, buf, offset);
            }
        }

        let mut quads = Vec::new();
        if let Some(bg) = self.background.as_ref().filter(|b| b.tex.ready) {
            quads.push(Quad {
                tex: &bg.tex,
                x: 0.0,
                y: 0.0,
                width: sc.extent.width as f32,
                height: sc.extent.height as f32,
                tiled: true,
                opacity: 1.0,
            });
        }
        for w in self.wins.iter().filter(|w| w.drawable()) {
            let Some(tex) = w.tex.as_ref().filter(|t| t.ready) else { continue };
            quads.push(Quad {
                tex,
                x: w.geom.x as f32,
                y: w.geom.y as f32,
                width: tex.width as f32,
                height: tex.height as f32,
                tiled: false,
                opacity: 1.0,
            });
        }
        let mut external: Vec<&Texture> = Vec::new();
        for q in quads.iter().filter(|q| q.tex.external) {
            if !external.iter().any(|t| t.image == q.tex.image) {
                external.push(q.tex);
            }
        }
        for t in &external {
            gpu.record_acquire(cmd, t);
        }

        let image = sc.images[index as usize];
        let view = sc.views[index as usize];
        let to_attachment = (
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        );
        image_barrier(
            gpu,
            cmd,
            image,
            (vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT, vk::AccessFlags2::NONE, vk::ImageLayout::UNDEFINED),
            to_attachment,
            None,
        );
        if zoomed {
            let scene = self.scene.as_ref().unwrap();
            image_barrier(
                gpu,
                cmd,
                scene.image,
                (vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::NONE, vk::ImageLayout::UNDEFINED),
                to_attachment,
                None,
            );
            self.renderer.record_scene(gpu, cmd, scene.view, sc.extent, self.clear, &quads);
            image_barrier(
                gpu,
                cmd,
                scene.image,
                to_attachment,
                (
                    vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                ),
                None,
            );
            let p = (self.pointer.0 as f64, self.pointer.1 as f64);
            self.renderer
                .record_zoom(gpu, cmd, view, sc.extent, scene, self.zoom.current(), self.zoom.origin(p));
        } else {
            self.renderer.record_scene(gpu, cmd, view, sc.extent, self.clear, &quads);
        }
        image_barrier(
            gpu,
            cmd,
            image,
            to_attachment,
            (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE, vk::ImageLayout::PRESENT_SRC_KHR),
            None,
        );
        for t in &external {
            gpu.record_release(cmd, t);
        }
        unsafe { dev.end_command_buffer(cmd)? };

        let rendered = sc.rendered[index as usize];
        let waits = [vk::SemaphoreSubmitInfo::default()
            .semaphore(acquired)
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(rendered)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        let submit = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&waits)
            .command_buffer_infos(&cmds)
            .signal_semaphore_infos(&signals)];
        unsafe { dev.queue_submit2(gpu.queue, &submit, fence)? };
        self.frames[slot].number = self.frame_no;
        self.frame_no += 1;

        let swapchains = [sc.swapchain];
        let indices = [index];
        let wait = [rendered];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&wait)
            .swapchains(&swapchains)
            .image_indices(&indices);
        match unsafe { swfn.queue_present(gpu.queue, &present) } {
            Ok(false) => {}
            Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.recreate_swapchain()?,
            Err(e) => return Err(e).context("vkQueuePresentKHR"),
        }
        Ok(())
    }

    fn mark_all_dirty(&mut self) {
        for w in self.wins.iter_mut() {
            w.content_dirty = true;
        }
        if let Some(bg) = self.background.as_mut() {
            bg.dirty = bg.source == Source::Shm;
        }
    }

    /// Runs until a signal arrives on `sigfd` or another compositor takes over.
    pub fn run(&mut self, sigfd: RawFd) -> Result<()> {
        let xfd = self.x.conn.as_raw_fd();
        loop {
            while let Some(ev) = self.x.conn.poll_for_event()? {
                self.handle(ev)?;
            }
            if self.quit {
                return Ok(());
            }
            let now = Instant::now();
            let mut timeout: Option<Duration> = None;
            if self.zoom.active() {
                if let Some(p) = self.x.pointer() {
                    if p != self.pointer {
                        self.pointer = p;
                        self.dirty = true;
                    }
                }
                timeout = Some(POINTER_POLL);
            }
            if self.zoom.animating() {
                self.dirty = true;
            }
            if self.dirty {
                let wait = self.last_frame.map_or(Duration::ZERO, |t| self.min_interval.saturating_sub(now - t));
                if wait.is_zero() {
                    self.dirty = false;
                    self.last_frame = Some(now);
                    self.render()?;
                    continue;
                }
                timeout = Some(timeout.map_or(wait, |t| t.min(wait)));
            }
            self.x.conn.flush()?;
            let mut fds = [
                libc::pollfd { fd: xfd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: sigfd, events: libc::POLLIN, revents: 0 },
            ];
            let ms = timeout.map_or(-1, |t| t.as_micros().div_ceil(1000) as i32);
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e).context("poll");
                }
            }
            if fds[1].revents != 0 {
                let mut info = [0u8; 128];
                unsafe { libc::read(sigfd, info.as_mut_ptr().cast(), info.len()) };
                let signo = u32::from_ne_bytes(info[..4].try_into().unwrap());
                log::info!("signal {signo}, shutting down");
                return Ok(());
            }
        }
    }

    fn teardown_gpu(&mut self) {
        let gpu = &*self.gpu;
        unsafe {
            let _ = gpu.device.device_wait_idle();
        }
        for mut w in self.wins.drain() {
            if let Some(t) = w.tex.take() {
                t.destroy(gpu);
            }
            if let Some(p) = w.pixmap.take() {
                let _ = self.x.conn.free_pixmap(p).map(|c| c.ignore_error());
            }
            if let Some(d) = w.damage.take() {
                let _ = self.x.conn.damage_destroy(d).map(|c| c.ignore_error());
            }
        }
        if let Some(bg) = self.background.take() {
            bg.tex.destroy(gpu);
        }
        for (_, g) in self.garbage.drain(..) {
            match g {
                Garbage::Texture(t) => t.destroy(gpu),
                Garbage::Buffer(b) => b.destroy(gpu),
            }
        }
        for f in self.frames.drain(..) {
            unsafe {
                gpu.device.destroy_fence(f.fence, None);
                gpu.device.destroy_semaphore(f.acquired, None);
                gpu.device.free_command_buffers(gpu.cmd_pool, &[f.cmd]);
            }
            if let Some(b) = f.staging {
                b.destroy(gpu);
            }
        }
        if let Some(s) = self.scene.take() {
            s.destroy(gpu);
        }
        if let Some(sc) = self.swapchain.take() {
            sc.destroy(gpu);
        }
        self.renderer.destroy(gpu);
        // Drops the device and the surface, which must go before the overlay window.
        unsafe { ManuallyDrop::drop(&mut self.gpu) };
        self.reader.release(&self.x.conn);
        if let Some(f) = self.fence.take() {
            use x11rb::protocol::sync::ConnectionExt as _;
            let _ = self.x.conn.sync_destroy_fence(f).map(|c| c.ignore_error());
        }
        let _ = &self.wsi;
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.teardown_gpu();
        self.x.release_compositor();
        log::info!("released the screen");
    }
}
