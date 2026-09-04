use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Reusable Slint components (`MMapView`, `MMapAdapter`) come from the
/// maplibre-native-slint submodule, imported as `@maplibre-native-slint/...`
/// exactly as that project documents.
const MAPLIBRE_SLINT: &str = "vendor/maplibre-native-slint/src";

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let library = manifest.join(MAPLIBRE_SLINT);
    if !library.join("maplibre.slint").is_file() {
        panic!(
            "{MAPLIBRE_SLINT} is empty. Run `git submodule update --init \
             vendor/maplibre-native-slint` (no --recursive needed: only the \
             .slint components are used)."
        );
    }
    // The submodule is pinned, so a bumped commit has to trigger a rebuild.
    println!("cargo::rerun-if-changed={}", library.display());

    let config = slint_build::CompilerConfiguration::new().with_library_paths(HashMap::from([(
        "maplibre-native-slint".to_owned(),
        library,
    )]));
    slint_build::compile_with_config(Path::new("ui/app.slint"), config)
        .expect("Slint build failed");
}
