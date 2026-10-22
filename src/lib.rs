//! rxcom: an X11 compositor on Vulkan. The binary lives in main.rs; the library
//! exists so the examples (test clients and component tests) can reuse the pieces.

pub mod compositor;
pub mod gpu;
pub mod logger;
pub mod shm;
pub mod windows;
pub mod x;
pub mod zoom;
