//! OpenStreetMap Sound Demo — a native rebuild of
//! <https://github.com/smellman/osm-sound-demo> on MapLibre Native, Slint and
//! rodio.

mod app;
mod audio;
mod gamepad;
mod map;
mod otherman;

slint::include_modules!();

fn main() {
    if let Err(error) = app::run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}
