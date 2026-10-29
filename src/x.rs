//! The X side: connection, extensions, the compositing manager selection, the
//! overlay window, picture formats and small request helpers.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::ffi::CString;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::present::{self, ConnectionExt as _};
use x11rb::protocol::render::{self, ConnectionExt as _, PictType};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::ConnectionExt as _;
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
    pub picom_zoom: u32,
    pub net_wm_name: u32,
    pub utf8_string: u32,
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
    depth_formats: HashMap<u8, Format>,
    /// The Present extension, for pacing frames to the vblank.
    pub present: bool,
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
        let present = conn.extension_information(present::X11_EXTENSION_NAME)?.is_some()
            && conn.present_query_version(1, 0)?.reply().is_ok();

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
        // Pixmaps have no visual; pick the standard formats by depth.
        let mut depth_formats = HashMap::new();
        for f in pf.formats.iter().filter(|f| f.type_ == PictType::DIRECT) {
            let d = &f.direct;
            let standard = match f.depth {
                24 => d.alpha_mask == 0 && (d.red_shift, d.green_shift, d.blue_shift) == (16, 8, 0),
                32 => d.alpha_mask == 0xff && (d.alpha_shift, d.red_shift, d.green_shift, d.blue_shift) == (24, 16, 8, 0),
                _ => false,
            };
            if standard {
                depth_formats.insert(f.depth, Format { id: f.id, alpha: d.alpha_mask != 0 });
            }
        }
        if !visual_formats.contains_key(&root_visual) {
            bail!("no picture format for the root visual");
        }

        let atoms = Atoms {
            cm_selection: intern(&conn, &format!("_NET_WM_CM_S{screen}"))?,
            xrootpmap_id: intern(&conn, "_XROOTPMAP_ID")?,
            esetroot_pmap_id: intern(&conn, "ESETROOT_PMAP_ID")?,
            picom_zoom: intern(&conn, "_PICOM_ZOOM")?,
            net_wm_name: intern(&conn, "_NET_WM_NAME")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
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
            depth_formats,
            present,
            owner: x11rb::NONE,
            overlay: x11rb::NONE,
        })
    }

    pub fn visual_format(&self, visual: u32) -> Option<Format> {
        self.visual_formats.get(&visual).copied()
    }

    pub fn depth_format(&self, depth: u8) -> Option<Format> {
        self.depth_formats.get(&depth).copied()
    }

    pub fn root_format(&self) -> Format {
        self.visual_formats[&self.root_visual]
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
            &CreateWindowAux::new().override_redirect(1),
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
        c.change_window_attributes(self.overlay, &ChangeWindowAttributesAux::new().event_mask(EventMask::EXPOSURE))?;

        c.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().event_mask(
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE,
            ),
        )?
        .check()
        .context("cannot select SubstructureNotify on the root")?;
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

    pub fn create_damage(&self, w: Window) -> Result<u32> {
        let d = self.conn.generate_id()?;
        self.conn.damage_create(d, w, damage::ReportLevel::NON_EMPTY)?;
        Ok(d)
    }

    pub fn pointer(&self) -> Option<[i16; 2]> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some([r.root_x, r.root_y])
    }

    /// The VT the X server runs on, which Xorg announces on the root window.
    pub fn vt(&self) -> Option<u32> {
        self.root_cardinal(self.atoms.xfree86_vt, AtomEnum::INTEGER)
    }

    /// Asks for a PresentCompleteNotify event on the overlay at its next vblank.
    pub fn select_present(&self) -> Result<()> {
        let eid = self.conn.generate_id()?;
        self.conn.present_select_input(eid, self.overlay, present::EventMask::COMPLETE_NOTIFY)?;
        Ok(())
    }

    pub fn notify_next_vblank(&self, serial: u32) -> Result<()> {
        self.conn.present_notify_msc(self.overlay, serial, 0, 1, 0)?;
        Ok(())
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
