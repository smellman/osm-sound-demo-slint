use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    slint_build::compile("ui/app.slint").expect("Slint build failed");
    bundle_style_resources();
    shared_frames_cfg();
}

/// Sets `shared_frames` where the map's frames can reach the window as
/// textures rather than being read back: Vulkan on Linux, Metal on macOS.
/// See `src/map/shared.rs`.
fn shared_frames_cfg() {
    println!("cargo::rustc-check-cfg=cfg(shared_frames)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let feature = |name: &str| std::env::var_os(format!("CARGO_FEATURE_{name}")).is_some();
    if (os == "linux" && feature("VULKAN")) || (os == "macos" && feature("METAL")) {
        println!("cargo::rustc-cfg=shared_frames");
    }
}

/// Writes the table `src/map/bundled.rs` includes: the glyph ranges under
/// `style/fonts` and the sprite under `style/sprites`, keyed by their paths
/// below `style/` and sorted so the lookup can binary-search them.
fn bundle_style_resources() {
    let style = Path::new(env!("CARGO_MANIFEST_DIR")).join("style");
    let mut files = Vec::new();
    for dir in ["fonts", "sprites"] {
        let root = style.join(dir);
        println!("cargo:rerun-if-changed={}", root.display());
        collect(&root, &mut files);
    }
    files.sort();

    let mut table = String::from("&[\n");
    for path in &files {
        // Directories are watched for added and removed files; a file is
        // watched for its contents.
        if let Some(parent) = path.parent() {
            println!("cargo:rerun-if-changed={}", parent.display());
        }
        let key = path
            .strip_prefix(&style)
            .expect("collected under style/")
            .to_str()
            .expect("bundled file names are UTF-8")
            .replace('\\', "/");
        table.push_str(&format!(
            "    ({key:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    table.push(']');

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set"));
    fs::write(out.join("bundled_files.rs"), table).expect("writing the bundled file table");
}

/// What the map asks for: glyph ranges, and the sprite's index and image. The
/// license texts beside them stay out of the binary.
const BUNDLED_EXTENSIONS: [&str; 3] = ["pbf", "json", "png"];

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|extension| {
            BUNDLED_EXTENSIONS
                .iter()
                .any(|bundled| extension == *bundled)
        }) {
            files.push(path);
        }
    }
}
