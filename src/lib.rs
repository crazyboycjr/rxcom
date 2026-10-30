//! rxcom: an X11 compositor. The binary lives in main.rs; the library exists so
//! the examples (test clients) can reuse the pieces.

pub mod compositor;
pub mod gl;
pub mod gpu;
pub mod logger;
pub mod scene;
pub mod vt;
pub mod watchdog;
pub mod windows;
pub mod x;
pub mod zoom;
