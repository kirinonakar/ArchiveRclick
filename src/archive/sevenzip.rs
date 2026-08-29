//! 7z.dll archive backend and libarchive routing facade.
//!
//! The Windows implementation is separated by responsibility under
//! `sevenzip/platform_impl`; other platforms expose an unavailable backend
//! while continuing to route supported formats through libarchive.

#[cfg(windows)]
#[path = "sevenzip/platform_impl/mod.rs"]
mod platform_impl;

#[cfg(not(windows))]
#[path = "sevenzip/unsupported.rs"]
mod platform_impl;

pub use platform_impl::{CompositeEngine, SevenZipEngine};
