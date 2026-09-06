//! Headless MapLibre Native rendering, driven from a dedicated thread.
//!
//! Adapted from the Rust reference implementation in
//! <https://github.com/maplibre/maplibre-native-slint>, extended with the
//! sound-reactive 3D building layers of the original web demo.
//!
//! The map runs in MapLibre Native's *continuous* mode, which keeps the map
//! alive between frames. The still (`renderStill`) mode re-renders from scratch
//! and re-lays out the building tiles on every change to the layer set, which
//! costs about 40 ms a frame here; continuous mode does the same work in under
//! 8 ms (see `report_static_vs_continuous`).
//!
//! MapLibre Native drives its work through its own run loop, which on macOS is
//! the process CoreFoundation run loop. Pumping that from inside a Slint
//! callback re-enters winit's event handling and aborts the process, so the
//! renderer lives on its own thread: the UI thread only posts commands and
//! picks up finished frames.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use maplibre_native_ffi::{
    CameraOptions, LatLng, MapHandle, MapMode, MapOptions, MetalContextDescriptor,
    MetalOwnedTextureDescriptor, NativePointer, RenderSessionHandle, RenderTargetExtent,
    RuntimeEventMask, RuntimeEventPayload, RuntimeEventSource, RuntimeEventType, RuntimeHandle,
    RuntimeOptions,
};

use crate::Size;
use crate::audio::BINS;

pub const DEFAULT_STYLE_URL: &str =
    "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/style.json";

/// The style the map opens with, `MAPLIBRE_STYLE_URL` overriding it.
fn default_style_url() -> String {
    std::env::var("MAPLIBRE_STYLE_URL")
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_STYLE_URL.to_owned())
}

/// Vector source and source-layer holding building footprints in the
/// OpenMapTiles schema used by tile.openstreetmap.jp.
const BUILDING_SOURCE: &str = "openmaptiles";
const BUILDING_SOURCE_LAYER: &str = "building";

/// Buildings are split into `BINS` layers by their true height, so each
/// frequency band drives its own slice of the skyline.
const MAX_BUILDING_HEIGHT: f64 = 200.0;

/// A band counts as unchanged until its target moves by more than this many
/// metres, or this many degrees of hue, which keeps the animation from setting
/// properties that would not be visible.
const HEIGHT_EPSILON: f64 = 2.0;
const HUE_EPSILON: f64 = 4.0;

/// How long the render thread waits for a command before turning the runtime
/// anyway, so in-flight tile requests keep progressing while the map is idle.
const IDLE_TICK: Duration = Duration::from_millis(16);

/// How long each runtime pump may block waiting for work. Short, because the
/// thread has a frame to draw afterwards.
const PUMP_BUDGET: Duration = Duration::from_millis(2);

const MIN_ZOOM: f64 = 0.0;
const MAX_ZOOM: f64 = 22.0;
const MIN_PITCH: f64 = 0.0;
/// MapLibre Native clamps the camera at 60°, so the web demo's 70° is not
/// reachable here.
const MAX_PITCH: f64 = 60.0;
const MAX_ABS_LAT: f64 = 85.0;
const WHEEL_STEP: f64 = 0.5;
const DOUBLE_CLICK_STEP: f64 = 1.0;

/// Fly-to duration bounds, and how much duration each degree of travel adds.
///
/// The Rust bindings expose only `jumpTo`, so a fly-to is eased here. It is not
/// merely cosmetic: jumping outruns tile loading and lands on a blank map, the
/// same problem the Raspberry Pi port describes when it defaults `MAPLIBRE_FLY_MS`
/// to six seconds. `MAPLIBRE_FLY_MS` overrides the whole duration here too, for
/// machines that need longer.
const FLY_MIN: Duration = Duration::from_millis(1500);
const FLY_MAX: Duration = Duration::from_millis(6000);
const FLY_MS_PER_DEGREE: f64 = 45.0;

/// How far a long fly-to zooms out at its midpoint, so the trip passes over
/// coarse tiles that are already cached instead of streaming a whole city.
const FLY_ARC_MAX_ZOOM_OUT: f64 = 3.0;
const FLY_ARC_DEGREES_PER_LEVEL: f64 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapCamera {
    pub lat: f64,
    pub lon: f64,
    pub zoom: f64,
    pub bearing: f64,
    pub pitch: f64,
}

impl Default for MapCamera {
    fn default() -> Self {
        Self {
            lat: 35.680655,
            lon: 139.767165,
            zoom: 16.0,
            bearing: 0.0,
            pitch: MAX_PITCH,
        }
    }
}

/// One frequency band's contribution to the skyline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    /// Extrusion height in metres.
    pub height: f64,
    /// Hue in degrees.
    pub hue: f64,
    /// Normalized band level, driving saturation and lightness.
    pub level: f64,
}

impl Default for Band {
    fn default() -> Self {
        Self {
            height: 0.0,
            hue: 0.0,
            level: 0.0,
        }
    }
}

impl Band {
    fn close_to(self, other: Self) -> bool {
        (self.height - other.height).abs() < HEIGHT_EPSILON
            && (self.hue - other.hue).abs() < HUE_EPSILON
    }
}

/// A rendered map image, handed from the render thread to the UI thread.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

enum Command {
    Resize(u32, u32),
    Style(String),
    Camera(MapCamera),
    Bands(Box<[Band; BINS]>),
}

#[derive(Debug)]
struct DragState {
    x: f32,
    y: f32,
}

/// A fly-to in progress.
#[derive(Debug)]
struct Flight {
    from: MapCamera,
    to: MapCamera,
    /// Signed shortest-path longitude delta, so a fly can cross the antimeridian.
    lon_delta: f64,
    /// Zoom levels to pull back at the midpoint.
    arc: f64,
    elapsed: Duration,
    duration: Duration,
}

/// Transient camera offsets, currently driven by the drop effect. Kept apart
/// from the user's camera so an effect can never leave the map somewhere
/// unexpected once it decays.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CameraBoost {
    pub zoom: f64,
    pub pitch: f64,
    pub bearing: f64,
}

/// Camera state and the pointer interactions that change it. Kept free of any
/// rendering so it can be exercised in tests.
#[derive(Debug, Default)]
struct CameraController {
    camera: MapCamera,
    boost: CameraBoost,
    drag_state: Option<DragState>,
    flight: Option<Flight>,
}

impl CameraController {
    /// The camera as it appears on screen: the base camera plus any boost.
    fn effective(&self) -> MapCamera {
        MapCamera {
            lat: self.camera.lat,
            lon: self.camera.lon,
            zoom: clamp_zoom(self.camera.zoom + self.boost.zoom),
            bearing: normalize_bearing(self.camera.bearing + self.boost.bearing),
            pitch: clamp_pitch(self.camera.pitch + self.boost.pitch),
        }
    }

    /// Starts an eased fly to the given camera.
    fn fly_to(&mut self, lat: f64, lon: f64, zoom: f64) {
        let to = MapCamera {
            lat: clamp_lat(lat),
            lon: normalize_lon(lon),
            zoom: clamp_zoom(zoom),
            ..self.camera
        };
        let lon_delta = shortest_lon_delta(self.camera.lon, to.lon);
        let travel = (to.lat - self.camera.lat).hypot(lon_delta);
        self.drag_state = None;
        self.flight = Some(Flight {
            from: self.camera,
            to,
            lon_delta,
            arc: (travel / FLY_ARC_DEGREES_PER_LEVEL).min(FLY_ARC_MAX_ZOOM_OUT),
            elapsed: Duration::ZERO,
            duration: fly_duration(travel),
        });
    }

    /// Advances an in-progress fly-to by `delta`, returning whether the camera
    /// moved.
    fn advance_flight(&mut self, delta: Duration) -> bool {
        let Some(flight) = self.flight.as_mut() else {
            return false;
        };
        flight.elapsed += delta;
        if flight.elapsed >= flight.duration {
            let to = flight.to;
            self.flight = None;
            self.camera.lat = to.lat;
            self.camera.lon = to.lon;
            self.camera.zoom = to.zoom;
            return true;
        }

        let t = flight.elapsed.as_secs_f64() / flight.duration.as_secs_f64();
        let eased = t * t * (3.0 - 2.0 * t);
        self.camera.lat = clamp_lat(flight.from.lat + (flight.to.lat - flight.from.lat) * eased);
        self.camera.lon = normalize_lon(flight.from.lon + flight.lon_delta * eased);
        let target = flight.from.zoom + (flight.to.zoom - flight.from.zoom) * eased;
        self.camera.zoom = clamp_zoom(target - flight.arc * (std::f64::consts::PI * t).sin());
        true
    }

    #[cfg(test)]
    fn jump_for_test(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.flight = None;
        self.camera.lat = clamp_lat(lat);
        self.camera.lon = normalize_lon(lon);
        self.camera.zoom = clamp_zoom(zoom);
    }

    fn mouse_moved(&mut self, x: f32, y: f32) -> bool {
        let Some(last) = self.drag_state.as_mut() else {
            return false;
        };
        self.flight = None;
        let dx = f64::from(x - last.x);
        let dy = f64::from(y - last.y);
        last.x = x;
        last.y = y;

        // Screen-space drag has to be un-rotated by the bearing on screen,
        // otherwise the map runs off sideways while the demo spins the camera.
        let view = self.effective();
        let bearing = view.bearing.to_radians();
        let east = dx * bearing.cos() - dy * bearing.sin();
        let north = -dx * bearing.sin() - dy * bearing.cos();

        let (lon_per_px, lat_per_px) = degrees_per_pixel(view.zoom, view.lat);
        self.camera.lon = normalize_lon(self.camera.lon - east * lon_per_px);
        self.camera.lat = clamp_lat(self.camera.lat - north * lat_per_px);
        true
    }

    /// Pans by a screen-space delta in pixels, as a drag would.
    fn pan_by(&mut self, dx: f64, dy: f64) {
        self.flight = None;
        let view = self.effective();
        let bearing = view.bearing.to_radians();
        let east = dx * bearing.cos() - dy * bearing.sin();
        let north = -dx * bearing.sin() - dy * bearing.cos();

        let (lon_per_px, lat_per_px) = degrees_per_pixel(view.zoom, view.lat);
        self.camera.lon = normalize_lon(self.camera.lon - east * lon_per_px);
        self.camera.lat = clamp_lat(self.camera.lat - north * lat_per_px);
    }

    fn wheel_zoomed(&mut self, delta: f32) -> bool {
        if delta == 0.0 {
            return false;
        }
        self.flight = None;
        let direction = if delta > 0.0 { -1.0 } else { 1.0 };
        self.camera.zoom = clamp_zoom(self.camera.zoom + direction * WHEEL_STEP);
        true
    }

    fn double_clicked(&mut self, shift: bool) {
        self.flight = None;
        let step = if shift {
            -DOUBLE_CLICK_STEP
        } else {
            DOUBLE_CLICK_STEP
        };
        self.camera.zoom = clamp_zoom(self.camera.zoom + step);
    }
}

/// UI-thread handle to the map. Every mutation is forwarded to the render
/// thread; finished frames are picked up with [`MapLibre::take_frame`].
pub struct MapLibre {
    /// `None` once shutdown has started, which is what tells the render thread
    /// to return.
    commands: Option<Sender<Command>>,
    render_thread: Option<JoinHandle<()>>,
    frames: Receiver<Frame>,
    /// Frames the render thread has finished, whether or not the UI picked them
    /// up. Compared against the UI's own count, this says which side of the
    /// channel a low frame rate is coming from.
    rendered: Arc<AtomicU64>,
    controller: CameraController,
    size: (u32, u32),
}

impl MapLibre {
    fn new(size: (u32, u32)) -> Self {
        let (commands, command_rx) = std::sync::mpsc::channel();
        // A single slot: if the UI falls behind there is no point queueing stale
        // frames, the newest one is always the one worth showing.
        let (frame_tx, frames) = sync_channel(1);
        let rendered = Arc::new(AtomicU64::new(0));
        let handle = std::thread::Builder::new()
            .name("maplibre-render".to_owned())
            .spawn({
                let rendered = Arc::clone(&rendered);
                move || render_thread(size, command_rx, frame_tx, &rendered)
            })
            .expect("spawning the map render thread");

        Self {
            commands: Some(commands),
            render_thread: Some(handle),
            frames,
            rendered,
            controller: CameraController::default(),
            size,
        }
    }

    fn send(&self, command: Command) {
        // The render thread only goes away when the app is shutting down.
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }

    fn push_camera(&self) {
        self.send(Command::Camera(self.controller.effective()));
    }

    /// The camera as it appears on screen, boost included.
    pub fn camera(&self) -> MapCamera {
        self.controller.effective()
    }

    /// Applies transient camera offsets. Passing [`CameraBoost::default`]
    /// returns the camera to the user's own position.
    pub fn set_boost(&mut self, boost: CameraBoost) {
        if self.controller.boost == boost {
            return;
        }
        self.controller.boost = boost;
        self.push_camera();
    }

    /// Frames finished by the render thread since startup. The UI counts the
    /// ones it actually showed, and the gap between the two is the number of
    /// frames dropped for want of a taker.
    pub fn rendered_count(&self) -> u64 {
        self.rendered.load(Ordering::Relaxed)
    }

    /// Takes the newest finished frame, if the render thread produced one since
    /// the last call.
    pub fn take_frame(&mut self) -> Option<Frame> {
        let mut newest = None;
        while let Ok(frame) = self.frames.try_recv() {
            newest = Some(frame);
        }
        newest
    }

    pub fn load_style(&mut self, style_url: &str) {
        self.send(Command::Style(style_url.to_owned()));
    }

    pub fn resize(&mut self, size: Size) {
        let new_size = safe_size(size);
        if self.size == new_size {
            return;
        }
        self.size = new_size;
        self.send(Command::Resize(new_size.0, new_size.1));
    }

    /// Starts an eased fly to the given camera. Advanced by
    /// [`MapLibre::advance_flight`] from the UI's frame tick.
    pub fn fly_to(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.controller.fly_to(lat, lon, zoom);
    }

    /// Advances an in-progress fly-to. Returns whether the camera moved.
    pub fn advance_flight(&mut self, delta: Duration) -> bool {
        if self.controller.advance_flight(delta) {
            self.push_camera();
            return true;
        }
        false
    }

    pub fn flying(&self) -> bool {
        self.controller.flight.is_some()
    }

    pub fn set_pitch(&mut self, pitch: f64) {
        self.controller.camera.pitch = clamp_pitch(pitch);
        self.push_camera();
    }

    pub fn set_bearing(&mut self, bearing: f64) {
        self.controller.camera.bearing = normalize_bearing(bearing);
        self.push_camera();
    }

    /// Pans by a screen-space delta in pixels. Used by the gamepad's left
    /// stick, which drives the map the same way a drag does.
    pub fn pan_by(&mut self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.controller.pan_by(dx, dy);
        self.push_camera();
    }

    /// Adds to the zoom level. Used by the gamepad's D-pad.
    pub fn nudge_zoom(&mut self, delta: f64) {
        if delta == 0.0 {
            return;
        }
        self.controller.flight = None;
        self.controller.camera.zoom = clamp_zoom(self.controller.camera.zoom + delta);
        self.push_camera();
    }

    /// Adds to the bearing. Both the demo's own spin and the gamepad's right
    /// stick go through here, so they compose instead of overwriting each other.
    pub fn nudge_bearing(&mut self, delta: f64) {
        if delta == 0.0 {
            return;
        }
        self.controller.camera.bearing = normalize_bearing(self.controller.camera.bearing + delta);
        self.push_camera();
    }

    pub fn mouse_pressed(&mut self, x: f32, y: f32) {
        self.controller.drag_state = Some(DragState { x, y });
    }

    pub fn mouse_released(&mut self) {
        self.controller.drag_state = None;
    }

    pub fn mouse_moved(&mut self, x: f32, y: f32) {
        if self.controller.mouse_moved(x, y) {
            self.push_camera();
        }
    }

    pub fn wheel_zoomed(&mut self, delta: f32) {
        if self.controller.wheel_zoomed(delta) {
            self.push_camera();
        }
    }

    pub fn double_clicked(&mut self, shift: bool) {
        self.controller.double_clicked(shift);
        self.push_camera();
    }

    /// Applies one frame of the sound animation: every band gets its own
    /// extrusion height and hue.
    ///
    /// `hue_offset` rotates the colour wheel over time — the native API exposes
    /// neither light settings nor paint-property setters, so the hue of the
    /// buildings themselves stands in for the web demo's animated `setLight`.
    /// `height_gain` scales the whole skyline, which the drop effect uses to
    /// make it jump.
    pub fn apply_levels(&mut self, levels: &[f32; BINS], hue_offset: f64, height_gain: f64) {
        let mut bands = [Band::default(); BINS];
        for (band, (slot, level)) in bands.iter_mut().zip(levels.iter()).enumerate() {
            let level = f64::from(*level);
            *slot = Band {
                height: (10.0 + 4.0 * band as f64 + level * 255.0) * height_gain,
                hue: (hue_offset + band as f64 * 6.0).rem_euclid(360.0),
                level,
            };
        }
        self.send(Command::Bands(Box::new(bands)));
    }

    /// Resets every band back to the flat, unlit state used when nothing plays.
    pub fn reset_levels(&mut self) {
        self.send(Command::Bands(Box::new([Band::default(); BINS])));
    }
}

impl Drop for MapLibre {
    fn drop(&mut self) {
        // Dropping the sender is what ends the render thread's loop; waiting
        // for it means MapLibre Native tears down its renderer and closes the
        // tile cache properly instead of being cut off mid-write. It parks on a
        // 16 ms timeout, so this returns promptly.
        self.commands = None;
        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }
    }
}

pub fn create_map(size: Size) -> Rc<RefCell<MapLibre>> {
    Rc::new(RefCell::new(MapLibre::new(safe_size(size))))
}

/// Owns the MapLibre Native renderer for the lifetime of the render thread.
/// Owns the MapLibre Native runtime, map and render session for the lifetime of
/// the render thread. All three handles are thread-affine, so they never leave
/// it.
struct Engine {
    runtime: Option<RuntimeHandle>,
    map: Option<Attached>,
    cache: PathBuf,
    size: (u32, u32),
    style_url: String,
    camera: MapCamera,
    bands: [Band; BINS],
    applied: [Option<Band>; BINS],
    /// Something the UI asked for has not been drawn yet.
    dirty: bool,
    /// MapLibre Native says it has more to draw — tiles still arriving, or a
    /// transition in flight. Unlike the old bindings, this is a real signal
    /// rather than a settle timer.
    wants_repaint: bool,
    /// Reused between frames; the read-back is the same size every time.
    pixels: Vec<u8>,
}

/// A map with a render target attached. Kept together because the session is
/// bound to the map, and a resize replaces both.
struct Attached {
    map: MapHandle,
    session: RenderSessionHandle,
    /// The band layers exist and can be updated in place.
    layers: bool,
    /// The style has finished loading, so its sources exist and layers
    /// referring to them can be added.
    style_loaded: bool,
}

impl Engine {
    fn new(size: (u32, u32)) -> Self {
        Self {
            runtime: None,
            map: None,
            cache: cache_path(),
            size,
            style_url: default_style_url(),
            camera: MapCamera::default(),
            bands: [Band::default(); BINS],
            applied: [None; BINS],
            dirty: true,
            wants_repaint: false,
            pixels: Vec::new(),
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Resize(width, height) => {
                if self.size != (width, height) {
                    self.size = (width, height);
                    // A map's size is fixed at creation and an owned texture is
                    // allocated at one extent, so a resize replaces both. The
                    // runtime, and with it the tile cache, survives.
                    self.map = None;
                    self.applied = [None; BINS];
                    self.mark_dirty();
                }
            }
            Command::Style(url) => {
                if self.style_url != url {
                    self.style_url = url;
                    if let Some(attached) = &mut self.map {
                        if let Err(error) = attached.map.set_style_url(&self.style_url) {
                            eprintln!("style load failed: {error}");
                        }
                        attached.layers = false;
                        attached.style_loaded = false;
                    }
                    self.applied = [None; BINS];
                    self.mark_dirty();
                }
            }
            Command::Camera(camera) => {
                if self.camera != camera {
                    self.camera = camera;
                    self.mark_dirty();
                }
            }
            Command::Bands(bands) => {
                if self.bands != *bands {
                    self.bands = *bands;
                    self.mark_dirty();
                }
            }
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether another frame is worth rendering.
    fn wants_frame(&self) -> bool {
        self.dirty || self.wants_repaint
    }

    /// Turns the runtime, letting tile loads and transitions make progress, and
    /// picks up whether the map wants another frame.
    fn pump(&mut self, timeout: Option<Duration>) {
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };
        if let Err(error) = runtime.pump(timeout, None) {
            eprintln!("pumping the map runtime failed: {error}");
            return;
        }

        let Some(attached) = &self.map else { return };
        let source = RuntimeEventSource::Map(attached.map.id());
        let batch = match runtime.drain_events(0) {
            Ok(batch) => batch,
            Err(error) => {
                eprintln!("draining map events failed: {error}");
                return;
            }
        };
        let mut wants_repaint = false;
        let mut style_loaded = false;
        for event in batch.iter() {
            if event.source() != source {
                continue;
            }
            match event.event_type() {
                RuntimeEventType::MapRenderUpdateAvailable => wants_repaint = true,
                RuntimeEventType::MapStyleLoaded => style_loaded = true,
                RuntimeEventType::MapRenderFrameFinished => {
                    if let RuntimeEventPayload::RenderFrame(frame) = event.payload() {
                        wants_repaint |= frame.needs_repaint;
                    }
                }
                _ => {}
            }
        }
        self.wants_repaint = wants_repaint;
        if style_loaded && let Some(attached) = &mut self.map {
            attached.style_loaded = true;
            // The style's sources exist now, so the band layers can go on.
            self.dirty = true;
        }
    }

    /// Renders one frame, first bringing the band layers up to date.
    fn render(&mut self) -> Option<Frame> {
        self.ensure_map()?;
        self.sync_bands();

        let camera = camera_options(self.camera);
        let attached = self.map.as_ref()?;
        if let Err(error) = attached.map.jump_to(&camera) {
            eprintln!("moving the camera failed: {error}");
        }
        if let Err(error) = attached.session.render_update() {
            eprintln!("rendering failed: {error}");
            self.dirty = false;
            return None;
        }

        let (width, height) = self.size;
        let expected = width as usize * height as usize * 4;
        if self.pixels.len() != expected {
            self.pixels = vec![0; expected];
        }
        let info = match attached
            .session
            .read_premultiplied_rgba8_into(&mut self.pixels)
        {
            Ok(info) => info,
            Err(error) => {
                eprintln!("reading the frame back failed: {error}");
                self.dirty = false;
                return None;
            }
        };

        self.dirty = false;

        // Slint builds the pixel buffer from the reported dimensions, so a
        // mismatch would panic the UI thread rather than show a bad frame.
        if (info.width, info.height) != (width, height) {
            eprintln!(
                "skipping a frame: read back {}x{}, expected {width}x{height}",
                info.width, info.height
            );
            return None;
        }

        Some(Frame {
            width,
            height,
            rgba: self.pixels.clone(),
        })
    }

    /// Brings up the runtime, map and render session, and loads the style.
    fn ensure_map(&mut self) -> Option<()> {
        if self.runtime.is_none() {
            let mut options = RuntimeOptions::default();
            options.cache_path = Some(self.cache.to_string_lossy().into_owned());
            match RuntimeHandle::with_options(&options) {
                Ok(runtime) => self.runtime = Some(runtime),
                Err(error) => {
                    eprintln!("creating the map runtime failed: {error}");
                    return None;
                }
            }
        }
        if self.map.is_some() {
            return Some(());
        }

        let runtime = self.runtime.as_ref()?;
        let (width, height) = self.size;
        let mut options = MapOptions::new(width, height, 1.0);
        options.mode = MapMode::Continuous;
        let map = match MapHandle::with_options(runtime, &options) {
            Ok(map) => map,
            Err(error) => {
                eprintln!("creating the map failed: {error}");
                return None;
            }
        };

        // A map queues no event of an unselected type, so this comes before the
        // style load.
        if let Err(error) = map.set_event_mask(
            RuntimeEventMask::MAP_RENDER_UPDATE_AVAILABLE
                | RuntimeEventMask::MAP_RENDER_FRAME_FINISHED
                | RuntimeEventMask::MAP_STYLE_LOADED,
        ) {
            eprintln!("selecting map events failed: {error}");
        }
        if let Err(error) = map.set_style_url(&self.style_url) {
            eprintln!("style load failed: {error}");
        }

        let session = match attach_render_target(&map, self.size) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("attaching the render target failed: {error}");
                return None;
            }
        };
        self.map = Some(Attached {
            map,
            session,
            layers: false,
            style_loaded: false,
        });
        Some(())
    }

    /// Brings the band layers up to date.
    ///
    /// The layers are created once and then their paint properties are set in
    /// place. That is the whole reason this app moved to the FFI bindings: the
    /// old ones had no property setter, so a band update meant removing and
    /// re-adding the layer, and every change to the layer set makes MapLibre
    /// Native re-run tile layout for the building source. Sixteen of those a
    /// frame starved tile loading outright — flying while a track played left
    /// the map blank until the music stopped — and cost a third of the frame
    /// rate besides. None of that applies to a property set.
    fn sync_bands(&mut self) {
        let Some(attached) = &self.map else { return };
        // A layer naming a source the style has not loaded yet is rejected, and
        // `set_style_url` only starts the load.
        if !attached.style_loaded {
            return;
        }
        if !attached.layers {
            for band in 0..BINS {
                let json = building_layer_json(band, &building_layer_id(band));
                if let Err(error) = attached
                    .map
                    .add_style_layer_json(json.to_string().as_bytes(), None)
                {
                    eprintln!("adding building layer {band} failed: {error}");
                    return;
                }
            }
            if let Some(attached) = &mut self.map {
                attached.layers = true;
            }
            self.applied = [None; BINS];
        }

        let Some(attached) = &self.map else { return };
        for band in 0..BINS {
            let target = self.bands[band];
            if self.applied[band].is_some_and(|applied| applied.close_to(target)) {
                continue;
            }
            let id = building_layer_id(band);
            let height = serde_json::json!(target.height).to_string();
            let color = serde_json::json!(band_color(target)).to_string();
            let set = attached
                .map
                .set_layer_property(&id, "fill-extrusion-height", height.as_bytes())
                .and_then(|()| {
                    attached
                        .map
                        .set_layer_property(&id, "fill-extrusion-color", color.as_bytes())
                });
            match set {
                Ok(()) => self.applied[band] = Some(target),
                Err(error) => eprintln!("updating building layer {band} failed: {error}"),
            }
        }
    }

    /// Closes the map and runtime in order, so the tile cache is flushed rather
    /// than cut off.
    fn close(&mut self) {
        if let Some(attached) = self.map.take() {
            drop(attached.session);
            if let Err(error) = attached.map.close() {
                eprintln!("closing the map failed: {error}");
            }
        }
        if let Some(runtime) = self.runtime.take()
            && let Err(error) = runtime.close()
        {
            eprintln!("closing the map runtime failed: {error}");
        }
    }
}

/// Render thread body: coalesce whatever commands are pending, turn the
/// runtime, then render at most one frame per pass.
fn render_thread(
    size: (u32, u32),
    commands: Receiver<Command>,
    frames: SyncSender<Frame>,
    rendered: &AtomicU64,
) {
    let mut engine = Engine::new(size);
    loop {
        // Only park when there is nothing to draw.
        if !engine.wants_frame() {
            match commands.recv_timeout(IDLE_TICK) {
                Ok(command) => engine.apply(command),
                Err(RecvTimeoutError::Timeout) => {}
                // The UI dropped its handle: the app is shutting down.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut disconnected = false;
        loop {
            match commands.try_recv() {
                Ok(command) => engine.apply(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            break;
        }

        // The runtime has to turn whether or not a frame is wanted: that is how
        // tile loads finish and how the map says it wants another one.
        engine.pump(Some(PUMP_BUDGET));
        if !engine.wants_frame() {
            continue;
        }
        if let Some(frame) = engine.render() {
            rendered.fetch_add(1, Ordering::Relaxed);
            // Drop the frame rather than stall if the UI has not consumed the
            // previous one yet.
            let _ = frames.try_send(frame);
        }
    }
    engine.close();
}

/// Attaches a render target the map draws into and this thread reads back.
///
/// The backend is chosen at compile time to match the `maplibre-native-ffi`
/// feature in `Cargo.toml`; the FFI hands the graphics plumbing to the caller,
/// so the device comes from here.
#[cfg(target_os = "macos")]
fn attach_render_target(
    map: &MapHandle,
    size: (u32, u32),
) -> maplibre_native_ffi::Result<RenderSessionHandle> {
    let extent = RenderTargetExtent::new(size.0, size.1, 1.0);
    let context = MetalContextDescriptor::new(metal_device());
    map.attach_ref()?
        .attach_metal_owned_texture(&MetalOwnedTextureDescriptor::new(extent, context))
}

/// The process-wide Metal device the render target allocates its texture on.
/// Leaked deliberately: it outlives every map, and the render target only
/// borrows the pointer.
#[cfg(target_os = "macos")]
fn metal_device() -> NativePointer {
    use objc2::rc::Retained;
    use objc2_metal::MTLCreateSystemDefaultDevice;

    static DEVICE: OnceLock<usize> = OnceLock::new();
    let address = *DEVICE.get_or_init(|| {
        // SAFETY: MTLCreateSystemDefaultDevice returns a retained-compatible
        // Objective-C object.
        let device = unsafe { Retained::retain(MTLCreateSystemDefaultDevice()) }
            .expect("MTLCreateSystemDefaultDevice returned nil");
        let address = Retained::as_ptr(&device) as usize;
        std::mem::forget(device);
        address
    });
    // SAFETY: the device is leaked, so the address stays valid for the process.
    unsafe { NativePointer::from_address(address) }
}

fn building_layer_id(band: usize) -> String {
    format!("3d-buildings-{band}")
}

/// Builds the style-spec JSON for one band's extrusion layer. Each band filters
/// buildings by their true height so the skyline is split into `BINS` slices,
/// exactly as the web demo did.
///
/// The paint values here are only the resting state; the animation sets them in
/// place with `set_layer_property`.
fn building_layer_json(band: usize, id: &str) -> serde_json::Value {
    let bin_width = MAX_BUILDING_HEIGHT / BINS as f64;
    let low = band as f64 * bin_width;
    let high = (band + 1) as f64 * bin_width;
    serde_json::json!({
        "id": id,
        "type": "fill-extrusion",
        "source": BUILDING_SOURCE,
        "source-layer": BUILDING_SOURCE_LAYER,
        "filter": ["all", [">", "render_height", low], ["<=", "render_height", high]],
        "paint": {
            "fill-extrusion-color": band_color(Band::default()),
            "fill-extrusion-height": Band::default().height,
            "fill-extrusion-opacity": 0.6,
        },
    })
}

/// Silent bands stay neutral grey (the web demo used a flat "#aaa");
/// saturation and lightness rise with the band level.
fn band_color(spec: Band) -> String {
    hsl_to_hex(spec.hue, spec.level * 80.0, 50.0 + spec.level * 15.0)
}

/// `h` in degrees, `s` and `l` in percent.
fn hsl_to_hex(h: f64, s: f64, l: f64) -> String {
    let s = (s / 100.0).clamp(0.0, 1.0);
    let l = (l / 100.0).clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to_byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", to_byte(r), to_byte(g), to_byte(b))
}

fn cache_path() -> PathBuf {
    std::env::temp_dir().join("osm-sound-demo-slint-tiles.sqlite")
}

fn safe_size(size: Size) -> (u32, u32) {
    let scale = render_scale();
    (
        ((size.width as f64 * scale) as u32).max(1),
        ((size.height as f64 * scale) as u32).max(1),
    )
}

/// What fraction of the map's on-screen size to render at, before Slint scales
/// the frame back up to fill it.
///
/// Everything the map costs scales with the pixels it covers, and on a large
/// display that dominates. At 1920x1200 the map ran at 9.8 fps under the band
/// animation; at 0.67 (1280x800) the same work ran at 20.4, and the tile layout
/// a rewrite triggers got cheaper too, since it follows the viewport. The frame
/// is upscaled, so the map goes soft — which is why this is off by default and
/// left to whoever knows what their display and GPU are worth.
///
/// `OSM_SOUND_DEMO_RENDER_SCALE`, clamped to something sane.
fn render_scale() -> f64 {
    static SCALE: OnceLock<f64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("OSM_SOUND_DEMO_RENDER_SCALE")
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .unwrap_or(1.0)
            .clamp(0.25, 1.0)
    })
}

/// The camera as MapLibre Native's FFI wants it.
fn camera_options(camera: MapCamera) -> CameraOptions {
    let mut options = CameraOptions::default();
    options.center = Some(LatLng::new(camera.lat, camera.lon));
    options.zoom = Some(camera.zoom);
    options.bearing = Some(camera.bearing);
    options.pitch = Some(camera.pitch);
    options
}

fn clamp_zoom(zoom: f64) -> f64 {
    zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

fn clamp_pitch(pitch: f64) -> f64 {
    pitch.clamp(MIN_PITCH, MAX_PITCH)
}

fn clamp_lat(lat: f64) -> f64 {
    lat.clamp(-MAX_ABS_LAT, MAX_ABS_LAT)
}

fn normalize_lon(lon: f64) -> f64 {
    let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped == -180.0 { 180.0 } else { wrapped }
}

/// Signed shortest-path delta between two longitudes, so a fly-to crosses the
/// antimeridian rather than going the long way round.
fn shortest_lon_delta(from: f64, to: f64) -> f64 {
    let delta = (to - from).rem_euclid(360.0);
    if delta > 180.0 { delta - 360.0 } else { delta }
}

/// Longer trips get longer flights, within bounds. `MAPLIBRE_FLY_MS` overrides
/// the result outright, matching the Raspberry Pi port's knob for slow GPUs.
fn fly_duration(travel_degrees: f64) -> Duration {
    if let Some(ms) = std::env::var("MAPLIBRE_FLY_MS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
    {
        return Duration::from_millis(ms);
    }
    let scaled = Duration::from_millis((travel_degrees * FLY_MS_PER_DEGREE) as u64);
    (FLY_MIN + scaled).min(FLY_MAX)
}

fn normalize_bearing(bearing: f64) -> f64 {
    bearing.rem_euclid(360.0)
}

fn degrees_per_pixel(zoom: f64, lat: f64) -> (f64, f64) {
    let scale = 256.0 * 2.0_f64.powf(zoom);
    let lon_per_px = 360.0 / scale;
    let lat_per_px = lon_per_px * lat.to_radians().cos().abs().max(0.1);
    (lon_per_px, lat_per_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Frames a test renders to let tiles arrive; the production settle window
    /// is a timer, which a test cannot wait on frame by frame.
    const TEST_SETTLE_FRAMES: u32 = 90;

    /// Size the timing probes render at, `OSM_SOUND_DEMO_RENDER_SIZE` as
    /// `<width>x<height>`.
    fn probe_size() -> (u32, u32) {
        let Some(value) = std::env::var_os("OSM_SOUND_DEMO_RENDER_SIZE") else {
            return (960, 640);
        };
        let value = value.to_string_lossy().into_owned();
        let (width, height) = value.split_once('x').expect("<width>x<height>");
        (
            width.trim().parse().expect("width"),
            height.trim().parse().expect("height"),
        )
    }

    fn controller_at(lat: f64, lon: f64, zoom: f64) -> CameraController {
        let mut controller = CameraController::default();
        controller.jump_for_test(lat, lon, zoom);
        controller
    }

    #[test]
    fn normalize_longitude_wraps_to_expected_range() {
        assert_eq!(normalize_lon(190.0), -170.0);
        assert_eq!(normalize_lon(-190.0), 170.0);
    }

    #[test]
    fn normalize_bearing_wraps_positively() {
        assert_eq!(normalize_bearing(450.0), 90.0);
        assert_eq!(normalize_bearing(-90.0), 270.0);
    }

    #[test]
    fn drag_with_zero_bearing_matches_screen_direction() {
        let mut controller = controller_at(0.0, 0.0, 1.0);
        controller.drag_state = Some(DragState { x: 100.0, y: 100.0 });
        assert!(controller.mouse_moved(110.0, 90.0));
        assert!(controller.camera.lon < 0.0);
        assert!(controller.camera.lat < 0.0);
    }

    #[test]
    fn drag_is_rotated_by_bearing() {
        let mut controller = controller_at(0.0, 0.0, 1.0);
        controller.camera.bearing = 90.0;
        controller.drag_state = Some(DragState { x: 100.0, y: 100.0 });
        assert!(controller.mouse_moved(110.0, 100.0));
        // Dragging right with the map rotated a quarter turn moves the centre
        // along the north/south axis instead of east/west.
        assert!(controller.camera.lat > 0.0);
        assert!(controller.camera.lon.abs() < 1e-9);
    }

    #[test]
    fn drag_without_a_press_is_ignored() {
        let mut controller = controller_at(0.0, 0.0, 1.0);
        assert!(!controller.mouse_moved(110.0, 90.0));
        assert_eq!(controller.camera, controller_at(0.0, 0.0, 1.0).camera);
    }

    #[test]
    fn stick_pan_matches_a_drag_of_the_same_delta() {
        let mut dragged = controller_at(35.0, 139.0, 12.0);
        dragged.drag_state = Some(DragState { x: 0.0, y: 0.0 });
        dragged.mouse_moved(12.0, -7.0);

        let mut panned = controller_at(35.0, 139.0, 12.0);
        panned.pan_by(12.0, -7.0);

        assert_eq!(dragged.camera, panned.camera);
    }

    #[test]
    fn stick_pan_follows_the_bearing_on_screen() {
        let mut controller = controller_at(0.0, 0.0, 4.0);
        controller.boost = CameraBoost {
            bearing: 90.0,
            ..CameraBoost::default()
        };
        controller.pan_by(10.0, 0.0);
        // A quarter turn on screen sends a sideways pan north/south instead.
        assert!(controller.camera.lat > 0.0, "{:?}", controller.camera);
        assert!(
            controller.camera.lon.abs() < 1e-9,
            "{:?}",
            controller.camera
        );
    }

    #[test]
    fn stick_pan_cancels_a_fly_to() {
        let mut controller = controller_at(0.0, 0.0, 4.0);
        controller.fly_to(35.0, 139.0, 16.0);
        controller.pan_by(5.0, 5.0);
        assert!(controller.flight.is_none());
    }

    #[test]
    fn a_boost_shifts_the_camera_on_screen_without_moving_the_base() {
        let mut controller = controller_at(35.0, 139.0, 16.0);
        controller.camera.bearing = 10.0;
        controller.boost = CameraBoost {
            zoom: -2.0,
            pitch: -30.0,
            bearing: 100.0,
        };
        let view = controller.effective();
        assert_eq!(view.zoom, 14.0);
        assert_eq!(view.pitch, 30.0);
        assert_eq!(view.bearing, 110.0);
        // The user's own camera is untouched, so the effect decays cleanly.
        assert_eq!(controller.camera.zoom, 16.0);
        assert_eq!(controller.camera.bearing, 10.0);

        // Boosts are clamped to what the map can actually show.
        controller.boost = CameraBoost {
            zoom: 50.0,
            pitch: 50.0,
            bearing: 0.0,
        };
        let view = controller.effective();
        assert_eq!(view.zoom, MAX_ZOOM);
        assert_eq!(view.pitch, MAX_PITCH);
    }

    #[test]
    fn wheel_zoom_is_clamped() {
        let mut controller = controller_at(0.0, 0.0, MAX_ZOOM);
        controller.wheel_zoomed(-120.0);
        assert_eq!(controller.camera.zoom, MAX_ZOOM);

        let mut controller = controller_at(0.0, 0.0, MIN_ZOOM);
        controller.wheel_zoomed(120.0);
        assert_eq!(controller.camera.zoom, MIN_ZOOM);
    }

    #[test]
    fn hsl_conversion_matches_known_colors() {
        assert_eq!(hsl_to_hex(0.0, 100.0, 50.0), "#ff0000");
        assert_eq!(hsl_to_hex(120.0, 100.0, 50.0), "#00ff00");
        assert_eq!(hsl_to_hex(240.0, 100.0, 50.0), "#0000ff");
        assert_eq!(hsl_to_hex(0.0, 0.0, 100.0), "#ffffff");
    }

    #[test]
    fn silent_bands_render_grey() {
        assert_eq!(band_color(Band::default()), "#808080");
    }

    #[test]
    fn building_layer_bins_cover_the_height_range() {
        let first = building_layer_json(0, "a");
        let last = building_layer_json(BINS - 1, "b");
        assert_eq!(first["filter"][1][2], serde_json::json!(0.0));
        assert_eq!(last["filter"][2][2], serde_json::json!(MAX_BUILDING_HEIGHT));
    }

    #[test]
    fn the_building_layers_use_constant_paint() {
        let json = building_layer_json(3, "a");
        let paint = &json["paint"];
        // Data-driven paint (a `step` / `get` expression) would be re-evaluated
        // per building every frame and costs about twenty times as much.
        assert!(paint["fill-extrusion-height"].is_number(), "{paint}");
        assert!(paint["fill-extrusion-color"].is_string(), "{paint}");
    }

    #[test]
    fn nearby_bands_do_not_trigger_a_layer_rebuild() {
        let base = Band {
            height: 100.0,
            hue: 10.0,
            level: 0.5,
        };
        assert!(base.close_to(Band {
            height: 101.0,
            ..base
        }));
        assert!(!base.close_to(Band {
            height: 110.0,
            ..base
        }));
        assert!(!base.close_to(Band { hue: 40.0, ..base }));
    }

    #[test]
    fn shortest_longitude_delta_crosses_the_antimeridian() {
        assert_eq!(shortest_lon_delta(170.0, -170.0), 20.0);
        assert_eq!(shortest_lon_delta(-170.0, 170.0), -20.0);
        assert_eq!(shortest_lon_delta(0.0, 90.0), 90.0);
    }

    #[test]
    fn a_fly_to_eases_to_its_destination() {
        let mut controller = CameraController::default();
        controller.jump_for_test(35.68, 139.76, 16.0);
        controller.fly_to(34.70, 135.49, 16.0);

        let flight = controller.flight.as_ref().expect("a flight started");
        let duration = flight.duration;
        assert!(duration >= FLY_MIN && duration <= FLY_MAX);

        // Halfway there the camera is between the two, and pulled back.
        assert!(controller.advance_flight(duration / 2));
        let midpoint = controller.camera;
        assert!(
            midpoint.lon < 139.76 && midpoint.lon > 135.49,
            "{midpoint:?}"
        );
        assert!(
            midpoint.zoom < 16.0,
            "midpoint should zoom out: {midpoint:?}"
        );

        // Overshooting the duration lands exactly on the destination.
        assert!(controller.advance_flight(duration));
        assert!(controller.flight.is_none());
        assert_eq!(controller.camera.lat, 34.70);
        assert_eq!(controller.camera.lon, 135.49);
        assert_eq!(controller.camera.zoom, 16.0);
        assert!(!controller.advance_flight(duration));
    }

    #[test]
    fn a_fly_to_takes_the_short_way_around_the_antimeridian() {
        let mut controller = CameraController::default();
        controller.jump_for_test(0.0, 175.0, 4.0);
        controller.fly_to(0.0, -175.0, 4.0);
        controller.advance_flight(Duration::from_millis(1));
        // Going the long way would put the camera near 0°, not past 180°.
        assert!(controller.camera.lon > 175.0, "{:?}", controller.camera);
    }

    #[test]
    fn dragging_cancels_a_fly_to() {
        let mut controller = CameraController::default();
        controller.jump_for_test(0.0, 0.0, 4.0);
        controller.fly_to(35.68, 139.76, 16.0);
        controller.drag_state = Some(DragState { x: 10.0, y: 10.0 });
        assert!(controller.mouse_moved(20.0, 20.0));
        assert!(controller.flight.is_none());
    }

    /// Opt-in: drives the real renderer to confirm that swapping a band's
    /// extrusion layer actually changes the rendered image. Needs the network
    /// for tiles, and a graphics device.
    #[test]
    fn band_heights_change_the_rendered_image() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            eprintln!("skipped: set OSM_SOUND_DEMO_RENDERER_TESTS=1 to run");
            return;
        }

        let mut engine = Engine::new((480, 360));
        // Continuous mode draws whatever has loaded so far, so let the tiles
        // arrive before comparing anything.
        let settle = |engine: &mut Engine| {
            let mut last = None;
            for _ in 0..TEST_SETTLE_FRAMES {
                engine.mark_dirty();
                last = engine.render();
            }
            last.expect("a frame renders")
        };

        let flat = settle(&mut engine);
        assert!(
            flat.rgba.iter().any(|byte| *byte != 0),
            "the renderer never produced an image"
        );

        engine.apply(Command::Bands(Box::new(
            [Band {
                height: 220.0,
                hue: 200.0,
                level: 1.0,
            }; BINS],
        )));
        let tall = settle(&mut engine);

        assert_eq!((flat.width, flat.height), (tall.width, tall.height));
        let changed = flat
            .rgba
            .iter()
            .zip(tall.rgba.iter())
            .filter(|(a, b)| a != b)
            .count();
        let ratio = changed as f64 / flat.rgba.len() as f64;
        eprintln!("{:.1}% of the subpixels changed", ratio * 100.0);
        assert!(
            ratio > 0.05,
            "raising every band barely changed the image ({ratio})"
        );
    }

    /// Opt-in timing probe for the render path the app actually uses.
    /// Same probe as the WGPU branch carries, so the two backends can be
    /// compared on identical work. Wall clock, not per-call timing: a layer
    /// swap returns as soon as the work is queued.
    #[test]
    fn report_playing_frame_rate() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            return;
        }
        let size = probe_size();
        let mut engine = Engine::new(size);
        for _ in 0..TEST_SETTLE_FRAMES {
            engine.mark_dirty();
            engine.render().expect("warm-up frame");
        }

        const BUDGET: Duration = Duration::from_secs(5);

        fn rate(engine: &mut Engine, mut step: impl FnMut(&mut Engine, u32)) -> f64 {
            let started = Instant::now();
            let mut frames = 0u32;
            while started.elapsed() < BUDGET {
                step(engine, frames);
                engine.mark_dirty();
                if engine.render().is_some() {
                    frames += 1;
                }
            }
            f64::from(frames) / started.elapsed().as_secs_f64()
        }

        let still = rate(&mut engine, |_, _| {});
        let camera = rate(&mut engine, |engine, frame| {
            engine.apply(Command::Camera(MapCamera {
                bearing: f64::from(frame) * 2.0,
                ..MapCamera::default()
            }));
        });
        let playing = rate(&mut engine, |engine, frame| {
            engine.apply(Command::Camera(MapCamera {
                bearing: f64::from(frame) * 2.0,
                ..MapCamera::default()
            }));
            let level = (f64::from(frame) / 10.0).sin().abs();
            engine.apply(Command::Bands(Box::new(
                [Band {
                    height: 20.0 + level * 180.0,
                    hue: f64::from(frame) * 3.0 % 360.0,
                    level,
                }; BINS],
            )));
        });

        eprintln!(
            "{}x{} — still: {still:.1} fps, camera only: {camera:.1} fps, camera + {} bands: {playing:.1} fps",
            size.0, size.1, BINS,
        );
    }

    #[test]
    fn report_frame_costs() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            return;
        }
        let size = probe_size();
        let mut engine = Engine::new(size);
        // Warm up so every band has a layer and the tiles are cached.
        for _ in 0..TEST_SETTLE_FRAMES {
            engine.mark_dirty();
            engine.render().expect("warm-up frame");
        }

        let time = |engine: &mut Engine| {
            let started = std::time::Instant::now();
            engine.render().expect("frame");
            started.elapsed()
        };

        let mut camera_only = std::time::Duration::ZERO;
        let mut one_band = std::time::Duration::ZERO;
        let mut all_bands = std::time::Duration::ZERO;
        const ROUNDS: u32 = 5;
        for step in 1..=ROUNDS {
            let nudge = f64::from(step);
            engine.apply(Command::Camera(MapCamera {
                bearing: nudge * 2.0,
                ..MapCamera::default()
            }));
            camera_only += time(&mut engine);

            let mut bands = engine.bands;
            bands[0].height += 40.0;
            engine.apply(Command::Bands(Box::new(bands)));
            one_band += time(&mut engine);

            let bands = [Band {
                height: 20.0 * nudge,
                hue: 30.0 * nudge,
                level: 0.5,
            }; BINS];
            engine.apply(Command::Bands(Box::new(bands)));
            all_bands += time(&mut engine);
        }
        eprintln!(
            "{}x{} per frame — camera only: {:?}, one band moved: {:?}, all {} moved: {:?}",
            size.0,
            size.1,
            camera_only / ROUNDS,
            one_band / ROUNDS,
            BINS,
            all_bands / ROUNDS,
        );
    }
}
