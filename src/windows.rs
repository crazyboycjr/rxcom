//! The top-level windows (children of the root) in stacking order, with the X
//! resources the compositor holds for each.

use x11rb::connection::Connection;
use x11rb::protocol::render::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Rectangle, Window};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border: u16,
}

impl Geometry {
    /// The window's area on the screen, border included, which is also the size
    /// of its pixmap.
    pub fn outer(&self) -> Rectangle {
        let b = 2 * self.border;
        Rectangle { x: self.x, y: self.y, width: self.width + b, height: self.height + b }
    }
}

#[derive(Debug, Default)]
pub struct Win {
    pub id: Window,
    pub geom: Geometry,
    pub visual: u32,
    pub input_only: bool,
    pub mapped: bool,
    /// The overlay window and our own windows: tracked for stacking, never drawn.
    pub ignored: bool,
    /// The bounding region is not the plain rectangle.
    pub shaped: bool,
    pub damage: Option<u32>,
    pub pixmap: Option<u32>,
    pub picture: Option<u32>,
    /// The picture has alpha, so the window blends instead of covering.
    pub alpha: bool,
    /// The picture no longer matches the window (just mapped, resized or reshaped).
    pub stale: bool,
}

impl Win {
    pub fn drawable(&self) -> bool {
        self.mapped && !self.input_only && !self.ignored
    }

    /// Frees the window's pixmap and picture; the damage object stays.
    pub fn release(&mut self, c: &impl Connection) {
        if let Some(p) = self.picture.take() {
            let _ = c.render_free_picture(p);
        }
        if let Some(p) = self.pixmap.take() {
            let _ = c.free_pixmap(p);
        }
    }
}

/// Bottom to top. A desktop has a few dozen windows, so lookups just scan.
#[derive(Default)]
pub struct WinList(Vec<Win>);

impl WinList {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn index(&self, id: Window) -> Option<usize> {
        self.0.iter().position(|w| w.id == id)
    }

    pub fn get(&self, id: Window) -> Option<&Win> {
        self.0.iter().find(|w| w.id == id)
    }

    pub fn get_mut(&mut self, id: Window) -> Option<&mut Win> {
        self.0.iter_mut().find(|w| w.id == id)
    }

    /// New windows start at the top of the stack, as X places them.
    pub fn insert_top(&mut self, w: Win) {
        self.remove(w.id);
        self.0.push(w);
    }

    pub fn remove(&mut self, id: Window) -> Option<Win> {
        Some(self.0.remove(self.index(id)?))
    }

    /// Places `id` directly above `above`, or at the bottom when `above` is None
    /// (0). Returns false when `above` is unknown; `id` then goes on top.
    pub fn restack(&mut self, id: Window, above: Window) -> bool {
        let Some(w) = self.remove(id) else { return false };
        let at = if above == x11rb::NONE { Some(0) } else { self.index(above).map(|i| i + 1) };
        self.0.insert(at.unwrap_or(self.0.len()), w);
        at.is_some()
    }

    pub fn raise(&mut self, id: Window) {
        if let Some(w) = self.remove(id) {
            self.0.push(w);
        }
    }

    pub fn lower(&mut self, id: Window) {
        if let Some(w) = self.remove(id) {
            self.0.insert(0, w);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Win> {
        self.0.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Win> {
        self.0.iter_mut()
    }

    pub fn drain(&mut self) -> Vec<Win> {
        std::mem::take(&mut self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(ids: &[Window]) -> WinList {
        let mut l = WinList::default();
        for &id in ids {
            l.insert_top(Win { id, ..Default::default() });
        }
        l
    }

    fn order(l: &WinList) -> Vec<Window> {
        l.iter().map(|w| w.id).collect()
    }

    #[test]
    fn restack_follows_above_sibling() {
        let mut l = list(&[1, 2, 3, 4]);
        assert!(l.restack(1, 3));
        assert_eq!(order(&l), [2, 3, 1, 4]);
        assert!(l.restack(4, x11rb::NONE));
        assert_eq!(order(&l), [4, 2, 3, 1]);
        assert!(l.restack(4, 1));
        assert_eq!(order(&l), [2, 3, 1, 4]);
        assert!(!l.restack(2, 99));
        assert_eq!(order(&l), [3, 1, 4, 2]);
    }

    #[test]
    fn circulate_and_remove() {
        let mut l = list(&[1, 2, 3]);
        l.raise(1);
        assert_eq!(order(&l), [2, 3, 1]);
        l.lower(3);
        assert_eq!(order(&l), [3, 2, 1]);
        assert!(l.remove(2).is_some());
        assert_eq!(order(&l), [3, 1]);
        l.insert_top(Win { id: 3, ..Default::default() });
        assert_eq!(order(&l), [1, 3]);
    }
}
