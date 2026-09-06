//! Proves the maplibre-native-ffi pipeline end to end before the app is ported
//! onto it: runtime, map, a Metal owned-texture render target, and a CPU
//! read-back that actually contains a map.
//!
//! Run with `cargo run --release --example ffi_smoke`.

use std::time::{Duration, Instant};

use maplibre_native_ffi::{
    CameraOptions, LatLng, MapHandle, MapMode, MapOptions, MetalContextDescriptor,
    MetalOwnedTextureDescriptor, NativePointer, RenderTargetExtent, RuntimeEventMask,
    RuntimeEventPayload, RuntimeEventSource, RuntimeEventType, RuntimeHandle, RuntimeOptions,
};

const STYLE_URL: &str = "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/style.json";
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 720;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime_options = RuntimeOptions::default();
    runtime_options.cache_path = Some(
        std::env::temp_dir()
            .join("osm-sound-demo-ffi-smoke.sqlite")
            .to_string_lossy()
            .into_owned(),
    );
    let mut runtime = RuntimeHandle::with_options(&runtime_options)?;

    let mut map_options = MapOptions::new(WIDTH, HEIGHT, 1.0);
    map_options.mode = MapMode::Continuous;
    let map = MapHandle::with_options(&runtime, &map_options)?;

    map.set_event_mask(
        RuntimeEventMask::MAP_RENDER_UPDATE_AVAILABLE | RuntimeEventMask::MAP_RENDER_FRAME_FINISHED,
    )?;
    map.set_style_url(STYLE_URL)?;

    // MapLibre Native clamps pitch at 60 by default; raise the bound first.
    let mut bounds = maplibre_native_ffi::BoundOptions::default();
    bounds.max_pitch = Some(85.0);
    map.set_bounds(&bounds)?;

    let mut camera = CameraOptions::default();
    camera.center = Some(LatLng::new(35.680655, 139.767165));
    camera.zoom = Some(16.0);
    camera.pitch = Some(85.0);
    map.jump_to(&camera)?;
    map.request_repaint()?;

    // The Metal device the render target allocates its texture on.
    let device = metal_device()?;
    let extent = RenderTargetExtent::new(WIDTH, HEIGHT, 1.0);
    let descriptor = MetalOwnedTextureDescriptor::new(extent, MetalContextDescriptor::new(device));
    // Sessions attach through a reference to the map, not the handle itself.
    let session = map.attach_ref()?.attach_metal_owned_texture(&descriptor)?;

    // Does MapLibre Native honour a pitch past the 60 degrees the old
    // bindings clamped at, or does it clamp internally?
    println!("asked for pitch 85, map reports {:?}", map.camera()?.pitch);

    let started = Instant::now();
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    let mut frames = 0u32;
    let mut ink = 0.0;

    // Give the map ten seconds to fetch and draw, reporting how much of the
    // frame stops being background as tiles land.
    while started.elapsed() < Duration::from_secs(10) {
        runtime.pump(Some(Duration::from_millis(8)), None)?;

        let source = RuntimeEventSource::Map(map.id());
        let mut wants_frame = false;
        for event in runtime.drain_events(0)?.iter() {
            if event.source() != source {
                continue;
            }
            match event.event_type() {
                RuntimeEventType::MapRenderUpdateAvailable => wants_frame = true,
                RuntimeEventType::MapRenderFrameFinished => {
                    if let RuntimeEventPayload::RenderFrame(frame) = event.payload() {
                        wants_frame |= frame.needs_repaint;
                    }
                }
                _ => {}
            }
        }
        if !wants_frame {
            continue;
        }

        session.render_update()?;
        frames += 1;
        let info = session.read_premultiplied_rgba8_into(&mut pixels)?;
        assert_eq!(
            (info.width, info.height),
            (WIDTH, HEIGHT),
            "read back an unexpected size"
        );

        // The toner style paints white behind everything, so anything darker
        // is map that has arrived.
        let dark = pixels
            .as_chunks::<4>()
            .0
            .iter()
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
    assert!(frames > 0, "the map never asked for a frame");
    assert!(ink > 0.05, "the read-back frame is essentially blank");

    drop(session);
    map.close().map_err(|error| error.to_string())?;
    runtime.close().map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn metal_device() -> Result<NativePointer, Box<dyn std::error::Error>> {
    use objc2::rc::Retained;
    use objc2_metal::MTLCreateSystemDefaultDevice;

    // SAFETY: MTLCreateSystemDefaultDevice returns a retained-compatible object.
    let device = unsafe { Retained::retain(MTLCreateSystemDefaultDevice()) }
        .ok_or("MTLCreateSystemDefaultDevice returned nil")?;
    // SAFETY: the pointer stays valid while `device` is alive, and the render
    // target only borrows it for the attach call.
    let pointer = unsafe { NativePointer::from_address(Retained::as_ptr(&device) as usize) };
    std::mem::forget(device);
    Ok(pointer)
}

#[cfg(not(target_os = "macos"))]
fn metal_device() -> Result<NativePointer, Box<dyn std::error::Error>> {
    Err("this smoke test is macOS only".into())
}
