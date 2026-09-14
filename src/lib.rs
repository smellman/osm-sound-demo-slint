//! OpenStreetMap Sound Demo — a native rebuild of
//! <https://github.com/smellman/osm-sound-demo> on MapLibre Native, Slint and
//! rodio.
//!
//! The app lives in a library rather than straight in the binary because
//! Android does not run a `main`: an android-activity app is a `cdylib` that
//! the platform loads and calls `android_main` on. Desktop keeps its binary,
//! and both go through [`run`].

// One backend, named explicitly. `maplibre-native-ffi` ships a separate
// prebuilt native artifact per backend, so this cannot be left to the linker.
#[cfg(not(any(feature = "vulkan", feature = "opengl", feature = "metal")))]
compile_error!(
    "no rendering backend selected: build with --features vulkan (Linux), \
     --features opengl (Linux and Android), or --features metal (macOS and iOS)"
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

pub use app::run;

/// Android's entry point, called by the platform once the activity is up.
///
/// `slint::android::init` has to come before anything that needs a backend,
/// because it is what installs one; on desktop the backend selector finds its
/// own. There is nowhere to print a fatal error to on a phone, so one is logged
/// and the activity is left to close.
/// The activity, kept so the app can ask it where the system bars are. Slint
/// takes ownership of the one it is handed, and this type is a handle to shared
/// state rather than the state itself, so a clone costs nothing.
#[cfg(target_os = "android")]
static ANDROID: std::sync::OnceLock<slint::android::AndroidApp> = std::sync::OnceLock::new();

#[cfg(target_os = "android")]
pub(crate) fn android_app() -> Option<&'static slint::android::AndroidApp> {
    ANDROID.get()
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(android: slint::android::AndroidApp) {
    let _ = ANDROID.set(android.clone());
    // Before Slint takes the app over, while the activity pointer is still to
    // hand: MapLibre Native needs it or every tile request fails.
    if let Err(error) = init_maplibre(&android) {
        eprintln!("the map will not load: {error}");
    }
    if let Err(error) = slint::android::init(android) {
        eprintln!("fatal: the Android backend would not start: {error}");
        return;
    }
    if let Err(error) = run() {
        eprintln!("fatal: {error}");
    }
}

/// Hands MapLibre Native the JVM and Context it needs before it will make an
/// HTTPS request.
///
/// Android has no system-wide certificate store a C++ library can just read;
/// verification goes through the platform, which means through Java. Without
/// this the style load fails with "Android TLS verifier is not initialized" and
/// the map stays black while everything else — including this app's own
/// requests, which go through `ureq` and its own roots — carries on working.
#[cfg(target_os = "android")]
fn init_maplibre(android: &slint::android::AndroidApp) -> Result<(), String> {
    use jni::JavaVM;

    let vm = android.vm_as_ptr();
    let activity = android.activity_as_ptr();
    if vm.is_null() || activity.is_null() {
        return Err("the activity handed over no JVM".to_owned());
    }

    // SAFETY: the pointer comes from the activity this was called for, and the
    // VM outlives the process.
    let vm = unsafe { JavaVM::from_raw(vm.cast()) }.map_err(|error| error.to_string())?;
    // The call has to be made from a thread the JVM knows about.
    let mut env = vm
        .attach_current_thread()
        .map_err(|error| error.to_string())?;

    // SAFETY: both pointers are live for the length of the call, which is all
    // the C API borrows them for. The middle argument is for JNI adapters and
    // is documented as ignorable by direct callers.
    let status = unsafe {
        maplibre_native_ffi_sys::mln_android_init(
            env.get_raw().cast(),
            std::ptr::null_mut(),
            activity.cast(),
        )
    };
    if status == maplibre_native_ffi_sys::MLN_STATUS_OK {
        Ok(())
    } else {
        Err(format!("mln_android_init returned {status}"))
    }
}
