//! The compositor: X events in, frames out.

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
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, GetGeometryReply, GetWindowAttributesReply, MapState, Place, Property, Window,
    WindowClass,
};

pub struct Options {
    pub display: Option<String>,
    /// Unredirect a full-screen opaque window after this long; None: never.
    pub unredirect_delay: Option<Duration>,
    /// The XRender filter for the zoom, "nearest" or "bilinear".
    pub zoom_filter: &'static [u8],
    pub max_fps: f64,
    pub background: u32,
}

/// How often the pointer is polled while zoomed, as picom-zoom does.
const POINTER_POLL: Duration = Duration::from_millis(4);
/// How long a frame waits for the vblank before it goes out anyway: without
/// vblanks (monitor off, VT away) frames keep coming, just slowly.
const VBLANK_TIMEOUT: Duration = Duration::from_millis(50);
const STATS_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Stats {
    frames: u32,
    vblanks: u32,
    vblank_timeouts: u32,
    /// Time from asking for the next vblank until it came, summed and worst.
    wait_sum: Duration,
    wait_max: Duration,
    since: Option<Instant>,
}

pub struct Compositor {
    x: X,
    wins: WinList,
    scene: Scene,
    background: Background,
    overlay_picture: u32,
    zoom: Zoom,
    filter: &'static [u8],
    pointer: [i16; 2],
    next_pointer_poll: Instant,
    /// The zoom view on screen (None: unzoomed).
    shown: Option<View>,
    /// The overlay lost its contents (exposed, remapped, VT switch): copy everything.
    full_present: bool,
    /// The outstanding NotifyMSC, and when it was sent. Only one at a time, so
    /// none pile up in the X server while no vblank comes.
    pending_msc: Option<(u32, Instant)>,
    /// A vblank has just passed.
    vblank_due: bool,
    serial: u32,
    unredirect_delay: Option<Duration>,
    /// A full-screen window that may get the screen, and since when.
    candidate: Option<(Window, Instant)>,
    /// The window the screen is unredirected for.
    unredirected: Option<Window>,
    vt: Option<Vt>,
    quit: bool,
    last_frame: Option<Instant>,
    min_interval: Duration,
    stats: Stats,
}

/// Reads one signal from the non-blocking signalfd, if any arrived.
fn pending_signal(sigfd: RawFd) -> Option<u32> {
    let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
    let n = unsafe { libc::read(sigfd, info.as_mut_ptr().cast(), info.len()) };
    (n == info.len() as isize).then(|| u32::from_ne_bytes(info[..4].try_into().unwrap()))
}

fn geometry(g: &GetGeometryReply) -> Geometry {
    Geometry { x: g.x, y: g.y, width: g.width, height: g.height, border: g.border_width }
}

impl Compositor {
    pub fn new(opts: &Options) -> Result<Compositor> {
        let mut x = X::connect(opts.display.as_deref())?;
        x.become_compositor()?;
        let setup = (|| -> Result<_> { Ok((Scene::new(&x)?, x.create_picture(x.overlay, x.root_format())?)) })();
        let (scene, overlay_picture) = match setup {
            Ok(v) => v,
            Err(e) => {
                x.release_compositor();
                return Err(e);
            }
        };
        let now = Instant::now();
        let zoom = Zoom::new(x.root_cardinal(x.atoms.picom_zoom, AtomEnum::CARDINAL), now);
        let mut c = Compositor {
            vt: x.vt().and_then(Vt::open),
            x,
            wins: WinList::new(),
            scene,
            background: Background::new(opts.background),
            overlay_picture,
            zoom,
            filter: opts.zoom_filter,
            pointer: [0, 0],
            next_pointer_poll: now,
            shown: None,
            full_present: true,
            pending_msc: None,
            vblank_due: false,
            serial: 0,
            unredirect_delay: opts.unredirect_delay,
            candidate: None,
            unredirected: None,
            quit: false,
            last_frame: None,
            min_interval: Duration::from_secs_f64(1.0 / opts.max_fps.max(1.0)),
            stats: Stats::default(),
        };
        // From here on Drop undoes everything, including the X state.
        c.init()?;
        Ok(c)
    }

    fn init(&mut self) -> Result<()> {
        if self.x.present {
            self.x.select_present()?;
        }
        for (w, attrs, geom) in self.x.scan()? {
            self.add_window(w, &attrs, &geom)?;
        }
        self.background.set(&self.x, self.x.root_pixmap())?;
        self.scene.add_all(&self.x)?;
        log::info!(
            "composing {} top-level windows with XRender on a {}x{} screen; presenting with the X server{}{}",
            self.wins.len(),
            self.x.width,
            self.x.height,
            if self.x.present { ", paced by Present" } else { "" },
            match &self.vt {
                Some(_) => format!(", watching VT {}", self.x.vt().unwrap_or(0)),
                None => String::new(),
            }
        );
        self.x.conn.flush()?;
        Ok(())
    }

    fn add_window(&mut self, id: Window, attrs: &GetWindowAttributesReply, geom: &GetGeometryReply) -> Result<()> {
        let mut w = Win::new(id, geometry(geom), attrs.visual);
        w.input_only = attrs.class == WindowClass::INPUT_ONLY;
        w.ignored = id == self.x.overlay || id == self.x.owner;
        w.mapped = attrs.map_state != MapState::UNMAPPED;
        if !w.input_only && !w.ignored {
            w.damage = self.x.create_damage(id).ok();
            self.x.conn.shape_select_input(id, true)?;
            w.shaped = self.x.bounding_shaped(id);
        }
        if w.drawable() {
            w.stale = true;
            self.scene.add_rect(&self.x, w.geom.outer())?;
        }
        log::debug!(
            "add 0x{id:x} {}x{}+{}+{} visual 0x{:x}{}{}{}",
            w.geom.width,
            w.geom.height,
            w.geom.x,
            w.geom.y,
            w.visual,
            if w.mapped { " mapped" } else { "" },
            if w.shaped { " shaped" } else { "" },
            if w.ignored { " (ignored)" } else { "" }
        );
        self.wins.insert_top(w);
        Ok(())
    }

    /// Frees the window's pixmap and picture; the damage object stays.
    fn release_picture(&mut self, id: Window) {
        let Some(w) = self.wins.get_mut(id) else { return };
        if let Some(p) = w.picture.take() {
            let _ = self.x.conn.render_free_picture(p);
        }
        if let Some(p) = w.pixmap.take() {
            let _ = self.x.conn.free_pixmap(p);
        }
    }

    fn forget_window(&mut self, id: Window, destroyed: bool) -> Result<()> {
        self.release_picture(id);
        if let Some(w) = self.wins.remove(id) {
            if let (Some(d), false) = (w.damage, destroyed) {
                // A destroyed window took its damage object with it.
                let _ = self.x.conn.damage_destroy(d);
            }
            if w.drawable() {
                self.scene.add_rect(&self.x, w.geom.outer())?;
            }
            log::debug!("forget 0x{id:x}");
        }
        Ok(())
    }

    /// Names the window's pixmap and puts a picture on it, clipped to its
    /// bounding shape when it has one. Errors (the window went away meanwhile)
    /// come back as events and are ignored; its events are on the way.
    fn bind(&mut self, id: Window) -> Result<()> {
        self.release_picture(id);
        let x = &self.x;
        let Some(w) = self.wins.get_mut(id) else { return Ok(()) };
        let pixmap = x.conn.generate_id()?;
        x.conn.composite_name_window_pixmap(id, pixmap)?;
        let format = x.visual_format(w.visual).unwrap_or(x.root_format());
        let picture = x.create_picture(pixmap, format)?;
        if w.shaped {
            let region = x.conn.generate_id()?;
            x.conn.xfixes_create_region_from_window(region, id, shape::SK::BOUNDING)?;
            // The region starts at the window's inside, the pixmap at its border.
            let b = w.geom.border as i16;
            x.conn.xfixes_translate_region(region, b, b)?;
            x.conn.xfixes_set_picture_clip_region(picture, region, 0, 0)?;
            x.conn.xfixes_destroy_region(region)?;
        }
        w.pixmap = Some(pixmap);
        w.picture = Some(picture);
        w.alpha = format.alpha;
        w.stale = false;
        Ok(())
    }

    fn damaged(&mut self, damage: u32, drawable: Window) -> Result<()> {
        log::trace!("damage on 0x{drawable:x}");
        match self.wins.get(drawable) {
            Some(w) if self.unredirected.is_none() && w.drawable() && !w.stale => {
                // The region starts at the window's inside; a repainted border
                // (xmonad's focus colour) lies at negative coordinates.
                let b = w.geom.border as i16;
                let (dx, dy) = (w.geom.x + b, w.geom.y + b);
                self.scene.add_window_damage(&self.x, damage, dx, dy)?;
            }
            // Not on screen, or about to be redrawn whole anyway.
            _ => {
                self.x.conn.damage_subtract(damage, NONE, NONE)?;
            }
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
            Event::MapNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    w.mapped = true;
                    w.stale = true;
                    if w.drawable() {
                        let r = w.geom.outer();
                        self.scene.add_rect(&self.x, r)?;
                    }
                }
            }
            Event::UnmapNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    let (was, r) = (w.drawable(), w.geom.outer());
                    w.mapped = false;
                    self.release_picture(e.window);
                    if was {
                        self.scene.add_rect(&self.x, r)?;
                    }
                }
            }
            Event::ConfigureNotify(e) if e.window == root => self.resize_screen(e.width, e.height)?,
            Event::ConfigureNotify(e) => {
                let Some(w) = self.wins.get_mut(e.window) else { return Ok(()) };
                let g = Geometry { x: e.x, y: e.y, width: e.width, height: e.height, border: e.border_width };
                let (old, new) = (w.geom.outer(), g.outer());
                if (old.width, old.height, w.geom.border) != (new.width, new.height, g.border) && w.mapped {
                    w.stale = true;
                }
                w.geom = g;
                let drawable = w.drawable();
                if !self.wins.restack(e.window, e.above_sibling) {
                    log::debug!("0x{:x} restacked above unknown 0x{:x}", e.window, e.above_sibling);
                }
                if drawable {
                    self.scene.add_rect(&self.x, old)?;
                    self.scene.add_rect(&self.x, new)?;
                }
            }
            Event::ReparentNotify(e) => {
                if e.parent == root {
                    if let Some((a, g)) = self.x.query_window(e.window) {
                        self.add_window(e.window, &a, &g)?;
                    }
                } else {
                    self.forget_window(e.window, false)?;
                }
            }
            Event::CirculateNotify(e) => {
                if e.place == Place::ON_TOP {
                    self.wins.raise(e.window);
                } else {
                    self.wins.lower(e.window);
                }
                if let Some(w) = self.wins.get(e.window).filter(|w| w.drawable()) {
                    let r = w.geom.outer();
                    self.scene.add_rect(&self.x, r)?;
                }
            }
            Event::GravityNotify(e) => {
                if let Some(w) = self.wins.get_mut(e.window) {
                    let old = w.geom.outer();
                    w.geom.x = e.x;
                    w.geom.y = e.y;
                    if w.drawable() {
                        let new = w.geom.outer();
                        self.scene.add_rect(&self.x, old)?;
                        self.scene.add_rect(&self.x, new)?;
                    }
                }
            }
            Event::DamageNotify(e) => self.damaged(e.damage, e.drawable)?,
            Event::ShapeNotify(e) if e.shape_kind == shape::SK::BOUNDING => {
                if let Some(w) = self.wins.get_mut(e.affected_window) {
                    w.shaped = e.shaped;
                    w.stale |= w.mapped;
                    if w.drawable() {
                        let r = w.geom.outer();
                        self.scene.add_rect(&self.x, r)?;
                    }
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
                    let now = Instant::now();
                    if !self.zoom.active() {
                        // The first zoomed frame needs to know where the pointer is.
                        self.next_pointer_poll = now;
                    }
                    self.zoom.set_target(z, now);
                } else if e.atom == a.xrootpmap_id || e.atom == a.esetroot_pmap_id {
                    self.background.set(&self.x, self.x.root_pixmap())?;
                    self.scene.add_all(&self.x)?;
                }
            }
            Event::Expose(e) if e.window == self.x.overlay => self.full_present = true,
            Event::PresentCompleteNotify(e) if e.window == self.x.overlay => {
                if let Some((serial, asked)) = self.pending_msc
                    && serial == e.serial
                {
                    self.pending_msc = None;
                    self.vblank_due = true;
                    let wait = asked.elapsed();
                    self.stats.wait_sum += wait;
                    self.stats.wait_max = self.stats.wait_max.max(wait);
                    self.stats.vblanks += 1;
                }
            }
            Event::SelectionClear(e) if e.selection == self.x.atoms.cm_selection => {
                log::warn!("another compositor took over the screen; exiting");
                // It owns the selection now; we must not touch the overlay or redirection.
                self.x.owner = NONE;
                self.x.overlay = NONE;
                self.quit = true;
            }
            Event::Error(e) => {
                if error_is_benign(&e) {
                    log::trace!("X error {:?} (request {}.{})", e.error_kind, e.major_opcode, e.minor_opcode);
                } else {
                    log::warn!("X error {:?} (request {}.{})", e.error_kind, e.major_opcode, e.minor_opcode);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn resize_screen(&mut self, width: u16, height: u16) -> Result<()> {
        if (width, height) == (self.x.width, self.x.height) {
            return Ok(());
        }
        log::info!("screen resized to {width}x{height}");
        self.x.width = width;
        self.x.height = height;
        self.scene.destroy(&self.x);
        self.scene = Scene::new(&self.x)?;
        self.repaint_all()
    }

    /// After a VT switch or an unredirection the overlay's contents are gone, and
    /// an outstanding NotifyMSC may never complete, so start over.
    fn repaint_all(&mut self) -> Result<()> {
        self.scene.add_all(&self.x)?;
        self.full_present = true;
        self.pending_msc = None;
        Ok(())
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
                self.redirect()?;
            }
            return Ok(None);
        }
        match (fs, self.candidate) {
            (Some(w), Some((c, since))) if w == c => {
                let left = delay.saturating_sub(now - since);
                if left.is_zero() {
                    self.unredirect(w)?;
                    return Ok(None);
                }
                Ok(Some(left))
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

    fn unredirect(&mut self, w: Window) -> Result<()> {
        log::info!("unredirecting the screen for the full-screen window 0x{w:x}");
        let ids: Vec<Window> = self.wins.order().to_vec();
        for id in ids {
            self.release_picture(id);
        }
        self.x.unredirect()?;
        self.unredirected = Some(w);
        self.candidate = None;
        self.vblank_due = false;
        Ok(())
    }

    fn redirect(&mut self) -> Result<()> {
        log::info!("compositing again");
        self.x.redirect()?;
        self.unredirected = None;
        let ids: Vec<Window> = self.wins.order().to_vec();
        for id in ids {
            if let Some(w) = self.wins.get_mut(id) {
                w.stale = w.mapped;
            }
        }
        self.repaint_all()
    }

    fn view(&self) -> Option<View> {
        self.zoom.view([self.scene.width, self.scene.height], self.pointer)
    }

    fn wants_frame(&self) -> bool {
        self.scene.dirty || self.full_present || self.zoom.animating() || self.view() != self.shown
    }

    fn frame(&mut self, now: Instant) -> Result<()> {
        let stale: Vec<Window> = self.wins.iter().filter(|w| w.drawable() && w.stale).map(|w| w.id).collect();
        for id in stale {
            self.bind(id)?;
        }
        let view = self.view();
        if self.scene.dirty {
            self.scene.paint(&self.x, &self.background, self.wins.iter())?;
        }
        let full = self.full_present || view != self.shown;
        self.scene.present(&self.x, self.overlay_picture, view, self.filter, full)?;
        self.scene.clear_damage(&self.x)?;
        self.x.conn.flush()?;
        self.vblank_due = false;
        self.shown = view;
        self.full_present = false;
        self.last_frame = Some(now);
        self.stats.frames += 1;
        log::trace!("frame: zoom {:?} pointer {:?}", view.map(|v| v.scale), self.pointer);
        Ok(())
    }

    fn log_stats(&mut self, now: Instant) {
        let since = *self.stats.since.get_or_insert(now);
        if now - since < STATS_EVERY {
            return;
        }
        let s = std::mem::take(&mut self.stats);
        if s.frames > 0 {
            log::debug!(
                "{} frames in {:.1} s; {} vblanks, waited for on average {:.2} ms, at most {:.2} ms; \
                 {} frames drawn without one",
                s.frames,
                (now - since).as_secs_f64(),
                s.vblanks,
                s.wait_sum.as_secs_f64() * 1e3 / s.vblanks.max(1) as f64,
                s.wait_max.as_secs_f64() * 1e3,
                s.vblank_timeouts
            );
        }
        self.stats.since = Some(now);
    }

    /// Runs until a signal arrives on `sigfd` or another compositor takes over.
    pub fn run(&mut self, sigfd: RawFd) -> Result<()> {
        let xfd = self.x.conn.as_raw_fd();
        loop {
            watchdog::busy();
            if let Some(signo) = pending_signal(sigfd) {
                log::info!("signal {signo}, shutting down");
                return Ok(());
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
            if self.zoom.active() {
                if now >= self.next_pointer_poll {
                    if let Some(p) = self.x.pointer() {
                        self.pointer = p;
                    }
                    self.next_pointer_poll = now + POINTER_POLL;
                }
                wake(self.next_pointer_poll - now);
            }
            if let Some(d) = self.update_redirection(now)? {
                wake(d);
            }
            if self.unredirected.is_none() && self.wants_frame() {
                // Draw right after a vblank. Without the Present extension, or
                // while no vblank comes (monitor off, VT away), a timer paces.
                let rested = self.last_frame.map_or(Duration::MAX, |t| now - t);
                // Present completes at once when the driver cannot wait for a
                // vblank (no CRTC), so its frames are capped too, with slack.
                let floor = if self.x.present { self.min_interval * 3 / 4 } else { self.min_interval };
                if rested < floor {
                    wake(floor - rested);
                } else if !self.x.present || self.vblank_due {
                    self.frame(now)?;
                    continue;
                } else if let Some((_, asked)) = self.pending_msc {
                    let waited = (now - asked).min(rested);
                    if waited >= VBLANK_TIMEOUT {
                        self.stats.vblank_timeouts += 1;
                        log::trace!("no vblank for {} ms; drawing anyway", VBLANK_TIMEOUT.as_millis());
                        self.frame(now)?;
                        continue;
                    }
                    wake(VBLANK_TIMEOUT - waited);
                } else {
                    self.serial = self.serial.wrapping_add(1);
                    self.x.notify_next_vblank(self.serial)?;
                    self.pending_msc = Some((self.serial, now));
                    wake(VBLANK_TIMEOUT);
                }
            } else {
                // A vblank nobody waited for must not hurry a later frame.
                self.vblank_due = false;
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
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e).context("poll");
                }
            }
            if fds.get(2).is_some_and(|f| f.revents != 0) && self.vt.as_mut().is_some_and(|vt| vt.returned()) {
                watchdog::busy();
                log::info!("back on the X server's VT; repainting everything");
                self.repaint_all()?;
            }
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        for w in self.wins.drain() {
            if let Some(p) = w.picture {
                let _ = self.x.conn.render_free_picture(p);
            }
            if let Some(p) = w.pixmap {
                let _ = self.x.conn.free_pixmap(p);
            }
            if let Some(d) = w.damage {
                let _ = self.x.conn.damage_destroy(d);
            }
        }
        self.background.release(&self.x);
        self.scene.destroy(&self.x);
        let _ = self.x.conn.render_free_picture(self.overlay_picture);
        self.x.release_compositor();
        log::info!("released the screen");
    }
}
