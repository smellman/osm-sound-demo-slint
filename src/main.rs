//! The desktop binary. Everything it does lives in the library beside it, which
//! Android loads instead — see `src/lib.rs`.

fn main() {
    if let Err(error) = osm_sound_demo_slint::run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}
