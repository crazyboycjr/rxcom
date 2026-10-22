//! The top-level windows (children of the root) in stacking order, with the X
//! and GPU resources the compositor holds for each.

use crate::gpu::texture::Texture;
use std::collections::HashMap;
use x11rb::protocol::xproto::Window;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border: u16,
}

impl Geometry {
    /// The size of the window pixmap, which includes the border.
    pub fn outer_size(&self) -> (u32, u32) {
        let b = 2 * self.border as u32;
        (self.width as u32 + b, self.height as u32 + b)
    }
}

/// How the window's pixels reach the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The pixmap's dma-buf is imported; the GPU samples it in place.
    DmaBuf,
    /// Pixels are copied with MIT-SHM GetImage and uploaded after each damage.
    Shm,
}

#[derive(Debug)]
pub struct Win {
    pub id: Window,
    pub geom: Geometry,
    pub depth: u8,
    pub input_only: bool,
    pub override_redirect: bool,
    pub mapped: bool,
    /// The overlay window and our own windows: tracked for stacking, never drawn.
    pub ignored: bool,
    pub damage: Option<u32>,
    pub pixmap: Option<u32>,
    /// The named pixmap no longer matches the window (just mapped or resized).
    pub pixmap_stale: bool,
    /// Damaged since the last frame.
    pub content_dirty: bool,
    pub source: Option<Source>,
    pub tex: Option<Texture>,
}

impl Win {
    pub fn new(id: Window, geom: Geometry, depth: u8) -> Self {
        Win {
            id,
            geom,
            depth,
            input_only: false,
            override_redirect: false,
            mapped: false,
            ignored: false,
            damage: None,
            pixmap: None,
            pixmap_stale: false,
            content_dirty: false,
            source: None,
            tex: None,
        }
    }

    pub fn drawable(&self) -> bool {
        self.mapped && !self.input_only && !self.ignored
    }
}

#[derive(Default)]
pub struct WinList {
    /// Bottom to top.
    order: Vec<Window>,
    map: HashMap<Window, Win>,
}

impl WinList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, id: Window) -> bool {
        self.map.contains_key(&id)
    }

    pub fn get(&self, id: Window) -> Option<&Win> {
        self.map.get(&id)
    }

    pub fn get_mut(&mut self, id: Window) -> Option<&mut Win> {
        self.map.get_mut(&id)
    }

    /// New windows start at the top of the stack, as X places them.
    pub fn insert_top(&mut self, w: Win) {
        let id = w.id;
        if self.map.insert(id, w).is_some() {
            self.order.retain(|&o| o != id);
        }
        self.order.push(id);
    }

    pub fn remove(&mut self, id: Window) -> Option<Win> {
        let w = self.map.remove(&id)?;
        self.order.retain(|&o| o != id);
        Some(w)
    }

    /// Places `id` directly above `above`, or at the bottom when `above` is None (0).
    /// Returns false when either window is unknown.
    pub fn restack(&mut self, id: Window, above: Window) -> bool {
        let Some(from) = self.order.iter().position(|&o| o == id) else {
            return false;
        };
        self.order.remove(from);
        if above == x11rb::NONE {
            self.order.insert(0, id);
            return true;
        }
        match self.order.iter().position(|&o| o == above) {
            Some(i) => {
                self.order.insert(i + 1, id);
                true
            }
            None => {
                self.order.push(id);
                false
            }
        }
    }

    pub fn raise(&mut self, id: Window) {
        if let Some(i) = self.order.iter().position(|&o| o == id) {
            self.order.remove(i);
            self.order.push(id);
        }
    }

    pub fn lower(&mut self, id: Window) {
        if let Some(i) = self.order.iter().position(|&o| o == id) {
            self.order.remove(i);
            self.order.insert(0, id);
        }
    }

    /// Window ids bottom to top.
    pub fn order(&self) -> &[Window] {
        &self.order
    }

    pub fn iter(&self) -> impl Iterator<Item = &Win> {
        self.order.iter().filter_map(|id| self.map.get(id))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Win> {
        self.map.values_mut()
    }

    pub fn drain(&mut self) -> Vec<Win> {
        self.order.clear();
        self.map.drain().map(|(_, w)| w).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(ids: &[Window]) -> WinList {
        let mut l = WinList::new();
        for &id in ids {
            l.insert_top(Win::new(id, Geometry::default(), 24));
        }
        l
    }

    #[test]
    fn restack_follows_above_sibling() {
        let mut l = list(&[1, 2, 3, 4]);
        assert!(l.restack(1, 3));
        assert_eq!(l.order(), &[2, 3, 1, 4]);
        assert!(l.restack(4, x11rb::NONE));
        assert_eq!(l.order(), &[4, 2, 3, 1]);
        assert!(l.restack(4, 1));
        assert_eq!(l.order(), &[2, 3, 1, 4]);
        assert!(!l.restack(2, 99));
        assert_eq!(l.order(), &[3, 1, 4, 2]);
    }

    #[test]
    fn circulate_and_remove() {
        let mut l = list(&[1, 2, 3]);
        l.raise(1);
        assert_eq!(l.order(), &[2, 3, 1]);
        l.lower(3);
        assert_eq!(l.order(), &[3, 2, 1]);
        assert!(l.remove(2).is_some());
        assert_eq!(l.order(), &[3, 1]);
        l.insert_top(Win::new(3, Geometry::default(), 24));
        assert_eq!(l.order(), &[1, 3]);
    }
}
