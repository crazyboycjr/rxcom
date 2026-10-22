//! The X side: connection, extensions, the compositing manager selection, the
//! overlay window, and small request helpers.

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::CString;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::dri3::{self, ConnectionExt as _};
use x11rb::protocol::shape;
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::sync::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, GetGeometryReply,
    GetWindowAttributesReply, PropMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

pub struct Atoms {
    pub cm_selection: u32,
    pub xrootpmap_id: u32,
    pub esetroot_pmap_id: u32,
    pub picom_zoom: u32,
    pub net_wm_name: u32,
    pub utf8_string: u32,
}

/// Optional extensions and what they allow.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// DRI3 1.2: BuffersFromPixmap with explicit modifiers.
    pub dri3: bool,
    /// MIT-SHM 1.2: segments passed as file descriptors.
    pub shm_fd: bool,
    pub sync: bool,
}

pub struct X {
    pub conn: XCBConnection,
    pub screen: usize,
    pub root: Window,
    pub root_visual: u32,
    pub root_depth: u8,
    pub width: u16,
    pub height: u16,
    pub atoms: Atoms,
    pub features: Features,
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
        let (root, root_visual, root_depth, width, height) =
            (s.root, s.root_visual, s.root_depth, s.width_in_pixels, s.height_in_pixels);

        let v = conn.composite_query_version(0, 4)?.reply().context("no Composite extension")?;
        if (v.major_version, v.minor_version) < (0, 3) {
            bail!("Composite {}.{} is too old, need 0.3", v.major_version, v.minor_version);
        }
        conn.damage_query_version(1, 1)?.reply().context("no DAMAGE extension")?;
        conn.xfixes_query_version(5, 0)?.reply().context("no XFIXES extension")?;

        let mut features = Features::default();
        if conn.extension_information(dri3::X11_EXTENSION_NAME)?.is_some() {
            let v = conn.dri3_query_version(1, 2)?.reply()?;
            features.dri3 = (v.major_version, v.minor_version) >= (1, 2);
            log::debug!("DRI3 {}.{}", v.major_version, v.minor_version);
        }
        if conn.extension_information(shm::X11_EXTENSION_NAME)?.is_some() {
            let v = conn.shm_query_version()?.reply()?;
            features.shm_fd = (v.major_version, v.minor_version) >= (1, 2);
        }
        if conn.extension_information(sync::X11_EXTENSION_NAME)?.is_some() {
            conn.sync_initialize(3, 1)?.reply()?;
            features.sync = true;
        }

        let atoms = Atoms {
            cm_selection: intern(&conn, &format!("_NET_WM_CM_S{screen}"))?,
            xrootpmap_id: intern(&conn, "_XROOTPMAP_ID")?,
            esetroot_pmap_id: intern(&conn, "ESETROOT_PMAP_ID")?,
            picom_zoom: intern(&conn, "_PICOM_ZOOM")?,
            net_wm_name: intern(&conn, "_NET_WM_NAME")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
        };
        Ok(X {
            conn,
            screen,
            root,
            root_visual,
            root_depth,
            width,
            height,
            atoms,
            features,
            owner: x11rb::NONE,
            overlay: x11rb::NONE,
        })
    }

    /// Takes the compositing manager selection, redirects all top-level windows and
    /// gets the overlay window. Refuses when another compositor is running.
    pub fn become_compositor(&mut self) -> Result<()> {
        let c = &self.conn;
        let sel = self.atoms.cm_selection;
        let owner = c.get_selection_owner(sel)?.reply()?.owner;
        if owner != x11rb::NONE {
            bail!("another compositor is running (window 0x{owner:x} owns _NET_WM_CM_S{})", self.screen);
        }

        let w = c.generate_id()?;
        c.create_window(
            0,
            w,
            self.root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1).event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, b"rxcom")?;
        c.change_property8(PropMode::REPLACE, w, self.atoms.net_wm_name, self.atoms.utf8_string, b"rxcom")?;
        c.set_selection_owner(w, sel, x11rb::CURRENT_TIME)?;
        if c.get_selection_owner(sel)?.reply()?.owner != w {
            c.destroy_window(w)?;
            bail!("lost the race for _NET_WM_CM_S{} to another compositor", self.screen);
        }
        self.owner = w;

        if let Err(e) = c.composite_redirect_subwindows(self.root, composite::Redirect::MANUAL)?.check() {
            c.destroy_window(w)?;
            c.flush()?;
            self.owner = x11rb::NONE;
            return Err(anyhow!("cannot redirect the root's children (another compositor?): {e:?}"));
        }

        self.overlay = c.composite_get_overlay_window(self.root)?.reply()?.overlay_win;
        // An empty input shape lets every click fall through to the windows below.
        let region = c.generate_id()?;
        c.xfixes_create_region(region, &[])?;
        c.xfixes_set_window_shape_region(self.overlay, shape::SK::INPUT, 0, 0, region)?;
        c.xfixes_destroy_region(region)?;

        c.change_window_attributes(
            self.root,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new().event_mask(
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE,
            ),
        )?
        .check()
        .context("cannot select SubstructureNotify on the root")?;
        c.flush()?;
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
            let _ = c
                .composite_unredirect_subwindows(self.root, composite::Redirect::MANUAL)
                .map(|ck| ck.ignore_error());
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

    pub fn create_damage(&self, w: Window) -> Result<u32> {
        let d = self.conn.generate_id()?;
        self.conn.damage_create(d, w, damage::ReportLevel::NON_EMPTY)?;
        Ok(d)
    }

    pub fn pointer(&self) -> Option<(i16, i16)> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((r.root_x, r.root_y))
    }

    pub fn create_fence(&self) -> Result<u32> {
        let f = self.conn.generate_id()?;
        self.conn.sync_create_fence(self.root, f, false)?.check()?;
        Ok(f)
    }

    /// Waits until the X server has finished all rendering queued so far, the way
    /// picom's xrender-sync-fence does: trigger a fence, wait for it, round trip.
    pub fn fence_sync(&self, fence: u32) -> Result<()> {
        let c = &self.conn;
        c.sync_trigger_fence(fence)?;
        c.sync_await_fence(&[fence])?;
        c.sync_reset_fence(fence)?;
        c.get_input_focus()?.reply()?;
        Ok(())
    }

    pub fn buffers_from_pixmap(&self, pixmap: u32) -> Result<dri3::BuffersFromPixmapReply> {
        Ok(self.conn.dri3_buffers_from_pixmap(pixmap)?.reply()?)
    }
}

/// Short names for the errors a compositor expects while windows come and go.
pub fn error_is_benign(e: &x11rb::x11_utils::X11Error) -> bool {
    use x11rb::protocol::ErrorKind as K;
    matches!(
        e.error_kind,
        K::Window | K::Drawable | K::Pixmap | K::Match | K::DamageBadDamage | K::Value
    )
}
