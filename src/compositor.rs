//! The compositor: X events in, frames out.

use crate::gl::{self, Gl};
use crate::gpu::Gpu;
use crate::scene::{Background, Scene};
use crate::vt::Vt;
use crate::watchdog;
use crate::windows::{Geometry, Undrawn, Win, WinList, covers};
use crate::x::{X, error_is_benign};
use crate::zoom::{View, Zoom};
use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::render::ConnectionExt as _;
use x11rb::protocol::screensaver;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::sync::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, GetGeometryReply, GetWindowAttributesReply,
    MapState, Place, Property, Rectangle, Window, WindowClass,
};

/// What puts the composed desktop on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presenter {
    /// The X server copies the scene onto the overlay window; it may tear.
    X,
    /// A Vulkan swapchain on the overlay window, where the X server can share
    /// the scene through DRI3.
    Vulkan,
    /// GLX on the overlay window, with the scene bound as a texture.
    Gl,
}

pub struct Options {
    pub display: Option<String>,
    /// Where it fails, the X server presents.
    pub presenter: Presenter,
    /// Unredirect a full-screen opaque window after this long; None: never.
    pub unredirect_delay: Option<Duration>,
    pub linear: bool,
    /// None: the refresh rate of the fastest lit monitor.
    pub max_fps: Option<f64>,
    /// None: FIFO_LATEST_READY where offered, else FIFO.
    pub present_mode: Option<ash::vk::PresentModeKHR>,
    /// With Vulkan, present at every refresh even when nothing changed.
    pub continuous: bool,
    pub background: u32,
}

/// Without XInput 2 the pointer is polled while zoomed, as picom-zoom does;
/// with it, raw motion wakes rxcom, and the slow poll only catches warps.
const POINTER_POLL: Duration = Duration::from_millis(4);
const WARP_POLL: Duration = Duration::from_millis(100);
/// How long rxcom polls the X server's fence before drawing anyway.
const FENCE_LIMIT: Duration = Duration::from_millis(100);
const GPU_RETRIES: u32 = 3;
/// How long rxcom waits for the X server's Expose after a map, and for the
/// client of a resized window to draw before it asks for a drawing again.
const DRAW_WAIT: Duration = Duration::from_millis(50);
/// How long a window waits for the rest once its client has drawn part of it.
const DRAWING_LIMIT: Duration = Duration::from_millis(500);
const STATS_EVERY: Duration = Duration::from_secs(10);
/// A wait for a free swapchain image this long is logged.
const NO_IMAGE_LOG: Duration = Duration::from_millis(200);
/// Continuous frames come this much sooner than the refresh period, so that a
/// variable-refresh monitor never waits for one and stays at its top rate; at
/// each refresh the presentation engine shows the newest.
const CONTINUOUS_PACE: f64 = 0.95;
/// No free swapchain image for this long means no vblanks (monitor off). Then
/// rxcom looks for one every NO_VBLANKS_RETRY while it has news to show, and
/// every NO_VBLANKS_IDLE_RETRY while it has none.
const NO_VBLANKS: Duration = Duration::from_millis(50);
const NO_VBLANKS_RETRY: Duration = Duration::from_millis(20);
const NO_VBLANKS_IDLE_RETRY: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Stats {
    frames: u32,
    /// Frames the X server composed, and frames Vulkan drew rather than
    /// present an image that showed them already.
    composed: u32,
    drawn: u32,
    /// Frames with news that waited for a free swapchain image.
    no_image: u32,
    /// From the composition until the fence read triggered: summed, worst.
    fence_sum: Duration,
    fence_max: Duration,
    fence_timeouts: u32,
    damage_events: u32,
}

pub struct Compositor {
    x: X,
    display: Option<String>,
    wins: WinList,
    scene: Scene,
    background: Background,
    overlay_picture: u32,
    gpu: Option<Gpu>,
    gl: Option<Gl>,
    /// GL is to be set up for the current scene once vblanks arrive: until
    /// then NVIDIA's texture-from-pixmap calls could wait for one forever.
    gl_pending: bool,
    /// The presenter asked for, used whenever it works.
    presenter: Presenter,
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
    /// The last frame that had something new to show, and the last present.
    last_frame: Option<Instant>,
    last_present: Option<Instant>,
    /// Counts versions of what the screen shows: a new one whenever the scene,
    /// the view or the overlay changes. Vulkan redraws only a swapchain image
    /// drawn with an older one.
    content: u64,
    continuous: bool,
    /// A monitor is lit (RandR), so there is a refresh to keep up with.
    lit: bool,
    /// The screen saver blanks the screen.
    saver: bool,
    max_fps: Option<f64>,
    present_mode: Option<ash::vk::PresentModeKHR>,
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

/// The shortest time between frames: the user's limit, else the period of
/// `refresh` (the fastest lit monitor's), else 60 Hz's.
fn frame_interval(refresh: Option<f64>, max_fps: Option<f64>) -> Duration {
    let fps = max_fps.or(refresh).unwrap_or(60.0);
    Duration::from_secs_f64(1.0 / fps.max(1.0))
}

/// The Vulkan presenter and the scene pixmap it shares, or None when Vulkan
/// cannot present here and the X server will.
fn try_gpu(x: &X, display: Option<&str>, mode: Option<ash::vk::PresentModeKHR>) -> (Option<Gpu>, Option<u32>) {
    if !x.dri3 {
        log::info!("no DRI3 1.2: the X server presents");
        return (None, None);
    }
    match watchdog::setting_up(|| Gpu::new(x, display, mode)) {
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

/// The GL presenter showing `scene`, or None when GL cannot present here and
/// the X server will.
fn try_gl(x: &X, display: Option<&str>, scene: u32) -> Option<Gl> {
    match watchdog::setting_up(|| Gl::new(x, display, scene)) {
        Ok(gl) => {
            log::info!("presenting with {}", gl.route);
            Some(gl)
        }
        Err(e) => {
            log::warn!("GL cannot present here ({e:#}); the X server presents");
            None
        }
    }
}

/// Starts watching the window's drawing operation by operation, until its
/// client has drawn (`Win::witness`).
fn watch(x: &X, w: &mut Win) {
    if w.witness.is_none() && w.damage.is_some() {
        w.witness = x.create_damage(w.id, ReportLevel::RAW_RECTANGLES).ok();
    }
}

/// Names the window's pixmap and puts a picture on it, clipped to its bounding
/// shape when it has one. Errors (the window went away meanwhile) come back as
/// events and are ignored; its events are on the way. While the client has not
/// drawn into the new pixmap, a copy of the scene where the window lies stands
/// in for it, as the X presenter's screen would.
fn bind(x: &X, scene: &Scene, w: &mut Win) -> Result<()> {
    w.release(&x.conn);
    let pixmap = x.conn.generate_id()?;
    x.conn.composite_name_window_pixmap(w.id, pixmap)?;
    let format = x.visual_format(w.visual).unwrap_or(x.root_format());
    let picture = x.create_picture(pixmap, format)?;
    let stand_in = if w.undrawn.is_some() { scene.snapshot(x, w.geom.outer())? } else { None };
    if w.shaped {
        let region = x.conn.generate_id()?;
        x.conn.xfixes_create_region_from_window(region, w.id, shape::SK::BOUNDING)?;
        // The region starts at the window's inside, the pixmap at its border.
        let b = w.geom.border as i16;
        x.conn.xfixes_translate_region(region, b, b)?;
        for p in std::iter::once(picture).chain(stand_in.map(|s| s.1)) {
            x.conn.xfixes_set_picture_clip_region(p, region, 0, 0)?;
        }
        x.conn.xfixes_destroy_region(region)?;
    }
    (w.pixmap, w.picture, w.alpha, w.stale) = (Some(pixmap), Some(picture), format.alpha, false);
    w.stand_in = stand_in;
    Ok(())
}

impl Compositor {
    pub fn new(opts: &Options) -> Result<Compositor> {
        let mut x = X::connect(opts.display.as_deref())?;
        x.become_compositor()?;
        let display = opts.display.as_deref();
        let (gpu, pixmap) = match opts.presenter {
            Presenter::Vulkan => try_gpu(&x, display, opts.present_mode),
            _ => (None, None),
        };
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
        let zoom = Zoom::new(x.root_cardinal(x.atoms.screen_zoom, AtomEnum::CARDINAL), now);
        let (lit, refresh) = x.monitors();
        let min_interval = frame_interval(refresh, opts.max_fps);
        let saver = x.saver_on();
        let mut c = Compositor {
            vt: x.vt().and_then(Vt::open),
            x,
            display: opts.display.clone(),
            wins: WinList::default(),
            scene,
            background: Background::new(opts.background),
            overlay_picture,
            gpu,
            gl: None,
            gl_pending: opts.presenter == Presenter::Gl,
            presenter: opts.presenter,
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
            last_present: None,
            content: 0,
            continuous: opts.continuous,
            lit,
            saver,
            max_fps: opts.max_fps,
            present_mode: opts.present_mode,
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
        let every = match &c.gpu {
            Some(g) if c.continuous && g.keeps_newest() => ", presenting at every refresh",
            Some(_) if c.continuous => ", presenting on changes only (FIFO would queue every refresh's present)",
            _ => "",
        };
        log::info!(
            "composing {} top-level windows on a {}x{} screen, up to {fps:.0} fps{every}{vt}",
            c.wins.len(),
            c.x.width,
            c.x.height
        );
        if c.gl_pending && !c.vblanks() {
            log::info!(
                "GL waits for vblanks (a lit monitor, no screen saver, X's VT); the X server presents until then"
            );
        }
        c.start_gl()?;
        c.x.conn.flush()?;
        Ok(c)
    }

    /// Sets GL up for the current scene once vblanks arrive, in place of any
    /// for an older scene: NVIDIA's texture-from-pixmap calls wait for a vblank,
    /// forever while none comes.
    fn start_gl(&mut self) -> Result<()> {
        if !self.gl_pending || !self.vblanks() {
            return Ok(());
        }
        self.gl_pending = false;
        self.drop_gl();
        self.gl = try_gl(&self.x, self.display.as_deref(), self.scene.pixmap);
        self.repaint_all()
    }

    /// Ends the GL presenter. Without vblanks, unbinding the scene could wait
    /// for one forever, so then it is abandoned to the process's exit.
    fn drop_gl(&mut self) {
        if let Some(mut gl) = self.gl.take() {
            gl.release(&self.x);
            if !self.vblanks() {
                std::mem::forget(gl);
            }
        }
    }

    /// Sets the presenter up from scratch with a new scene: after a VT switch
    /// (Vulkan), a new screen size or a GPU error. The X server presents when
    /// that fails, or after GPU_RETRIES errors. GL moves to the new scene once
    /// vblanks allow; until then no frame is drawn.
    fn rebuild_gpu(&mut self, why: &str) -> Result<()> {
        log::info!("{why}: setting the presenter up again");
        self.gpu = None;
        let retry = self.gpu_failures < GPU_RETRIES;
        if !retry && self.presenter != Presenter::X {
            log::warn!("{GPU_RETRIES} GPU errors in a row; the X server presents for now");
        }
        let display = self.display.as_deref();
        let pixmap = match self.presenter {
            Presenter::Vulkan if retry => {
                let (gpu, pixmap) = try_gpu(&self.x, display, self.present_mode);
                self.gpu = gpu;
                pixmap
            }
            _ => None,
        };
        self.scene.destroy(&self.x);
        self.scene = Scene::new(&self.x, pixmap)?;
        self.gl_pending = self.presenter == Presenter::Gl && retry;
        if !self.gl_pending {
            self.drop_gl();
        }
        self.start_gl()?;
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
            w.damage = self.x.create_damage(id, ReportLevel::NON_EMPTY).ok();
            if !w.mapped {
                watch(&self.x, &mut w);
            }
            self.x.conn.shape_select_input(id, true)?;
            let events = ChangeWindowAttributesAux::new().event_mask(EventMask::EXPOSURE | EventMask::PROPERTY_CHANGE);
            self.x.conn.change_window_attributes(id, &events)?;
            w.shaped = self.x.bounding_shaped(id);
            w.bypass = self.x.cardinal(id, self.x.atoms.bypass_compositor, AtomEnum::CARDINAL).unwrap_or(0);
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

    /// A window that appeared on the root while rxcom runs. When its client
    /// mapped it at once, the X server asked it to draw before rxcom could
    /// watch, so rxcom asks again: ClearArea repaints the window's background,
    /// if it has one, and sends Expose.
    fn adopt(&mut self, id: Window) -> Result<()> {
        let Some((a, g)) = self.x.query_window(id) else { return Ok(()) };
        self.add_window(id, &a, &g)?;
        if let Some(w) = self.wins.get_mut(id)
            && w.drawable()
        {
            watch(&self.x, w);
            w.undrawn = Some(Undrawn::Mapped(Instant::now() + DRAW_WAIT));
            self.x.conn.clear_area(true, id, 0, 0, 0, 0)?;
        }
        Ok(())
    }

    /// Ends the waits that have a deadline. A window that got no Expose shows
    /// its own pixels, as does one drawn only in part for too long. One
    /// resized before rxcom watched it may have drawn unseen, or its client may
    /// be slow: rxcom asks for a drawing it will see, which a slow client's
    /// pending one absorbs. Returns how long until the next deadline.
    fn expire_undrawn(&mut self, now: Instant) -> Result<Option<Duration>> {
        let mut next: Option<Duration> = None;
        for w in self.wins.iter_mut() {
            let t = match w.undrawn {
                Some(Undrawn::Asked(Some(t))) if t <= now => {
                    self.x.conn.clear_area(true, w.id, 0, 0, 0, 0)?;
                    let t = now + DRAW_WAIT;
                    w.undrawn = Some(Undrawn::Mapped(t));
                    t
                }
                Some(Undrawn::Mapped(t) | Undrawn::Asked(Some(t)) | Undrawn::Drawing(_, t)) => t,
                _ => continue,
            };
            if t > now {
                next = Some(next.map_or(t - now, |n| n.min(t - now)));
            } else if let Some(r) = w.drawn(&self.x.conn) {
                self.scene.add_rect(&self.x, r)?;
            }
        }
        Ok(next)
    }

    fn forget_window(&mut self, id: Window, destroyed: bool) -> Result<()> {
        let Some(mut w) = self.wins.remove(id) else { return Ok(()) };
        w.release(&self.x.conn);
        // A destroyed window took its damage objects with it.
        if !destroyed {
            for d in [w.damage, w.witness].into_iter().flatten() {
                let _ = self.x.conn.damage_destroy(d);
            }
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

    /// One rectangle of one drawing operation on a window waiting for its
    /// client, `a` starting at the window's inside. Once the X server has
    /// asked the client to draw, damage within the window is the client's:
    /// what the X server paints itself around a new pixmap reaches into the
    /// border (it reports a border repaint as its bounding box), while a
    /// client's drawing is clipped to the inside. The wait ends once the
    /// client's drawing covers the window: at once for a GL swap or a whole
    /// image, later for a drawing in parts, perhaps the end of one begun
    /// before the window was mapped again.
    fn witnessed(&mut self, id: Window, a: Rectangle) -> Result<()> {
        let Some(w) = self.wins.get_mut(id) else { return Ok(()) };
        let (width, height) = (w.geom.width, w.geom.height);
        let within = a.x >= 0
            && a.y >= 0
            && i32::from(a.x) + i32::from(a.width) <= i32::from(width)
            && i32::from(a.y) + i32::from(a.height) <= i32::from(height);
        let (mut parts, until) = match &mut w.undrawn {
            Some(Undrawn::Asked(_)) if within => (Vec::new(), Instant::now() + DRAWING_LIMIT),
            Some(Undrawn::Drawing(parts, until)) if within => (std::mem::take(parts), *until),
            _ => return Ok(()),
        };
        parts.push(a);
        if !covers(&parts, width, height) {
            w.undrawn = Some(Undrawn::Drawing(parts, until));
        } else if let Some(r) = w.drawn(&self.x.conn) {
            self.scene.add_rect(&self.x, r)?;
        }
        Ok(())
    }

    fn handle(&mut self, ev: Event) -> Result<()> {
        let root = self.x.root;
        match ev {
            Event::CreateNotify(e) if e.parent == root => self.adopt(e.window)?,
            Event::DestroyNotify(e) => self.forget_window(e.window, true)?,
            Event::MapNotify(e) => self.moved(e.window, |w| {
                // One mapped at creation was adopted mapped, waiting already.
                if !w.mapped {
                    w.undrawn = Some(Undrawn::Mapped(Instant::now() + DRAW_WAIT));
                }
                (w.mapped, w.stale) = (true, true);
            })?,
            Event::UnmapNotify(e) => {
                self.moved(e.window, |w| (w.mapped, w.undrawn) = (false, None))?;
                if let Some(w) = self.wins.get_mut(e.window) {
                    w.release(&self.x.conn);
                    watch(&self.x, w);
                }
            }
            Event::ConfigureNotify(e) if e.window == root => {
                if (e.width, e.height) != (self.x.width, self.x.height) {
                    log::info!("screen resized to {}x{}", e.width, e.height);
                    (self.x.width, self.x.height) = (e.width, e.height);
                    self.rebuild_gpu("new screen size")?;
                }
            }
            Event::RandrNotify(_) => {
                let (lit, refresh) = self.x.monitors();
                let interval = frame_interval(refresh, self.max_fps);
                if interval != self.min_interval || lit != self.lit {
                    let none = if lit { "" } else { ", none lit" };
                    log::info!("monitors changed: up to {:.0} fps{none}", 1.0 / interval.as_secs_f64());
                    (self.min_interval, self.lit) = (interval, lit);
                }
            }
            Event::ScreensaverNotify(e) => {
                let on = matches!(e.state, screensaver::State::ON | screensaver::State::CYCLE);
                if on != self.saver {
                    log::info!("screen saver {}", if on { "on" } else { "off" });
                    self.saver = on;
                }
            }
            Event::ConfigureNotify(e) => {
                let g = Geometry { x: e.x, y: e.y, width: e.width, height: e.height, border: e.border_width };
                self.moved(e.window, |w| {
                    let (old, new) = (w.geom.outer(), g.outer());
                    // A new size means a new pixmap. Its client may redraw
                    // before rxcom watches, so the wait has a deadline.
                    if w.mapped && (old.width, old.height, w.geom.border) != (new.width, new.height, g.border) {
                        w.stale = true;
                        if !matches!(w.undrawn, Some(Undrawn::Mapped(_))) {
                            w.undrawn = Some(Undrawn::Asked(Some(Instant::now() + DRAW_WAIT)));
                        }
                    }
                    w.geom = g;
                })?;
                if let Some(w) = self.wins.get_mut(e.window)
                    && w.undrawn.is_some()
                {
                    watch(&self.x, w);
                }
                if !self.wins.restack(e.window, e.above_sibling) {
                    log::debug!("0x{:x} restacked above unknown 0x{:x}", e.window, e.above_sibling);
                }
            }
            Event::ReparentNotify(e) if e.parent == root => self.adopt(e.window)?,
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
            Event::DamageNotify(e) => {
                self.stats.damage_events += 1;
                match self.wins.get(e.drawable) {
                    Some(w) if w.witness == Some(e.damage) => self.witnessed(e.drawable, e.area)?,
                    Some(w)
                        if w.damage == Some(e.damage)
                            && self.unredirected.is_none()
                            && w.drawable()
                            && !w.stale
                            && w.stand_in.is_none() =>
                    {
                        // The region starts at the window's inside; a repainted
                        // border (xmonad's focus colour) lies at negative coordinates.
                        let b = w.geom.border as i16;
                        let (dx, dy) = (w.geom.x + b, w.geom.y + b);
                        self.scene.add_window_damage(&self.x, e.damage, dx, dy, w.geom.outer())?;
                    }
                    // Not on screen, about to be redrawn whole, behind its
                    // stand-in, or a witness already gone.
                    _ => drop(self.x.conn.damage_subtract(e.damage, NONE, NONE)?),
                }
            }
            Event::ShapeNotify(e) if e.shape_kind == shape::SK::BOUNDING => {
                self.moved(e.affected_window, |w| (w.shaped, w.stale) = (e.shaped, w.stale || w.mapped))?
            }
            Event::XinputRawMotion(_) => self.pointer_moved = true,
            Event::PropertyNotify(e) if e.window == root => {
                let a = &self.x.atoms;
                if e.atom == a.screen_zoom {
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
            Event::PropertyNotify(e) if e.atom == self.x.atoms.bypass_compositor => {
                let v = (e.state != Property::DELETE).then(|| self.x.cardinal(e.window, e.atom, AtomEnum::CARDINAL));
                if let Some(w) = self.wins.get_mut(e.window) {
                    w.bypass = v.flatten().unwrap_or(0);
                }
            }
            Event::Expose(e) if e.window == self.x.overlay => self.full_present = true,
            // The client is asked to draw what the X server mapped.
            Event::Expose(e) => {
                if let Some(w) = self.wins.get_mut(e.window)
                    && let Some(Undrawn::Mapped(_)) = w.undrawn
                {
                    w.undrawn = Some(Undrawn::Asked(None));
                }
            }
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
    /// through it, or it asks for the screen, and the zoom is off: then the
    /// screen can be its own. A window that asks to stay composited never is.
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
        (covers && w.bypass != 2 && (opaque || w.bypass == 1)).then_some(w.id)
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
                // New pixmaps again, copied from a screen the windows drew on.
                for w in self.wins.iter_mut().filter(|w| w.mapped) {
                    w.drawn(&self.x.conn);
                    w.stale = true;
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

    /// Whether vblanks arrive: a monitor is lit, the screen saver has not
    /// blanked it, and the X server's VT is in front.
    fn vblanks(&self) -> bool {
        self.lit && !self.saver && self.vt.as_ref().is_none_or(Vt::ours)
    }

    /// Whether to present at every refresh: with Vulkan, while compositing and
    /// vblanks arrive. A variable-refresh monitor then stays at its top rate
    /// instead of following the desktop's pauses.
    fn continuous_now(&self) -> bool {
        self.continuous
            && self.gpu.as_ref().is_some_and(Gpu::keeps_newest)
            && self.unredirected.is_none()
            && self.vblanks()
    }

    /// When the next frame is due, if one is: once something changed, at most
    /// at the frame limit, and while continuous, a refresh after the last
    /// present. A present that only keeps up with the refresh never delays a
    /// frame with news, which supersedes it if both wait for the same vblank.
    /// GL swaps nothing while no vblanks arrive, since its swap would wait.
    fn next_frame(&self, now: Instant) -> Option<Instant> {
        if self.unredirected.is_some() || (self.gl.is_some() && !self.vblanks()) {
            return None;
        }
        let continuous = self.continuous_now();
        let pace = if continuous { self.min_interval.mul_f64(CONTINUOUS_PACE) } else { self.min_interval };
        let after = |last: Option<Instant>| Some(last.map_or(now, |t| t + pace));
        if self.wants_frame() {
            after(self.last_frame)
        } else if continuous {
            after(self.last_present)
        } else {
            None
        }
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
    /// no image free yet; damage meanwhile merges into that frame. A frame
    /// with nothing new costs the X server nothing.
    fn frame(&mut self, now: Instant) -> Result<Option<Duration>> {
        let news = self.wants_frame();
        let painted = self.scene.dirty;
        if let Some(gl) = &mut self.gl
            && !gl.ready(&self.x, painted)?
        {
            return Ok(Some(Duration::from_millis(1)));
        }
        if let Some(gpu) = &mut self.gpu {
            if (painted && !gpu.scene_free()?) || !gpu.acquire()? {
                let since = *self.blocked_since.get_or_insert(now);
                let retry = match (now - since > NO_VBLANKS, news) {
                    (false, _) => Duration::from_millis(1),
                    (true, true) => NO_VBLANKS_RETRY,
                    (true, false) => NO_VBLANKS_IDLE_RETRY,
                };
                return Ok(Some(retry));
            }
            if let Some(since) = self.blocked_since.take() {
                self.stats.no_image += news as u32;
                if now - since >= NO_IMAGE_LOG {
                    log::info!("no free swapchain image for {} ms", (now - since).as_millis());
                }
            }
        }
        for w in self.wins.iter_mut().filter(|w| w.drawable() && w.stale) {
            bind(&self.x, &self.scene, w)?;
        }
        let view = self.view();
        if painted || self.full_present || view != self.shown {
            self.content += 1;
        }
        if painted {
            self.scene.paint(&self.x, &self.background, self.wins.iter())?;
            self.stats.composed += 1;
        }
        let damage = self.scene.bounds.filter(|_| painted);
        if painted && (self.gpu.is_some() || self.gl.is_some()) {
            self.scene.clear_damage(&self.x)?;
            if !self.gl.as_ref().is_some_and(Gl::waits_on_gpu) {
                self.wait_for_composition()?;
            }
        }
        if let Some(gpu) = &mut self.gpu {
            if gpu.draw(view.unwrap_or(View::IDENTITY), self.linear, self.content)? {
                self.stats.drawn += 1;
            }
        } else if let Some(gl) = &mut self.gl {
            let full = self.full_present || view.is_some() || view != self.shown;
            if gl.draw(&self.x, view.unwrap_or(View::IDENTITY), self.linear, damage, full)? {
                self.stats.drawn += 1;
            }
        } else {
            let full = self.full_present || view != self.shown;
            let filter: &[u8] = if self.linear { b"bilinear" } else { b"nearest" };
            self.scene.present(&self.x, self.overlay_picture, view, filter, full)?;
            self.scene.clear_damage(&self.x)?;
            self.stats.drawn += 1;
        }
        self.x.conn.flush()?;
        (self.shown, self.full_present, self.last_present) = (view, false, Some(now));
        if news {
            self.last_frame = Some(now);
        }
        self.stats.frames += 1;
        Ok(None)
    }

    /// Polls until the X server's GPU has finished the composition, for at
    /// most FENCE_LIMIT; past that the frame goes out anyway and the next one
    /// repaints the whole scene.
    fn wait_for_composition(&mut self) -> Result<()> {
        let start = Instant::now();
        if self.x.sync_gpu(self.fence, FENCE_LIMIT)? {
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
        Ok(())
    }

    /// A GPU error ends the Vulkan or GL presenter: set it up again at once,
    /// or let X present.
    fn frame_or_recover(&mut self, now: Instant) -> Result<Option<Duration>> {
        match self.frame(now) {
            Err(e)
                if (self.gpu.is_some() && e.downcast_ref::<ash::vk::Result>().is_some())
                    || (self.gl.is_some() && e.downcast_ref::<gl::Lost>().is_some()) =>
            {
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
        let gl = self.gl.as_mut().map(Gl::take_stats).map_or(String::new(), |(partial, swap, wait)| {
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            format!(
                "; {partial} partial redraws, the slowest swap call took {:.2} ms, the slowest fence wait {:.2} ms",
                ms(swap),
                ms(wait)
            )
        });
        log::debug!(
            "{} frames in {:.0} s, {} composed, {} drawn, {} waited for a free image; X's GPU work took {:.2} ms \
             a composition on average, at most {:.2} ms; {} fence timeouts; {} damage events{gl}",
            s.frames,
            (now - self.stats_since).as_secs_f64(),
            s.composed,
            s.drawn,
            s.no_image,
            s.fence_sum.as_secs_f64() * 1e3 / s.composed.max(1) as f64,
            s.fence_max.as_secs_f64() * 1e3,
            s.fence_timeouts,
            s.damage_events
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
                // NVIDIA keeps a GL context's memory across VT switches unless
                // it opted into reports of purged memory, which rxcom's has not.
                if let Some(gl) = &mut self.gl {
                    log::info!("back on the X server's VT: repainting everything");
                    gl.forget_history();
                    self.repaint_all()?;
                } else {
                    self.rebuild_gpu("back on the X server's VT")?;
                }
            }
            while let Some(ev) = self.x.conn.poll_for_event()? {
                self.handle(ev)?;
            }
            if self.quit {
                return Ok(());
            }
            self.start_gl()?;
            let now = Instant::now();
            let mut timeout: Option<Duration> = None;
            let mut wake = |d: Duration| timeout = Some(timeout.map_or(d, |t| t.min(d)));
            self.zoom.advance(now);
            if let Some(d) = self.track_pointer(now)? {
                wake(d);
            }
            if let Some(d) = self.expire_undrawn(now)? {
                wake(d);
            }
            if let Some(d) = self.gpu.as_mut().and_then(|g| g.check_presents(now)) {
                wake(d);
            }
            if let Some(d) = self.update_redirection(now)? {
                wake(d);
            }
            if let Some(due) = self.next_frame(now) {
                if due > now {
                    wake(due - now);
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
            let ts = timeout.map(|t| libc::timespec {
                tv_sec: t.as_secs().min(i32::MAX as u64) as libc::time_t,
                tv_nsec: t.subsec_nanos().into(),
            });
            let ts = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
            if unsafe { libc::ppoll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ts, std::ptr::null()) } < 0 {
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
        self.drop_gl();
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
