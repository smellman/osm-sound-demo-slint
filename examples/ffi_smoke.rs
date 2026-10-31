//! Proves the maplibre-native-ffi pipeline end to end on its own: runtime, map,
//! a Metal owned-texture render target, and a CPU read-back that actually
//! contains a map.
//!
//! Run with `cargo run --release --features metal --example ffi_smoke`.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use maplibre_native_ffi::{
    BoundOptions, CameraOptions, CameraUpdate, FrameDemand, FrameDemandFlag, LatLng, LogicalExtent,
    MapOptions, MetalContextDescriptor, MetalOwnedTextureDescriptor, NativeFuture,
    RenderDriverKind, RenderResult, RenderSessionAttachOptions, RuntimeHandle, RuntimeOptions,
    Wake,
};

const STYLE_URL: &str = "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/style.json";
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 720;
const TIMEOUT: Duration = Duration::from_secs(10);

/// Counts the session's frame wakes, so the loop can sleep until one comes.
#[derive(Default)]
struct Signal {
    count: Mutex<u64>,
    changed: Condvar,
}

impl Signal {
    fn raise(&self) {
        *self.count.lock().unwrap() += 1;
        self.changed.notify_all();
    }

    fn seen(&self) -> u64 {
        *self.count.lock().unwrap()
    }

    fn wait_past(&self, seen: u64, timeout: Duration) {
        let count = self.count.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(count, timeout, |count| *count == seen);
    }
}

fn wait<T>(future: NativeFuture<T>) -> Result<T, Box<dyn std::error::Error>> {
    match future.wait(TIMEOUT) {
        Ok(result) => Ok(result?),
        Err(_) => Err("timed out".into()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cache = std::env::temp_dir()
        .join("osm-sound-demo-ffi-smoke.sqlite")
        .to_string_lossy()
        .into_owned();
    let runtime = RuntimeHandle::create(&RuntimeOptions {
        cache_path: &cache,
        ..Default::default()
    })?;

    let extent = LogicalExtent::new(WIDTH, HEIGHT, 1.0);
    let (map, ready) = runtime.create_map(&MapOptions::new(extent))?;
    wait(ready)?;
    wait(map.set_style_url(STYLE_URL)?)?;

    // MapLibre Native clamps pitch at 60 by default; raise the bound first.
    wait(map.set_bounds(&BoundOptions {
        max_pitch: Some(85.0),
        ..Default::default()
    })?)?;
    wait(map.update_camera(&CameraUpdate {
        camera: CameraOptions {
            center: Some(LatLng::new(35.680655, 139.767165)),
            zoom: Some(16.0),
            pitch: Some(85.0),
            ..Default::default()
        },
        ..Default::default()
    })?)?;

    // Does MapLibre Native honour a pitch past the 60 degrees the old
    // bindings clamped at, or does it clamp internally?
    let camera = wait(map.get_camera()?)?.camera;
    println!("asked for pitch 85, map reports {}", camera.pitch);

    // The Metal device the render target allocates its texture on.
    let device = metal_device()?;
    let signal = Arc::new(Signal::default());
    let wake = {
        let signal = Arc::clone(&signal);
        Wake::new(move || signal.raise())
    };
    let options = RenderSessionAttachOptions {
        frame_wake: wake,
        ..RenderSessionAttachOptions::new(RenderDriverKind::CoreWorker)
    };
    let descriptor = MetalOwnedTextureDescriptor {
        extent,
        context: MetalContextDescriptor { device },
        // MTLPixelFormatBGRA8Unorm.
        pixel_format: 80,
    };
    // SAFETY: the device is leaked in `metal_device`, so it outlives the session.
    let (session, attached) = unsafe { map.attach_metal_owned_texture(&descriptor, &options)? };
    wait(attached)?;

    let started = Instant::now();
    let mut frames = 0u32;
    let mut ink = 0.0;
    let mut token = 1;
    let mut armed = false;

    // Give the map ten seconds to fetch and draw, reporting how much of the
    // frame stops being background as tiles land. One demand at a time waits
    // for the map's next update, so every update it publishes is drawn.
    while started.elapsed() < TIMEOUT {
        let seen = signal.seen();
        if !armed {
            session.request_frame(&FrameDemand {
                flags: FrameDemandFlag::WAIT_FOR_UPDATE,
                token,
            })?;
            armed = true;
        }
        let results = session.drain_frame_results()?;
        if results.is_empty() {
            signal.wait_past(seen, Duration::from_millis(100));
            continue;
        }
        let mut rendered = false;
        for result in results {
            if result.token == token {
                armed = false;
                token += 1;
            }
            rendered |= result.disposition == RenderResult::Rendered;
        }
        if !rendered {
            continue;
        }

        frames += 1;
        let frame = wait(session.read_texture()?)?;
        assert_eq!(
            (frame.info.width, frame.info.height),
            (WIDTH, HEIGHT),
            "read back an unexpected size"
        );

        // The toner style paints white behind everything, so anything darker
        // is map that has arrived.
        let row = WIDTH as usize * 4;
        let dark = frame
            .data
            .chunks(frame.info.stride as usize)
            .flat_map(|line| line[..row].as_chunks::<4>().0)
            .filter(|pixel| pixel[0] < 220 || pixel[1] < 220 || pixel[2] < 220)
            .count();
        ink = dark as f64 / (WIDTH * HEIGHT) as f64;
        if frames.is_multiple_of(30) {
            println!("{frames} frames, {:.1}% ink", ink * 100.0);
        }
    }

    println!(
        "done: {frames} frames in {:?}, {:.1}% ink",
        started.elapsed(),
        ink * 100.0
    );
    assert!(frames > 0, "the map never drew a frame");
    assert!(ink > 0.05, "the read-back frame is essentially blank");

    wait(session.detach()?)?;
    wait(session.release().map_err(|(_, error)| error)?)?;
    wait(map.release().map_err(|(_, error)| error)?)?;
    wait(runtime.release().map_err(|(_, error)| error)?)?;
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn metal_device() -> Result<*mut std::ffi::c_void, Box<dyn std::error::Error>> {
    use objc2::rc::Retained;
    use objc2_metal::MTLCreateSystemDefaultDevice;

    // SAFETY: MTLCreateSystemDefaultDevice returns a retained-compatible object.
    let device = unsafe { Retained::retain(MTLCreateSystemDefaultDevice()) }
        .ok_or("MTLCreateSystemDefaultDevice returned nil")?;
    // Leaked, so the pointer stays valid for as long as the session uses it.
    let pointer = Retained::as_ptr(&device) as *mut std::ffi::c_void;
    std::mem::forget(device);
    Ok(pointer)
}

#[cfg(not(target_vendor = "apple"))]
fn metal_device() -> Result<*mut std::ffi::c_void, Box<dyn std::error::Error>> {
    Err("this smoke test needs a Metal device".into())
}
