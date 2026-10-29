//! The compositor: X events in, frames out.

use crate::gpu::Gpu;
use crate::scene::{Background, Scene};
use crate::vt::Vt;
use crate::watchdog;
use crate::windows::{Geometry, Win, WinList};
use crate::x::{X, error_is_benign};
use crate::zoom::{View, Zoom};
use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::render::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::sync::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, GetGeometryReply, GetWindowAttributesReply, MapState, Place, Property, Window, WindowClass,
};

pub struct Options {
    pub display: Option<String>,
    /// Present with Vulkan when the X server can share the scene.
    pub vulkan: bool,
    /// Unredirect a full-screen opaque window after this long; None: never.
    pub unredirect_delay: Option<Duration>,
    pub linear: bool,
    /// None: the refresh rate of the fastest lit monitor.
    pub max_fps: Option<f64>,
    pub background: u32,
}

/// Without XInput 2 the pointer is polled while zoomed, as picom-zoom does;
/// with it, raw motion wakes rxcom, and the slow poll only catches warps.
const POINTER_POLL: Duration = Duration::from_millis(4);
const WARP_POLL: Duration = Duration::from_millis(100);
/// How long rxcom polls the X server's fence before drawing anyway.
const FENCE_LIMIT: Duration = Duration::from_millis(100);
const GPU_RETRIES: u32 = 3;
const STATS_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Stats {
    frames: u32,
    /// Frames that waited for a free swapchain image.
    no_image: u32,
    /// From the composition until the fence read triggered: summed, worst.
    fence_sum: Duration,
    fence_max: Duration,
    fence_timeouts: u32,
}

pub struct Compositor {
    x: X,
    display: Option<String>,
    wins: WinList,
    scene: Scene,
    background: Background,
    overlay_picture: u32,
    gpu: Option<Gpu>,
    /// Present with Vulkan whenever it works.
    vulkan: bool,
    /// GPU errors without a quiet minute in between, and the last one.
    gpu_failures: u32,
    last_failure: Option<Instant>,
    /// The fence that tells when the X server's GPU work is done.
    fence: u32,
    /// Since when no swapchain image has been free.
    blocked_since: Option<Instant>,
    zoom: Zoom,
    linear: bool,
    pointer: [i16; 2],
    next_pointer_poll: Instant,
    last_pointer_query: Instant,
    pointer_moved: bool,
    watching_pointer: bool,
    /// The zoom view on screen (None: unzoomed).
    shown: Option<View>,
    /// The overlay lost its contents (exposed, remapped, VT switch): copy everything.
    full_present: bool,
    unredirect_delay: Option<Duration>,
    /// A full-screen window that may get the screen, and since when.
    candidate: Option<(Window, Instant)>,
    /// The window the screen is unredirected for.
    unredirected: Option<Window>,
    vt: Option<Vt>,
    quit: bool,
    last_frame: Option<Instant>,
    max_fps: Option<f64>,
    min_interval: Duration,
    stats: Stats,
    stats_since: Instant,
}

/// Reads one signal from the non-blocking signalfd, if any arrived.
fn pending_signal(sigfd: RawFd) -> Option<u32> {
    let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
    let n = unsafe { libc::read(sigfd, info.as_mut_ptr().cast(), info.len()) };
    (n == info.len() as isize).then(|| u32::from_ne_bytes(info[..4].try_into().unwrap()))
}

/// The shortest time between frames: the user's limit, else the fastest lit
/// monitor's refresh period, else 60 Hz's (a server without RandR monitors).
fn frame_interval(x: &X, max_fps: Option<f64>) -> Duration {
    let fps = max_fps.or_else(|| x.max_refresh()).unwrap_or(60.0);
    Duration::from_secs_f64(1.0 / fps.max(1.0))
}

/// The Vulkan presenter and the scene pixmap it shares, or None when Vulkan
/// cannot present here and the X server will.
fn try_gpu(x: &X, display: Option<&str>) -> (Option<Gpu>, Option<u32>) {
    if !x.dri3 {
        log::info!("no DRI3 1.2: the X server presents");
        return (None, None);
    }
    match Gpu::new(x, display) {
        Ok((gpu, pixmap)) => {
            log::info!("presenting with {}", gpu.route);
            (Some(gpu), Some(pixmap))
        }
        Err(e) => {
            log::warn!("Vulkan cannot present here ({e:#}); the X server presents");
            (None, None)
        }
    }
}

/// Names the window's pixmap and puts a picture on it, clipped to its bounding
/// shape when it has one. Errors (the window went away meanwhile) come back as
/// events and are ignored; its events are on the way.
fn bind(x: &X, w: &mut Win) -> Result<()> {
    w.release(&x.conn);
    let pixmap = x.conn.generate_id()?;
    x.conn.composite_name_window_pixmap(w.id, pixmap)?;
    let format = x.visual_format(w.visual).unwrap_or(x.root_format());
    let picture = x.create_picture(pixmap, format)?;
    if w.shaped {
        let region = x.conn.generate_id()?;
        x.conn.xfixes_create_region_from_window(region, w.id, shape::SK::BOUNDING)?;
        // The region starts at the window's inside, the pixmap at its border.
        let b = w.geom.border as i16;
        x.conn.xfixes_translate_region(region, b, b)?;
        x.conn.xfixes_set_picture_clip_region(picture, region, 0, 0)?;
        x.conn.xfixes_destroy_region(region)?;
    }
    (w.pixmap, w.picture, w.alpha, w.stale) = (Some(pixmap), Some(picture), format.alpha, false);
    Ok(())
}

impl Compositor {
    pub fn new(opts: &Options) -> Result<Compositor> {
        let mut x = X::connect(opts.display.as_deref())?;
        x.become_compositor()?;
        let (gpu, pixmap) = if opts.vulkan { try_gpu(&x, opts.display.as_deref()) } else { (None, None) };
        let setup = (|| -> Result<_> {
            Ok((Scene::new(&x, pixmap)?, x.create_picture(x.overlay, x.root_format())?, x.create_fence()?))
        })();
        let (scene, overlay_picture, fence) = match setup {
            Ok(v) => v,
            Err(e) => {
                drop(gpu);
                x.release_compositor();
                return Err(e);
            }
        };
        let now = Instant::now();
        let zoom = Zoom::new(x.root_cardinal(x.atoms.picom_zoom, AtomEnum::CARDINAL), now);
        let min_interval = frame_interval(&x, opts.max_fps);
        let mut c = Compositor {
            vt: x.vt().and_then(Vt::open),
            x,
            display: opts.display.clone(),
            wins: WinList::default(),
            scene,
            background: Background::new(opts.background),
            overlay_picture,
            gpu,
            vulkan: opts.vulkan,
            gpu_failures: 0,
            last_failure: None,
            fence,
            blocked_since: None,
            zoom,
            linear: opts.linear,
            pointer: [0, 0],
            next_pointer_poll: now,
            last_pointer_query: now - Duration::from_secs(1),
            pointer_moved: true,
            watching_pointer: false,
            shown: None,
            full_present: true,
            unredirect_delay: opts.unredirect_delay,
            candidate: None,
            unredirected: None,
            quit: false,
            last_frame: None,
            max_fps: opts.max_fps,
            min_interval,
            stats: Stats::default(),
            stats_since: now,
        };
        // From here on Drop undoes everything, including the X state.
        for (w, attrs, geom) in c.x.scan()? {
            c.add_window(w, &attrs, &geom)?;
        }
        c.background.set(&c.x, c.x.root_pixmap())?;
        c.scene.add_all(&c.x)?;
        let vt = c.vt.as_ref().map(|_| format!(", watching VT {}", c.x.vt().unwrap_or(0))).unwrap_or_default();
        let fps = 1.0 / c.min_interval.as_secs_f64();
        log::info!(
            "composing {} top-level windows on a {}x{} screen, up to {fps:.0} fps{vt}",
            c.wins.len(),
            c.x.width,
            c.x.height
        );
        c.x.conn.flush()?;
        Ok(c)
    }

    /// Sets the Vulkan presenter up from scratch with a new scene: after a VT
    /// switch, a new screen size or a GPU error. The X server presents when that
    /// fails, or after GPU_RETRIES errors.
    fn rebuild_gpu(&mut self, why: &str) -> Result<()> {
        log::info!("{why}: setting the presenter up again");
        self.gpu = None;
        let pixmap = match (self.vulkan, self.gpu_failures < GPU_RETRIES) {
            (true, true) => {
                let (gpu, pixmap) = try_gpu(&self.x, self.display.as_deref());
                self.gpu = gpu;
                pixmap
            }
            (true, false) => {
                log::warn!("{GPU_RETRIES} GPU errors in a row; the X server presents for now");
                None
            }
            (false, _) => None,
        };
        self.scene.destroy(&self.x);
        self.scene = Scene::new(&self.x, pixmap)?;
        self.blocked_since = None;
        self.repaint_all()
    }

    fn add_window(&mut self, id: Window, attrs: &GetWindowAttributesReply, geom: &GetGeometryReply) -> Result<()> {
        let mut w = Win {
            id,
            geom: Geometry { x: geom.x, y: geom.y, width: geom.width, height: geom.height, border: geom.border_width },
            visual: attrs.visual,
            input_only: attrs.class == WindowClass::INPUT_ONLY,
            mapped: attrs.map_state != MapState::UNMAPPED,
            ignored: id == self.x.overlay || id == self.x.owner,
            ..Default::default()
        };
        if !w.input_only && !w.ignored {
            w.damage = self.x.create_damage(id).ok();
            self.x.conn.shape_select_input(id, true)?;
            w.shaped = self.x.bounding_shaped(id);
        }
        if w.drawable() {
            w.stale = true;
            self.scene.add_rect(&self.x, w.geom.outer())?;
        }
        let g = &w.geom;
        log::debug!("add 0x{id:x} {}x{}+{}+{}{}", g.width, g.height, g.x, g.y, if w.mapped { " mapped" } else { "" });
        self.wins.insert_top(w);
        Ok(())
    }

    fn forget_window(&mut self, id: Window, destroyed: bool) -> Result<()> {
        let Some(mut w) = self.wins.remove(id) else { return Ok(()) };
        w.release(&self.x.conn);
        if let (Some(d), false) = (w.damage, destroyed) {
            // A destroyed window took its damage object with it.
            let _ = self.x.conn.damage_destroy(d);
        }
        if w.drawable() {
            self.scene.add_rect(&self.x, w.geom.outer())?;
        }
        log::debug!("forget 0x{id:x}");
        Ok(())
    }

    /// Damages the window's old and new areas after it moved or changed.
    fn moved(&mut self, id: Window, change: impl FnOnce(&mut Win)) -> Result<()> {
        let Some(w) = self.wins.get_mut(id) else { return Ok(()) };
        let (was, old) = (w.drawable(), w.geom.outer());
        change(w);
        let (is, new) = (w.drawable(), w.geom.outer());
        if was {
            self.scene.add_rect(&self.x, old)?;
        }
        if is {
            self.scene.add_rect(&self.x, new)?;
        }
        Ok(())
    }

    fn handle(&mut self, ev: Event) -> Result<()> {
        let root = self.x.root;
        match ev {
            Event::CreateNotify(e) if e.parent == root => {
                if let Some((a, g)) = self.x.query_window(e.window) {
                    self.add_window(e.window, &a, &g)?;
                }
            }
            Event::DestroyNotify(e) => self.forget_window(e.window, true)?,
            Event::MapNotify(e) => self.moved(e.window, |w| (w.mapped, w.stale) = (true, true))?,
            Event::UnmapNotify(e) => {
                self.moved(e.window, |w| w.mapped = false)?;
                if let Some(w) = self.wins.get_mut(e.window) {
                    w.release(&self.x.conn);
                }
            }
            Event::ConfigureNotify(e) if e.window == root => {
                if (e.width, e.height) != (self.x.width, self.x.height) {
                    log::info!("screen resized to {}x{}", e.width, e.height);
                    (self.x.width, self.x.height) = (e.width, e.height);
                    self.rebuild_gpu("new screen size")?;
                }
            }
            Event::RandrNotify(_) if self.max_fps.is_none() => {
                let interval = frame_interval(&self.x, None);
                if interval != self.min_interval {
                    log::info!("monitors changed: up to {:.0} fps", 1.0 / interval.as_secs_f64());
                    self.min_interval = interval;
                }
            }
            Event::ConfigureNotify(e) => {
                let g = Geometry { x: e.x, y: e.y, width: e.width, height: e.height, border: e.border_width };
                self.moved(e.window, |w| {
                    let (old, new) = (w.geom.outer(), g.outer());
                    w.stale |= w.mapped && (old.width, old.height, w.geom.border) != (new.width, new.height, g.border);
                    w.geom = g;
                })?;
                if !self.wins.restack(e.window, e.above_sibling) {
                    log::debug!("0x{:x} restacked above unknown 0x{:x}", e.window, e.above_sibling);
                }
            }
            Event::ReparentNotify(e) if e.parent == root => {
                if let Some((a, g)) = self.x.query_window(e.window) {
                    self.add_window(e.window, &a, &g)?;
                }
            }
            Event::ReparentNotify(e) => self.forget_window(e.window, false)?,
            Event::CirculateNotify(e) => {
                if e.place == Place::ON_TOP {
                    self.wins.raise(e.window)
                } else {
                    self.wins.lower(e.window)
                }
                self.moved(e.window, |_| ())?;
            }
            Event::GravityNotify(e) => self.moved(e.window, |w| (w.geom.x, w.geom.y) = (e.x, e.y))?,
            Event::DamageNotify(e) => match self.wins.get(e.drawable) {
                Some(w) if self.unredirected.is_none() && w.drawable() && !w.stale => {
                    // The region starts at the window's inside; a repainted
                    // border (xmonad's focus colour) lies at negative coordinates.
                    let b = w.geom.border as i16;
                    let (dx, dy) = (w.geom.x + b, w.geom.y + b);
                    self.scene.add_window_damage(&self.x, e.damage, dx, dy)?;
                }
                // Not on screen, or about to be redrawn whole anyway.
                _ => drop(self.x.conn.damage_subtract(e.damage, NONE, NONE)?),
            },
            Event::ShapeNotify(e) if e.shape_kind == shape::SK::BOUNDING => {
                self.moved(e.affected_window, |w| (w.shaped, w.stale) = (e.shaped, w.stale || w.mapped))?
            }
            Event::XinputRawMotion(_) => self.pointer_moved = true,
            Event::PropertyNotify(e) if e.window == root => {
                let a = &self.x.atoms;
                if e.atom == a.picom_zoom {
                    let v = (e.state != Property::DELETE).then(|| self.x.root_cardinal(e.atom, AtomEnum::CARDINAL));
                    let z = Zoom::from_property(v.flatten());
                    log::debug!("zoom target {z:.3}");
                    // The first zoomed frame needs to know where the pointer is.
                    self.pointer_moved = true;
                    self.zoom.set_target(z, Instant::now());
                } else if e.atom == a.xrootpmap_id || e.atom == a.esetroot_pmap_id {
                    self.background.set(&self.x, self.x.root_pixmap())?;
                    self.scene.add_all(&self.x)?;
                }
            }
            Event::Expose(e) if e.window == self.x.overlay => self.full_present = true,
            Event::SelectionClear(e) if e.selection == self.x.atoms.cm_selection => {
                log::warn!("another compositor took over the screen; exiting");
                // It owns the selection now; we must not touch the overlay or redirection.
                (self.x.owner, self.x.overlay, self.quit) = (NONE, NONE, true);
            }
            Event::Error(e) if error_is_benign(&e) => {
                log::trace!("X error {:?} (request {}.{})", e.error_kind, e.major_opcode, e.minor_opcode)
            }
            Event::Error(e) => log::warn!("X error {:?} (request {}.{})", e.error_kind, e.major_opcode, e.minor_opcode),
            _ => {}
        }
        Ok(())
    }

    /// After a VT switch or an unredirection the overlay's contents are gone.
    fn repaint_all(&mut self) -> Result<()> {
        self.full_present = true;
        self.scene.add_all(&self.x)
    }

    /// The top-most window when it covers the whole screen and nothing shows
    /// through it, and the zoom is off: then the screen can be its own.
    fn fullscreen_window(&self) -> Option<Window> {
        if self.zoom.active() {
            return None;
        }
        let w = self.wins.iter().filter(|w| w.drawable()).last()?;
        let r = w.geom.outer();
        let covers = r.x <= 0
            && r.y <= 0
            && r.x as i32 + r.width as i32 >= self.x.width as i32
            && r.y as i32 + r.height as i32 >= self.x.height as i32;
        let opaque = !w.shaped && !self.x.visual_format(w.visual).is_some_and(|f| f.alpha);
        (covers && opaque).then_some(w.id)
    }

    /// Unredirects for a full-screen window once it has been on top for the
    /// delay, and redirects again as soon as anything else needs compositing.
    /// Returns how long until the delay ends.
    fn update_redirection(&mut self, now: Instant) -> Result<Option<Duration>> {
        let Some(delay) = self.unredirect_delay else { return Ok(None) };
        let fs = self.fullscreen_window();
        if let Some(u) = self.unredirected {
            if fs != Some(u) {
                log::info!("compositing again");
                self.x.redirect()?;
                self.unredirected = None;
                for w in self.wins.iter_mut() {
                    w.stale = w.mapped;
                }
                self.repaint_all()?;
            }
            return Ok(None);
        }
        match (fs, self.candidate) {
            (Some(w), Some((c, since))) if w == c => {
                let left = delay.saturating_sub(now - since);
                if !left.is_zero() {
                    return Ok(Some(left));
                }
                log::info!("unredirecting the screen for the full-screen window 0x{w:x}");
                // No surface goes away with GPU work in flight.
                if let Some(gpu) = &mut self.gpu {
                    gpu.drop_swapchain();
                }
                for w in self.wins.iter_mut() {
                    w.release(&self.x.conn);
                }
                self.x.unredirect()?;
                (self.unredirected, self.candidate) = (Some(w), None);
                Ok(None)
            }
            (Some(w), _) => {
                self.candidate = Some((w, now));
                Ok(Some(delay))
            }
            (None, _) => {
                self.candidate = None;
                Ok(None)
            }
        }
    }

    fn view(&self) -> Option<View> {
        self.zoom.view([self.scene.width, self.scene.height], self.pointer)
    }

    fn wants_frame(&self) -> bool {
        self.scene.dirty || self.full_present || self.zoom.animating() || self.view() != self.shown
    }

    /// Follows the pointer while zoomed: on raw motion, at most once a frame, or
    /// by polling where X has no XInput 2. Returns when to look again.
    fn track_pointer(&mut self, now: Instant) -> Result<Option<Duration>> {
        let active = self.zoom.active();
        if active != self.watching_pointer {
            self.x.watch_pointer(active)?;
            self.watching_pointer = active;
        }
        if !active {
            return Ok(None);
        }
        let soonest = self.last_pointer_query + self.min_interval;
        if (self.pointer_moved && now >= soonest) || now >= self.next_pointer_poll {
            if let Some(p) = self.x.pointer() {
                self.pointer = p;
            }
            (self.pointer_moved, self.last_pointer_query) = (false, now);
            self.next_pointer_poll = now + if self.x.xi2 { WARP_POLL } else { POINTER_POLL };
        }
        let next = if self.pointer_moved { soonest.min(self.next_pointer_poll) } else { self.next_pointer_poll };
        Ok(Some(next.saturating_duration_since(now)))
    }

    /// Draws a frame, or returns how soon to try again when the Vulkan side has
    /// no image free yet; damage meanwhile merges into that frame.
    fn frame(&mut self, now: Instant) -> Result<Option<Duration>> {
        if let Some(gpu) = &mut self.gpu {
            if !gpu.scene_free()? || !gpu.acquire()? {
                let since = *self.blocked_since.get_or_insert(now);
                // No vblanks for a while (monitor off): look less often.
                let late = now - since > Duration::from_millis(50);
                return Ok(Some(Duration::from_millis(if late { 20 } else { 1 })));
            }
            self.stats.no_image += self.blocked_since.take().is_some() as u32;
        }
        for w in self.wins.iter_mut().filter(|w| w.drawable() && w.stale) {
            bind(&self.x, w)?;
        }
        let view = self.view();
        let painted = self.scene.dirty;
        if painted {
            self.scene.paint(&self.x, &self.background, self.wins.iter())?;
        }
        if let Some(gpu) = &mut self.gpu {
            self.scene.clear_damage(&self.x)?;
            let start = Instant::now();
            if !painted {
            } else if self.x.sync_gpu(self.fence, FENCE_LIMIT)? {
                self.stats.fence_sum += start.elapsed();
                self.stats.fence_max = self.stats.fence_max.max(start.elapsed());
            } else {
                // Its trigger may be lost for good; draw anyway, with a new fence.
                if self.stats.fence_timeouts == 0 {
                    log::warn!("the X server's fence did not trigger within {} ms", FENCE_LIMIT.as_millis());
                }
                self.stats.fence_timeouts += 1;
                self.x.conn.sync_destroy_fence(self.fence)?;
                self.fence = self.x.create_fence()?;
                self.scene.add_all(&self.x)?;
            }
            gpu.draw(view.unwrap_or(View::IDENTITY), self.linear)?;
        } else {
            let full = self.full_present || view != self.shown;
            let filter: &[u8] = if self.linear { b"bilinear" } else { b"nearest" };
            self.scene.present(&self.x, self.overlay_picture, view, filter, full)?;
            self.scene.clear_damage(&self.x)?;
        }
        self.x.conn.flush()?;
        (self.shown, self.full_present, self.last_frame) = (view, false, Some(now));
        self.stats.frames += 1;
        Ok(None)
    }

    /// A GPU error ends the Vulkan presenter; rebuild it, or let X present.
    fn frame_or_recover(&mut self, now: Instant) -> Result<Option<Duration>> {
        match self.frame(now) {
            Err(e) if self.gpu.is_some() && e.downcast_ref::<ash::vk::Result>().is_some() => {
                if self.last_failure.is_some_and(|t| now - t > Duration::from_secs(60)) {
                    self.gpu_failures = 0;
                }
                (self.gpu_failures, self.last_failure) = (self.gpu_failures + 1, Some(now));
                log::warn!("GPU error ({e:#})");
                self.rebuild_gpu("after a GPU error").map(|_| None)
            }
            r => r,
        }
    }

    fn log_stats(&mut self, now: Instant) {
        if now - self.stats_since < STATS_EVERY {
            return;
        }
        let s = std::mem::take(&mut self.stats);
        log::debug!(
            "{} frames in {:.0} s, {} waited for a free image; X's GPU work took {:.2} ms a frame on average, \
             at most {:.2} ms; {} fence timeouts",
            s.frames,
            (now - self.stats_since).as_secs_f64(),
            s.no_image,
            s.fence_sum.as_secs_f64() * 1e3 / s.frames.max(1) as f64,
            s.fence_max.as_secs_f64() * 1e3,
            s.fence_timeouts
        );
        self.stats_since = now;
    }

    /// Runs until a signal arrives on `sigfd` or another compositor takes over.
    pub fn run(&mut self, sigfd: RawFd) -> Result<()> {
        let xfd = self.x.conn.as_raw_fd();
        let mut vt_changed = false;
        loop {
            watchdog::busy();
            if let Some(signo) = pending_signal(sigfd) {
                log::info!("signal {signo}, shutting down");
                return Ok(());
            }
            if std::mem::take(&mut vt_changed) && self.vt.as_mut().is_some_and(|vt| vt.returned()) {
                self.rebuild_gpu("back on the X server's VT")?;
            }
            while let Some(ev) = self.x.conn.poll_for_event()? {
                self.handle(ev)?;
            }
            if self.quit {
                return Ok(());
            }
            let now = Instant::now();
            let mut timeout: Option<Duration> = None;
            let mut wake = |d: Duration| timeout = Some(timeout.map_or(d, |t| t.min(d)));
            self.zoom.advance(now);
            if let Some(d) = self.track_pointer(now)? {
                wake(d);
            }
            if let Some(d) = self.update_redirection(now)? {
                wake(d);
            }
            if self.unredirected.is_none() && self.wants_frame() {
                let rested = self.last_frame.map_or(Duration::MAX, |t| now - t);
                if rested < self.min_interval {
                    wake(self.min_interval - rested);
                } else if let Some(d) = self.frame_or_recover(now)? {
                    wake(d);
                } else {
                    continue;
                }
            }
            self.log_stats(now);
            self.x.conn.flush()?;
            watchdog::idle();

            let mut fds = vec![
                libc::pollfd { fd: xfd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: sigfd, events: libc::POLLIN, revents: 0 },
            ];
            if let Some(vt) = &self.vt {
                fds.push(libc::pollfd { fd: vt.fd(), events: libc::POLLPRI | libc::POLLERR, revents: 0 });
            }
            let ms = timeout.map_or(-1, |t| t.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32);
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) } < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e).context("poll");
                }
            }
            vt_changed = fds.get(2).is_some_and(|f| f.revents != 0);
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.gpu.take();
        for mut w in self.wins.drain() {
            w.release(&self.x.conn);
            if let Some(d) = w.damage {
                let _ = self.x.conn.damage_destroy(d);
            }
        }
        self.background.release(&self.x);
        self.scene.destroy(&self.x);
        let _ = self.x.conn.render_free_picture(self.overlay_picture);
        let _ = self.x.conn.sync_destroy_fence(self.fence);
        self.x.release_compositor();
        log::info!("released the screen");
    }
}
