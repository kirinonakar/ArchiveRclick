pub mod app;
pub mod archive;
pub mod platform;
pub mod tasks;

// slint_build::compile() records only the last generated file in
// SLINT_INCLUDE_GENERATED, so include each generated unit explicitly instead
// of using slint::include_modules!() (which would drop ui/main.slint's types).
include!(concat!(env!("OUT_DIR"), "/main.rs"));
include!(concat!(env!("OUT_DIR"), "/progress_window.rs"));
