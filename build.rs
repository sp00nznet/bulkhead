//! bulkhead-gui.exe asks for elevation when it starts.
//!
//! Every button reads a disk, and an unelevated window let a double-click open
//! normally and then fail every action with "Access is denied". The manifest
//! makes Windows show the UAC prompt up front instead.
//!
//! Only the GUI binary gets it. bulkhead.exe stays asInvoker: `ls` and `cp` on
//! an image file work without elevation, and WinPE runs `bulkhead gui` from the
//! CLI binary. No application manifest beyond the UAC level either: a
//! common-controls v6 dependency would fail to load in WinPE, which ships only
//! the v5 comctl32 that `media` copies in.
//!
//! The linker writes the manifest itself (link.exe and lld-link both do), so no
//! resource compiler or manifest file is needed.
// .cargo/config.toml's /DELAYLOAD:comctl32.dll reaches this build script too,
// which imports nothing from comctl32, and the linker says so.
#![allow(linker_messages)]

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=bulkhead-gui=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=bulkhead-gui=/MANIFESTUAC:level='requireAdministrator'");
    }
}
