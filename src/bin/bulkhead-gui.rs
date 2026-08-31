//! bulkhead-gui -- the same window as `bulkhead gui`, as its own executable.
//!
//! Two binaries because the two audiences do not overlap: the command line is
//! for someone who already knows what they want, the window is for someone
//! holding a broken machine. Shipping one file that does both means whichever
//! they downloaded is the wrong one.
//!
//! The only difference is the subsystem. Without this attribute Windows opens a
//! console behind the window and leaves it there for the life of the program.
//! Everything else lives in the library, and every disk operation is still the
//! bulkhead.exe sitting next to this one.
#![windows_subsystem = "windows"]

fn main() {
    if let Err(e) = bulkhead::gui::run_gui() {
        // No console to print to, so the failure has to be a window or it is
        // nothing at all.
        bulkhead::gui::fatal(&e.to_string());
        std::process::exit(1);
    }
}
