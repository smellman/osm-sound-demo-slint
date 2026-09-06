//! OpenStreetMap Sound Demo — a native rebuild of
//! <https://github.com/smellman/osm-sound-demo> on MapLibre Native, Slint and
//! rodio.

// One backend, named explicitly. `maplibre-native-ffi` ships a separate
// prebuilt native artifact per backend, so this cannot be left to the linker.
#[cfg(not(any(feature = "vulkan", feature = "opengl", feature = "metal")))]
compile_error!(
    "no rendering backend selected: build with --features vulkan (Linux), \
     --features opengl (Linux, for comparison), or --features metal (macOS)"
);
#[cfg(any(
    all(feature = "vulkan", feature = "opengl"),
    all(feature = "vulkan", feature = "metal"),
    all(feature = "opengl", feature = "metal"),
))]
compile_error!("only one rendering backend can be enabled at a time");

mod app;
mod audio;
mod gamepad;
mod locate;
mod map;
mod otherman;
mod stream;

slint::include_modules!();

fn main() {
    if let Err(error) = app::run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}
