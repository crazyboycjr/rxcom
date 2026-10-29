//! The X server composes the desktop. Each frame the damaged part of the screen
//! is redrawn into one screen-sized pixmap with XRender, as xcompmgr does: the
//! background, then every mapped window bottom to top. The X presenter then
//! copies that pixmap, or a zoomed view of it, onto the overlay window.

use crate::windows::Win;
use crate::x::X;
use crate::zoom::View;
use anyhow::Result;
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::render::{self, ConnectionExt as _, PictOp, Transform};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Rectangle};

pub struct Scene {
    pub pixmap: u32,
    pub width: u16,
    pub height: u16,
    /// Draws into the scene, clipped to the damage.
    target: u32,
    /// Reads the scene when presenting; carries the zoom transform.
    source: u32,
    /// The part of the screen still to redraw.
    damage: u32,
    scratch: u32,
    pub dirty: bool,
    /// The zoom view `source` is transformed for.
    transform: Option<View>,
}

fn fixed(v: f64) -> i32 {
    (v * 65536.0).round() as i32
}

impl Scene {
    pub fn new(x: &X) -> Result<Scene> {
        let c = &x.conn;
        let depth = c.setup().roots[x.screen].root_depth;
        let pixmap = c.generate_id()?;
        c.create_pixmap(depth, pixmap, x.root, x.width, x.height)?;
        let format = x.root_format();
        let full = Rectangle { x: 0, y: 0, width: x.width, height: x.height };
        let (damage, scratch) = (c.generate_id()?, c.generate_id()?);
        c.xfixes_create_region(damage, &[full])?;
        c.xfixes_create_region(scratch, &[])?;
        Ok(Scene {
            pixmap,
            width: x.width,
            height: x.height,
            target: x.create_picture(pixmap, format)?,
            source: x.create_picture(pixmap, format)?,
            damage,
            scratch,
            dirty: true,
            transform: None,
        })
    }

    pub fn destroy(&self, x: &X) {
        let c = &x.conn;
        let _ = c.render_free_picture(self.target);
        let _ = c.render_free_picture(self.source);
        let _ = c.xfixes_destroy_region(self.damage);
        let _ = c.xfixes_destroy_region(self.scratch);
        let _ = c.free_pixmap(self.pixmap);
    }

    /// Collects a window's new damage; `(dx, dy)` is where the window's inside
    /// (within the border) starts on the screen.
    pub fn add_window_damage(&mut self, x: &X, damage: u32, dx: i16, dy: i16) -> Result<()> {
        let c = &x.conn;
        c.damage_subtract(damage, NONE, self.scratch)?;
        c.xfixes_translate_region(self.scratch, dx, dy)?;
        c.xfixes_union_region(self.damage, self.scratch, self.damage)?;
        self.dirty = true;
        Ok(())
    }

    pub fn add_rect(&mut self, x: &X, r: Rectangle) -> Result<()> {
        x.conn.xfixes_set_region(self.scratch, &[r])?;
        x.conn.xfixes_union_region(self.damage, self.scratch, self.damage)?;
        self.dirty = true;
        Ok(())
    }

    pub fn add_all(&mut self, x: &X) -> Result<()> {
        x.conn.xfixes_set_region(self.damage, &[Rectangle { x: 0, y: 0, width: self.width, height: self.height }])?;
        self.dirty = true;
        Ok(())
    }

    /// Redraws the damaged part of the scene: the background, then `wins` bottom
    /// to top. The damage stays until `clear_damage`, so a presenter can copy
    /// just that part.
    pub fn paint<'a>(&mut self, x: &X, bg: &Background, wins: impl Iterator<Item = &'a Win>) -> Result<()> {
        let c = &x.conn;
        c.xfixes_set_picture_clip_region(self.target, self.damage, 0, 0)?;
        match bg.picture {
            Some(p) => c.render_composite(PictOp::SRC, p, NONE, self.target, 0, 0, 0, 0, 0, 0, self.width, self.height)?,
            None => c.render_fill_rectangles(
                PictOp::SRC,
                self.target,
                bg.color,
                &[Rectangle { x: 0, y: 0, width: self.width, height: self.height }],
            )?,
        };
        for w in wins.filter(|w| w.drawable()) {
            let Some(picture) = w.picture else { continue };
            let r = w.geom.outer();
            let op = if w.alpha { PictOp::OVER } else { PictOp::SRC };
            c.render_composite(op, picture, NONE, self.target, 0, 0, 0, 0, r.x, r.y, r.width, r.height)?;
        }
        Ok(())
    }

    pub fn clear_damage(&mut self, x: &X) -> Result<()> {
        x.conn.xfixes_set_region(self.damage, &[])?;
        self.dirty = false;
        Ok(())
    }

    /// The X presenter: copies the scene onto `overlay` (a picture on the overlay
    /// window), only its damaged part unless `full`, or draws the zoomed `view`
    /// with `filter` ("nearest" or "bilinear").
    pub fn present(&mut self, x: &X, overlay: u32, view: Option<View>, filter: &[u8], full: bool) -> Result<()> {
        let c = &x.conn;
        if view != self.transform {
            let v = view.unwrap_or(View::IDENTITY);
            // The transform maps a screen pixel's centre to the scene point it shows.
            let inv = 1.0 / v.scale;
            let t = |i: usize| fixed(v.crop[i] as f64 - v.origin[i] as f64 * inv);
            c.render_set_picture_transform(
                self.source,
                Transform {
                    matrix11: fixed(inv),
                    matrix12: 0,
                    matrix13: t(0),
                    matrix21: 0,
                    matrix22: fixed(inv),
                    matrix23: t(1),
                    matrix31: 0,
                    matrix32: 0,
                    matrix33: fixed(1.0),
                },
            )?;
            c.render_set_picture_filter(self.source, if view.is_some() { filter } else { b"nearest" }, &[])?;
            self.transform = view;
        }
        let clip = if full || view.is_some() { NONE } else { self.damage };
        c.xfixes_set_picture_clip_region(overlay, clip, 0, 0)?;
        c.render_composite(PictOp::SRC, self.source, NONE, overlay, 0, 0, 0, 0, 0, 0, self.width, self.height)?;
        Ok(())
    }
}

/// What lies under the windows: the root pixmap tiled, or a solid colour.
pub struct Background {
    pub picture: Option<u32>,
    pub pixmap: Option<u32>,
    pub color: render::Color,
}

impl Background {
    pub fn new(rgb: u32) -> Self {
        let c = |shift: u32| (((rgb >> shift) & 0xff) * 0x101) as u16;
        Background { picture: None, pixmap: None, color: render::Color { red: c(16), green: c(8), blue: c(0), alpha: 0xffff } }
    }

    /// Uses `pixmap` (the root pixmap) from now on; None means the solid colour.
    pub fn set(&mut self, x: &X, pixmap: Option<u32>) -> Result<()> {
        self.release(x);
        let Some(p) = pixmap else {
            log::info!("no root pixmap; solid background");
            return Ok(());
        };
        let Ok(g) = x.conn.get_geometry(p)?.reply() else {
            log::warn!("root pixmap 0x{p:x} is gone; solid background");
            return Ok(());
        };
        let Some(format) = x.depth_format(g.depth) else {
            log::warn!("root pixmap 0x{p:x} has depth {}; solid background", g.depth);
            return Ok(());
        };
        let picture = x.conn.generate_id()?;
        x.conn.render_create_picture(picture, p, format.id, &render::CreatePictureAux::new().repeat(render::Repeat::NORMAL))?;
        log::info!("background: root pixmap 0x{p:x}, {}x{}", g.width, g.height);
        self.picture = Some(picture);
        self.pixmap = Some(p);
        Ok(())
    }

    pub fn release(&mut self, x: &X) {
        if let Some(p) = self.picture.take() {
            let _ = x.conn.render_free_picture(p);
        }
        self.pixmap = None;
    }
}
