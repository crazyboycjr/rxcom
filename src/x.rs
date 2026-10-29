//! The X side: connection, extensions, the compositing manager selection, the
//! overlay window, picture formats and small request helpers.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::OwnedFd;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::dri3::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::render::{self, ConnectionExt as _, PictType};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::sync::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, GetGeometryReply,
    GetWindowAttributesReply, PropMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

pub struct Atoms {
    pub cm_selection: u32,
    pub xrootpmap_id: u32,
    pub esetroot_pmap_id: u32,
    pub screen_zoom: u32,
    pub xfree86_vt: u32,
}

/// A picture format and whether it carries alpha.
#[derive(Clone, Copy, Debug)]
pub struct Format {
    pub id: u32,
    pub alpha: bool,
}

pub struct X {
    pub conn: XCBConnection,
    pub screen: usize,
    pub root: Window,
    pub root_visual: u32,
    pub width: u16,
    pub height: u16,
    pub atoms: Atoms,
    visual_formats: HashMap<u32, Format>,
    /// The standard ARGB32 format, for depth-32 pixmaps.
    argb32: Option<Format>,
    /// DRI3 1.2, to share the scene with Vulkan.
    pub dri3: bool,
    /// XInput 2.2, for raw pointer motion.
    pub xi2: bool,
    /// RandR 1.3, for the monitors' refresh rates.
    pub randr: bool,
    /// Owns the _NET_WM_CM_Sn selection.
    pub owner: Window,
    pub overlay: Window,
}

pub fn intern(conn: &XCBConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

impl X {
    pub fn connect(display: Option<&str>) -> Result<X> {
        let name = display.map(CString::new).transpose()?;
        let (conn, screen) = XCBConnection::connect(name.as_deref())
            .with_context(|| format!("cannot open display {}", display.unwrap_or("$DISPLAY")))?;
        let s = &conn.setup().roots[screen];
        let (root, root_visual, width, height) = (s.root, s.root_visual, s.width_in_pixels, s.height_in_pixels);

        let v = conn.composite_query_version(0, 4)?.reply().context("no Composite extension")?;
        if (v.major_version, v.minor_version) < (0, 3) {
            bail!("Composite {}.{} is too old, need 0.3", v.major_version, v.minor_version);
        }
        conn.damage_query_version(1, 1)?.reply().context("no DAMAGE extension")?;
        conn.xfixes_query_version(5, 0)?.reply().context("no XFIXES extension")?;
        let v = conn.render_query_version(0, 11)?.reply().context("no RENDER extension")?;
        if (v.major_version, v.minor_version) < (0, 6) {
            bail!("RENDER {}.{} is too old, need 0.6", v.major_version, v.minor_version);
        }
        conn.shape_query_version()?.reply().context("no SHAPE extension")?;
        let dri3 = conn.extension_information(dri3::X11_EXTENSION_NAME)?.is_some()
            && conn.dri3_query_version(1, 2)?.reply().is_ok_and(|v| (v.major_version, v.minor_version) >= (1, 2));
        let xi2 = conn.extension_information(xinput::X11_EXTENSION_NAME)?.is_some()
            && conn.xinput_xi_query_version(2, 2)?.reply().is_ok();
        let randr = conn.extension_information(randr::X11_EXTENSION_NAME)?.is_some()
            && conn.randr_query_version(1, 3)?.reply().is_ok_and(|v| (v.major_version, v.minor_version) >= (1, 3));
        conn.sync_initialize(3, 1)?.reply().context("no SYNC extension")?;
        // The X server's scheduler then serves rxcom's requests ahead of busy clients.
        conn.sync_set_priority(x11rb::NONE, 10)?;

        let pf = conn.render_query_pict_formats()?.reply()?;
        let alpha_of: HashMap<u32, bool> = pf
            .formats
            .iter()
            .filter(|f| f.type_ == PictType::DIRECT)
            .map(|f| (f.id, f.direct.alpha_mask != 0))
            .collect();
        let mut visual_formats = HashMap::new();
        for d in &pf.screens[screen].depths {
            for v in &d.visuals {
                if let Some(&alpha) = alpha_of.get(&v.format) {
                    visual_formats.insert(v.visual, Format { id: v.format, alpha });
                }
            }
        }
        let argb32 = pf.formats.iter().find(|f| {
            let d = &f.direct;
            f.depth == 32
                && (d.alpha_mask, d.alpha_shift, d.red_shift, d.green_shift, d.blue_shift) == (0xff, 24, 16, 8, 0)
        });
        let argb32 = argb32.map(|f| Format { id: f.id, alpha: true });
        if !visual_formats.contains_key(&root_visual) {
            bail!("no picture format for the root visual");
        }

        let atoms = Atoms {
            cm_selection: intern(&conn, &format!("_NET_WM_CM_S{screen}"))?,
            xrootpmap_id: intern(&conn, "_XROOTPMAP_ID")?,
            esetroot_pmap_id: intern(&conn, "ESETROOT_PMAP_ID")?,
            screen_zoom: intern(&conn, "_SCREEN_ZOOM")?,
            xfree86_vt: intern(&conn, "XFree86_VT")?,
        };
        Ok(X {
            conn,
            screen,
            root,
            root_visual,
            width,
            height,
            atoms,
            visual_formats,
            argb32,
            dri3,
            xi2,
            randr,
            owner: x11rb::NONE,
            overlay: x11rb::NONE,
        })
    }

    pub fn visual_format(&self, visual: u32) -> Option<Format> {
        self.visual_formats.get(&visual).copied()
    }

    /// The format for a pixmap of `depth`, which has no visual of its own.
    pub fn pixmap_format(&self, depth: u8) -> Option<Format> {
        match depth {
            32 => self.argb32,
            d if d == self.conn.setup().roots[self.screen].root_depth => Some(self.root_format()),
            _ => None,
        }
    }

    pub fn root_format(&self) -> Format {
        self.visual_formats[&self.root_visual]
    }

    /// Takes the compositing manager selection, redirects all top-level windows and
    /// gets the overlay window. Refuses when another compositor is running; if
    /// this fails halfway, rxcom exits and the X server undoes the rest.
    pub fn become_compositor(&mut self) -> Result<()> {
        let c = &self.conn;
        let sel = self.atoms.cm_selection;
        let owner = c.get_selection_owner(sel)?.reply()?.owner;
        if owner != x11rb::NONE {
            bail!("another compositor is running (window 0x{owner:x} owns _NET_WM_CM_S{})", self.screen);
        }
        let w = c.generate_id()?;
        let aux = CreateWindowAux::new().override_redirect(1);
        c.create_window(0, w, self.root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux)?.check()?;
        c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, b"rxcom")?;
        c.set_selection_owner(w, sel, x11rb::CURRENT_TIME)?;
        if c.get_selection_owner(sel)?.reply()?.owner != w {
            bail!("lost the race for _NET_WM_CM_S{} to another compositor", self.screen);
        }
        self.owner = w;
        let redirect = c.composite_redirect_subwindows(self.root, composite::Redirect::MANUAL)?;
        redirect.check().context("cannot redirect the root's children (another compositor?)")?;

        self.overlay = c.composite_get_overlay_window(self.root)?.reply()?.overlay_win;
        // An empty input shape lets every click fall through to the windows below.
        let region = c.generate_id()?;
        c.xfixes_create_region(region, &[])?;
        c.xfixes_set_window_shape_region(self.overlay, shape::SK::INPUT, 0, 0, region)?;
        c.xfixes_destroy_region(region)?;
        c.change_window_attributes(self.overlay, &ChangeWindowAttributesAux::new().event_mask(EventMask::EXPOSURE))?;

        c.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?
        .check()
        .context("cannot select SubstructureNotify on the root")?;
        if self.randr {
            c.randr_select_input(self.root, randr::NotifyMask::CRTC_CHANGE)?;
        }
        c.flush()?;
        Ok(())
    }

    /// Hands the screen back to the windows, for a full-screen window that does
    /// not need compositing: they draw to the screen directly again.
    pub fn unredirect(&self) -> Result<()> {
        self.conn.composite_unredirect_subwindows(self.root, composite::Redirect::MANUAL)?;
        self.conn.unmap_window(self.overlay)?;
        Ok(())
    }

    pub fn redirect(&self) -> Result<()> {
        self.conn
            .composite_redirect_subwindows(self.root, composite::Redirect::MANUAL)?
            .check()
            .context("cannot redirect the root's children again")?;
        self.conn.map_window(self.overlay)?;
        Ok(())
    }

    /// Undoes become_compositor so the display is left as found. Safe to call twice.
    pub fn release_compositor(&mut self) {
        let c = &self.conn;
        if self.overlay != x11rb::NONE {
            let _ = c.composite_release_overlay_window(self.root).map(|ck| ck.ignore_error());
            self.overlay = x11rb::NONE;
        }
        if self.owner != x11rb::NONE {
            let _ =
                c.composite_unredirect_subwindows(self.root, composite::Redirect::MANUAL).map(|ck| ck.ignore_error());
            // Destroying the owner window gives up the selection.
            let _ = c.destroy_window(self.owner).map(|ck| ck.ignore_error());
            self.owner = x11rb::NONE;
        }
        let _ = c.get_input_focus().map(|ck| ck.reply());
    }

    /// The root's children bottom to top, with attributes and geometry, read under a
    /// server grab so no window is created or restacked halfway through.
    pub fn scan(&self) -> Result<Vec<(Window, GetWindowAttributesReply, GetGeometryReply)>> {
        let c = &self.conn;
        c.grab_server()?;
        let result = (|| {
            let tree = c.query_tree(self.root)?.reply()?;
            let cookies = tree
                .children
                .iter()
                .map(|&w| Ok((w, c.get_window_attributes(w)?, c.get_geometry(w)?)))
                .collect::<Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for (w, a, g) in cookies {
                if let (Ok(a), Ok(g)) = (a.reply(), g.reply()) {
                    out.push((w, a, g));
                }
            }
            Ok(out)
        })();
        c.ungrab_server()?;
        c.flush()?;
        result
    }

    pub fn query_window(&self, w: Window) -> Option<(GetWindowAttributesReply, GetGeometryReply)> {
        let a = self.conn.get_window_attributes(w).ok()?;
        let g = self.conn.get_geometry(w).ok()?;
        Some((a.reply().ok()?, g.reply().ok()?))
    }

    /// Whether the window's bounding region is not its plain rectangle.
    pub fn bounding_shaped(&self, w: Window) -> bool {
        let r = self.conn.shape_query_extents(w).ok().and_then(|c| c.reply().ok());
        r.is_some_and(|r| r.bounding_shaped)
    }

    /// Reads a single 32-bit value of a root property.
    pub fn root_cardinal(&self, atom: u32, ty: impl Into<u32>) -> Option<u32> {
        let r = self.conn.get_property(false, self.root, atom, ty, 0, 1).ok()?.reply().ok()?;
        r.value32()?.next()
    }

    pub fn root_pixmap(&self) -> Option<u32> {
        let p = [self.atoms.xrootpmap_id, self.atoms.esetroot_pmap_id]
            .into_iter()
            .find_map(|a| self.root_cardinal(a, AtomEnum::PIXMAP).filter(|&p| p != x11rb::NONE))?;
        // Setters that do not retain their pixmap leave a dead XID behind, which a
        // later client may reuse for a window.
        let is_window = self.conn.get_window_attributes(p).ok().is_some_and(|c| c.reply().is_ok());
        (!is_window).then_some(p)
    }

    pub fn create_damage(&self, w: Window, level: damage::ReportLevel) -> Result<u32> {
        let d = self.conn.generate_id()?;
        self.conn.damage_create(d, w, level)?;
        Ok(d)
    }

    pub fn pointer(&self) -> Option<[i16; 2]> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some([r.root_x, r.root_y])
    }

    /// The refresh rate of the fastest lit monitor, if RandR reports one.
    pub fn max_refresh(&self) -> Option<f64> {
        if !self.randr {
            return None;
        }
        let res = self.conn.randr_get_screen_resources_current(self.root).ok()?.reply().ok()?;
        let cookies: Vec<_> = res
            .crtcs
            .iter()
            .filter_map(|&crtc| self.conn.randr_get_crtc_info(crtc, res.config_timestamp).ok())
            .collect();
        cookies
            .into_iter()
            .filter_map(|c| c.reply().ok())
            .filter_map(|crtc| res.modes.iter().find(|m| m.id == crtc.mode))
            .map(|m| {
                let mut lines = f64::from(m.vtotal);
                if m.mode_flags.contains(randr::ModeFlag::DOUBLE_SCAN) {
                    lines *= 2.0;
                }
                if m.mode_flags.contains(randr::ModeFlag::INTERLACE) {
                    lines /= 2.0;
                }
                f64::from(m.dot_clock) / (f64::from(m.htotal) * lines)
            })
            .filter(|hz| hz.is_finite() && *hz > 0.0)
            .reduce(f64::max)
    }

    /// The VT the X server runs on, which Xorg announces on the root window.
    pub fn vt(&self) -> Option<u32> {
        self.root_cardinal(self.atoms.xfree86_vt, AtomEnum::INTEGER)
    }

    /// Raw motion of any pointer, delivered on the root whatever window has it.
    pub fn watch_pointer(&self, on: bool) -> Result<()> {
        if self.xi2 {
            let mask = if on { xinput::XIEventMask::RAW_MOTION } else { 0u32.into() };
            let m = xinput::EventMask { deviceid: xinput::Device::ALL_MASTER.into(), mask: vec![mask] };
            self.conn.xinput_xi_select_events(self.root, &[m])?;
        }
        Ok(())
    }

    /// The DRM modifiers the X server takes for a depth-24 pixmap on this screen.
    pub fn modifiers(&self) -> Result<Vec<u64>> {
        let r = self.conn.dri3_get_supported_modifiers(self.root, 24, 32)?.reply()?;
        let mut m = r.window_modifiers;
        m.extend(r.screen_modifiers.iter().filter(|s| !m.contains(s)).copied().collect::<Vec<_>>());
        Ok(m)
    }

    /// Wraps a single-plane dma-buf of depth 24 in a new pixmap.
    pub fn import_pixmap(&self, fd: OwnedFd, (w, h): (u16, u16), stride: u32, offset: u32, m: u64) -> Result<u32> {
        let p = self.conn.generate_id()?;
        let c = self.conn.dri3_pixmap_from_buffers(
            p,
            self.root,
            w,
            h,
            stride,
            offset,
            0,
            0,
            0,
            0,
            0,
            0,
            24,
            32,
            m,
            vec![fd],
        );
        c?.check().context("DRI3 PixmapFromBuffers")?;
        Ok(p)
    }

    /// The dma-buf behind a pixmap: (fd, stride, offset, modifier).
    pub fn buffer_from_pixmap(&self, pixmap: u32) -> Result<(OwnedFd, u32, u32, u64)> {
        let mut r = self.conn.dri3_buffers_from_pixmap(pixmap)?.reply().context("DRI3 BuffersFromPixmap")?;
        if r.buffers.len() != 1 || r.bpp != 32 {
            bail!("pixmap has {} planes at {} bpp", r.buffers.len(), r.bpp);
        }
        Ok((r.buffers.remove(0), r.strides[0], r.offsets[0], r.modifier))
    }

    pub fn create_fence(&self) -> Result<u32> {
        let f = self.conn.generate_id()?;
        self.conn.sync_create_fence(self.root, f, false)?;
        Ok(f)
    }

    /// Waits for everything the X server has queued so far to finish on its GPU,
    /// polling the fence with QueryFence. It never sends SyncAwaitFence, whose
    /// wakeup NVIDIA's X driver sometimes loses. Returns false after `limit`.
    pub fn sync_gpu(&self, fence: u32, limit: std::time::Duration) -> Result<bool> {
        let start = std::time::Instant::now();
        self.conn.sync_trigger_fence(fence)?;
        loop {
            if self.conn.sync_query_fence(fence)?.reply()?.triggered {
                self.conn.sync_reset_fence(fence)?;
                return Ok(true);
            }
            if start.elapsed() >= limit {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    pub fn create_picture(&self, drawable: u32, format: Format) -> Result<u32> {
        let p = self.conn.generate_id()?;
        self.conn.render_create_picture(p, drawable, format.id, &render::CreatePictureAux::new())?;
        Ok(p)
    }
}

/// Short names for the errors a compositor expects while windows come and go.
pub fn error_is_benign(e: &x11rb::x11_utils::X11Error) -> bool {
    use x11rb::protocol::ErrorKind as K;
    matches!(
        e.error_kind,
        K::Window | K::Drawable | K::Pixmap | K::Match | K::DamageBadDamage | K::Value | K::RenderPicture
    )
}
