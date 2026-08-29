//! Dynamically loaded libarchive backend.
//!
//! Archive parsing and encoding are delegated to libarchive, but extraction is
//! deliberately performed with Rust filesystem APIs.  This keeps every output
//! path behind the validation in `path_safety` and prevents archive-provided
//! links or special files from ever reaching a disk writer.

#[cfg(windows)]
#[path = "libarchive/windows/mod.rs"]
mod platform_impl;

#[cfg(not(windows))]
#[path = "libarchive/unsupported.rs"]
mod platform_impl;

pub use platform_impl::{LibArchiveEngine, load};
