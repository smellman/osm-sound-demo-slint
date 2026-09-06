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
    AnimationOptions, BoundOptions, CameraOptions, LatLng, MapHandle, MapMode, MapOptions,
    NativePointer, RenderSessionHandle, RenderTargetExtent, RuntimeEventMask, RuntimeEventPayload,
    RuntimeEventSource, RuntimeEventType, RuntimeHandle, RuntimeOptions,
};
#[cfg(feature = "metal")]
use maplibre_native_ffi::{MetalContextDescriptor, MetalOwnedTextureDescriptor};
#[cfg(feature = "opengl")]
use maplibre_native_ffi::{
    EglContextDescriptor, OpenGLContextDescriptor, OpenGLOwnedTextureDescriptor,
};
#[cfg(feature = "vulkan")]
use maplibre_native_ffi::{VulkanContextDescriptor, VulkanOwnedTextureDescriptor};

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

/// The buildings' colour. Flat, as the web demo's was: the style's light is
/// what tints the scene with the music.
const BUILDING_COLOR: &str = "#aaa";

/// The pitch the map opens at, matching the web demo.
const DEFAULT_PITCH: f64 = 70.0;

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
/// The web demo allowed up to 85°. MapLibre Native clamps at 60 unless the
/// bound is raised first, which `Engine::ensure_map` does — asking for more
/// without that silently gives 60 back.
const MAX_PITCH: f64 = 85.0;
const MAX_ABS_LAT: f64 = 85.0;
const WHEEL_STEP: f64 = 0.5;
const DOUBLE_CLICK_STEP: f64 = 1.0;

/// Fly-to duration. MapLibre Native picks one from the distance when none is
/// given, which is what the web demo's `flyTo` did; `MAPLIBRE_FLY_MS` overrides
/// it, as the Raspberry Pi port's knob of the same name does for slow GPUs.
fn fly_duration_ms() -> Option<f64> {
    std::env::var("MAPLIBRE_FLY_MS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

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
            pitch: DEFAULT_PITCH,
        }
    }
}

/// One frequency band's contribution to the skyline. Only the height: the
/// colour comes from the style's light, which follows the music too.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Band {
    /// Extrusion height in metres.
    pub height: f64,
}

impl Band {
    fn close_to(self, other: Self) -> bool {
        (self.height - other.height).abs() < HEIGHT_EPSILON
    }
}

/// A rendered map image, handed from the render thread to the UI thread.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The map's own camera, reported while it is flying so the UI can follow.
    pub camera: Option<MapCamera>,
    /// Whether a fly-to is still in the air.
    pub flying: bool,
    /// The most recent fly-to the render thread has taken on, whether or not it
    /// is still in the air. A frame drawn before the command arrived carries
    /// the one before it, which is how the UI tells the two apart.
    pub flight: Option<u64>,
}

enum Command {
    Resize(u32, u32),
    Style(String),
    Camera(MapCamera),
    /// Hand the camera to MapLibre Native and let it fly there itself.
    FlyTo {
        camera: MapCamera,
        duration_ms: Option<f64>,
        /// Matched against the transition-finished event, so a stale one from
        /// a cancelled flight is ignored.
        id: u64,
    },
    Bands(Box<[Band; BINS]>),
    Light(Light),
}

/// The style's light, which the web demo animated with `map.setLight`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Light {
    /// Hue in degrees.
    pub hue: f64,
    /// Saturation in percent.
    pub saturation: f64,
    /// 0.0..=1.0.
    pub intensity: f64,
}

impl Light {
    /// Whether a change is worth sending to the style.
    fn close_to(self, other: Self) -> bool {
        (self.hue - other.hue).abs() < HUE_EPSILON
            && (self.saturation - other.saturation).abs() < 2.0
            && (self.intensity - other.intensity).abs() < 0.02
    }
}

#[derive(Debug)]
struct DragState {
    x: f32,
    y: f32,
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

    #[cfg(test)]
    fn jump_for_test(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.camera.lat = clamp_lat(lat);
        self.camera.lon = normalize_lon(lon);
        self.camera.zoom = clamp_zoom(zoom);
    }

    fn mouse_moved(&mut self, x: f32, y: f32) -> bool {
        let Some(last) = self.drag_state.as_mut() else {
            return false;
        };
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
        let direction = if delta > 0.0 { -1.0 } else { 1.0 };
        self.camera.zoom = clamp_zoom(self.camera.zoom + direction * WHEEL_STEP);
        true
    }

    fn double_clicked(&mut self, shift: bool) {
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
    /// Whether a fly-to is in the air, as last reported by the render thread.
    flying: bool,
    /// Identifies each fly-to so a finished one can be matched to its start.
    flight_id: u64,
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
            flying: false,
            flight_id: 0,
        }
    }

    fn send(&self, command: Command) {
        // The render thread only goes away when the app is shutting down.
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }

    fn push_camera(&mut self) {
        // An absolute camera cancels a flight, on both sides of the channel.
        self.flying = false;
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
        if let Some(frame) = &newest {
            // While the map is flying it owns the camera; follow it so the
            // status line and the next drag start from where it actually is.
            if let Some(camera) = frame.camera {
                self.controller.camera = camera;
            }
            if frame_ends_flight(frame.flight, frame.flying, self.flight_id) {
                self.flying = false;
            }
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

    /// Hands the camera to MapLibre Native and lets it fly there.
    ///
    /// The map owns the camera until it lands: each frame reports where it got
    /// to, and [`MapLibre::take_frame`] follows along. Anything that moves the
    /// camera from here — a drag, the sticks, an effect — cancels the flight,
    /// because it sends an absolute camera the map has to obey.
    pub fn fly_to(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.controller.drag_state = None;
        self.flight_id += 1;
        self.flying = true;
        self.send(Command::FlyTo {
            camera: MapCamera {
                lat: clamp_lat(lat),
                lon: normalize_lon(lon),
                zoom: clamp_zoom(zoom),
                ..self.controller.camera
            },
            duration_ms: fly_duration_ms(),
            id: self.flight_id,
        });
    }

    pub fn flying(&self) -> bool {
        self.flying
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
        self.flying = false;
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
    /// `height_gain` scales the whole skyline, which the drop effect uses to
    /// make it jump.
    pub fn apply_levels(&mut self, levels: &[f32; BINS], height_gain: f64) {
        let mut bands = [Band::default(); BINS];
        for (band, (slot, level)) in bands.iter_mut().zip(levels.iter()).enumerate() {
            let level = f64::from(*level);
            slot.height = (10.0 + 4.0 * band as f64 + level * 255.0) * height_gain;
        }
        self.send(Command::Bands(Box::new(bands)));
    }

    /// Sets the style's light, as the web demo's `map.setLight` did.
    pub fn set_light(&mut self, light: Light) {
        self.send(Command::Light(light));
    }

    /// Resets every band back to the flat, unlit state used when nothing plays.
    pub fn reset_levels(&mut self) {
        self.send(Command::Bands(Box::new([Band::default(); BINS])));
        self.send(Command::Light(Light::default()));
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
    light: Light,
    applied_light: Option<Light>,
    /// The transition the map is running, if any.
    flight_id: Option<u64>,
    /// The last fly-to taken on, kept after `flight_id` is cleared so a frame
    /// can still say which flight it belongs to.
    last_flight: Option<u64>,
    /// A fly-to that arrived before the map existed, or before this pass.
    pending_fly: Option<(MapCamera, Option<f64>, u64)>,
    /// Report the camera with the next frame even though nothing is flying,
    /// so the UI picks up where a flight ended.
    report_camera: bool,
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
            light: Light::default(),
            applied_light: None,
            flight_id: None,
            last_flight: None,
            pending_fly: None,
            report_camera: false,
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
                    // Resize the session rather than rebuilding the map: that
                    // keeps the renderer, the tile pyramid and — the reason
                    // this matters — the loaded style. Rebuilding re-fetched
                    // the style from its URL, so resizing the window with no
                    // network left the map blank.
                    if let Some(attached) = &self.map
                        && let Err(error) = attached.session.resize(width, height, 1.0)
                    {
                        eprintln!("resizing the render target failed: {error}");
                        // Fall back to a rebuild, which at least recovers when
                        // the network is there.
                        self.map = None;
                        self.applied = [None; BINS];
                        self.applied_light = None;
                    }
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
                        self.applied_light = None;
                    }
                    self.applied = [None; BINS];
                    self.mark_dirty();
                }
            }
            Command::Camera(camera) => {
                // An absolute camera is the UI taking the wheel back, so a
                // flight in the air gives way rather than fighting it.
                if self.flight_id.take().is_some()
                    && let Some(attached) = &self.map
                    && let Err(error) = attached.map.cancel_transitions()
                {
                    eprintln!("cancelling the fly-to failed: {error}");
                }
                if self.camera != camera {
                    self.camera = camera;
                    self.mark_dirty();
                }
            }
            Command::FlyTo {
                camera,
                duration_ms,
                id,
            } => {
                self.camera = camera;
                self.flight_id = Some(id);
                self.last_flight = Some(id);
                self.pending_fly = Some((camera, duration_ms, id));
                self.mark_dirty();
            }
            Command::Bands(bands) => {
                if self.bands != *bands {
                    self.bands = *bands;
                    self.mark_dirty();
                }
            }
            Command::Light(light) => {
                if self.light != light {
                    self.light = light;
                    self.mark_dirty();
                }
            }
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Asks for the map's camera to ride along with the next frame, for when
    /// the frame that was carrying it never reached the UI.
    fn request_camera_report(&mut self) {
        self.report_camera = true;
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
        let mut landed = false;
        for event in batch.iter() {
            if event.source() != source {
                continue;
            }
            match event.event_type() {
                RuntimeEventType::MapRenderUpdateAvailable => wants_repaint = true,
                RuntimeEventType::MapStyleLoaded => style_loaded = true,
                RuntimeEventType::MapCameraTransitionFinished => {
                    if let RuntimeEventPayload::CameraTransitionFinished(finished) = event.payload()
                        && self.flight_id == Some(finished.transition_id)
                    {
                        landed = true;
                    }
                }
                RuntimeEventType::MapRenderFrameFinished => {
                    if let RuntimeEventPayload::RenderFrame(frame) = event.payload() {
                        wants_repaint |= frame.needs_repaint;
                    }
                }
                _ => {}
            }
        }
        self.wants_repaint = wants_repaint;
        if landed {
            self.flight_id = None;
            // Take the camera the flight ended on before `jump_to` resumes:
            // otherwise the next frame pins the map back to the last position
            // sampled mid-flight.
            if let Some(attached) = &self.map
                && let Ok(camera) = attached.map.camera()
            {
                self.camera = self.camera.with(&camera);
            }
            self.report_camera = true;
            // One more frame, so the UI sees `flying: false` and stops waiting.
            self.dirty = true;
        }
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
        self.sync_light();

        if let Some((camera, duration_ms, id)) = self.pending_fly.take() {
            let mut animation = AnimationOptions::default();
            animation.duration_ms = duration_ms;
            animation.transition_id = Some(id);
            let attached = self.map.as_ref()?;
            if let Err(error) = attached
                .map
                .fly_to(&camera_options(camera), Some(&animation))
            {
                eprintln!("starting the fly-to failed: {error}");
                self.flight_id = None;
            }
        } else if self.flight_id.is_none() {
            // The map drives its own camera while a flight is in the air.
            let camera = camera_options(self.camera);
            let attached = self.map.as_ref()?;
            if let Err(error) = attached.map.jump_to(&camera) {
                eprintln!("moving the camera failed: {error}");
            }
        }
        let attached = self.map.as_ref()?;
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

        // While the map owns the camera, hand it back with the frame so the UI
        // can follow. `flying` also tells the UI when it has landed.
        //
        // The landing frame needs `report_camera` to carry it: `flight_id` is
        // cleared when the transition-finished event arrives, which is in the
        // pump before this render, so `flying` is already false here. Without
        // it the UI's last sample stays the mid-flight one — short of the
        // destination, and visibly so in the zoom, which is what the map is
        // still easing when the last airborne frame is drawn.
        let flying = self.flight_id.is_some();
        let report = flying || std::mem::take(&mut self.report_camera);
        let camera = report
            .then(|| self.map.as_ref().and_then(|a| a.map.camera().ok()))
            .flatten()
            .map(|camera| MapCamera {
                lat: camera
                    .center
                    .map_or(self.camera.lat, |center| center.latitude),
                lon: camera
                    .center
                    .map_or(self.camera.lon, |center| center.longitude),
                zoom: camera.zoom.unwrap_or(self.camera.zoom),
                bearing: camera.bearing.unwrap_or(self.camera.bearing),
                pitch: camera.pitch.unwrap_or(self.camera.pitch),
            });
        if let Some(camera) = camera {
            self.camera = camera;
        }

        Some(Frame {
            width,
            height,
            rgba: self.pixels.clone(),
            camera,
            flying,
            flight: self.last_flight,
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
                | RuntimeEventMask::MAP_STYLE_LOADED
                | RuntimeEventMask::MAP_CAMERA_TRANSITION_FINISHED,
        ) {
            eprintln!("selecting map events failed: {error}");
        }
        // Raise the pitch bound before any camera goes in: MapLibre Native
        // clamps at 60 by default and would silently hold anything steeper.
        let mut bounds = BoundOptions::default();
        bounds.max_pitch = Some(MAX_PITCH);
        if let Err(error) = map.set_bounds(&bounds) {
            eprintln!("raising the pitch bound failed: {error}");
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
            match attached
                .map
                .set_layer_property(&id, "fill-extrusion-height", height.as_bytes())
            {
                Ok(()) => self.applied[band] = Some(target),
                Err(error) => eprintln!("updating building layer {band} failed: {error}"),
            }
        }
    }

    /// Sets the style's light when it has moved on.
    ///
    /// This is the web demo's `map.setLight({ color, intensity })`, which the
    /// old bindings could not express — the building hue stood in for it. Two
    /// property sets, so it costs no more than a band update.
    fn sync_light(&mut self) {
        let Some(attached) = &self.map else { return };
        if !attached.style_loaded {
            return;
        }
        if self
            .applied_light
            .is_some_and(|applied| applied.close_to(self.light))
        {
            return;
        }

        let color =
            serde_json::json!(hsl_to_hex(self.light.hue, self.light.saturation, 50.0)).to_string();
        let intensity = serde_json::json!(self.light.intensity).to_string();
        let set = attached
            .map
            .set_style_light_property("color", color.as_bytes())
            .and_then(|()| {
                attached
                    .map
                    .set_style_light_property("intensity", intensity.as_bytes())
            });
        match set {
            Ok(()) => self.applied_light = Some(self.light),
            Err(error) => {
                eprintln!("setting the style light failed: {error}");
                // Stop retrying every frame on a style with no light.
                self.applied_light = Some(self.light);
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

/// MapLibre Native allows one runtime per owning thread, so an engine that goes
/// out of scope without giving its own up leaves that thread unable to build
/// another. The app only ever has one, but the tests share a thread.
impl Drop for Engine {
    fn drop(&mut self) {
        self.close();
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
            let carried_camera = frame.camera.is_some();
            if frames.try_send(frame).is_err() && carried_camera {
                // A dropped frame is no loss on its own, but the camera it was
                // carrying is: nothing else will tell the UI where the map
                // stopped.
                engine.request_camera_report();
            }
        }
    }
    // `Drop` closes it; naming it here as well would only be a second way in.
}

/// Attaches a render target the map draws into and this thread reads back.
///
/// The backend is chosen at compile time to match the `maplibre-native-ffi`
/// feature in `Cargo.toml`; the FFI hands the graphics plumbing to the caller,
/// so the device comes from here.
#[cfg(feature = "metal")]
fn attach_render_target(
    map: &MapHandle,
    size: (u32, u32),
) -> maplibre_native_ffi::Result<RenderSessionHandle> {
    let extent = RenderTargetExtent::new(size.0, size.1, 1.0);
    let context = MetalContextDescriptor::new(metal_device());
    map.attach_ref()?
        .attach_metal_owned_texture(&MetalOwnedTextureDescriptor::new(extent, context))
}

/// Attaches a Vulkan owned-texture render target.
///
/// Headless: nothing here presents to a surface, so no swapchain and no
/// surface extensions. MapLibre Native draws into a texture the session owns
/// and this thread reads it back.
#[cfg(feature = "vulkan")]
fn attach_render_target(
    map: &MapHandle,
    size: (u32, u32),
) -> maplibre_native_ffi::Result<RenderSessionHandle> {
    let vulkan = vulkan_context();
    let extent = RenderTargetExtent::new(size.0, size.1, 1.0);
    // SAFETY: every address below names an object leaked in
    // `create_vulkan_context`, so all of them outlive this session.
    let pointer = |address: usize| unsafe { NativePointer::from_address(address) };
    let mut context = VulkanContextDescriptor::new(
        pointer(vulkan.instance),
        pointer(vulkan.physical_device),
        pointer(vulkan.device),
        pointer(vulkan.graphics_queue),
        vulkan.graphics_queue_family_index,
    );
    // Let MapLibre Native resolve its entry points through the same loader
    // rather than assuming a system one.
    context.get_instance_proc_addr = pointer(vulkan.get_instance_proc_addr);
    context.get_device_proc_addr = pointer(vulkan.get_device_proc_addr);
    map.attach_ref()?
        .attach_vulkan_owned_texture(&VulkanOwnedTextureDescriptor::new(extent, context))
}

/// Attaches an OpenGL owned-texture render target, drawn through EGL.
///
/// Shared ownership: this thread owns an EGL context and keeps it current, and
/// the session creates its own in the same share group. That is the
/// configuration `maplibre-native-ffi` exercises for owned textures on Linux.
/// `get_proc_address` is left null, as it is there — the session resolves its
/// entry points through the display it is handed.
#[cfg(feature = "opengl")]
fn attach_render_target(
    map: &MapHandle,
    size: (u32, u32),
) -> maplibre_native_ffi::Result<RenderSessionHandle> {
    let egl = egl_context();
    let extent = RenderTargetExtent::new(size.0, size.1, 1.0);
    // SAFETY: every address below names an object leaked in
    // `create_egl_context`, so all of them outlive this session.
    let pointer = |address: usize| unsafe { NativePointer::from_address(address) };
    let context = EglContextDescriptor::new(
        pointer(egl.display),
        pointer(egl.config),
        pointer(egl.context),
    );
    map.attach_ref()?.attach_opengl_owned_texture(&OpenGLOwnedTextureDescriptor::new(
        extent,
        OpenGLContextDescriptor::Egl(context),
    ))
}

/// The EGL handles a render target borrows, as plain addresses for the same
/// reason [`VulkanContext`] holds them that way.
#[cfg(feature = "opengl")]
#[derive(Clone, Copy)]
struct EglContext {
    display: usize,
    config: usize,
    context: usize,
}

/// This thread's EGL context, created on first use and kept current.
///
/// Per thread, not per process as the Vulkan device is: an EGL context is
/// current on one thread at a time, and the session inherits the one current
/// where it renders. The render thread is the only one that reaches this in the
/// app; the renderer tests each get their own.
#[cfg(feature = "opengl")]
fn egl_context() -> EglContext {
    thread_local! {
        static CONTEXT: std::cell::OnceCell<EglContext> = const { std::cell::OnceCell::new() };
    }
    CONTEXT.with(|cell| *cell.get_or_init(|| create_egl_context().expect("creating an EGL context")))
}

#[cfg(feature = "opengl")]
fn create_egl_context() -> Result<EglContext, Box<dyn std::error::Error>> {
    use glutin_egl_sys::egl;
    use glutin_egl_sys::egl::types::EGLint;
    use libloading::Library;
    use std::ffi::{CString, c_void};

    // Bindings only: the library itself has to be opened, and the two are kept
    // alive by leaking them at the end.
    let lib = unsafe { Library::new("libEGL.so.1") }
        .or_else(|_| unsafe { Library::new("libEGL.so") })
        .map_err(|error| format!("failed to load libEGL: {error}"))?;
    type EglGetProcAddress = unsafe extern "system" fn(*const c_void) -> *const c_void;
    let get_proc_address: libloading::Symbol<'_, EglGetProcAddress> =
        unsafe { lib.get(b"eglGetProcAddress\0")? };
    // SAFETY: every symbol is resolved from the library just opened, or from
    // its own `eglGetProcAddress`.
    let egl = unsafe {
        egl::Egl::load_with(|symbol| {
            let name = CString::new(symbol).expect("EGL symbol names do not contain NULs");
            if let Ok(loaded) = lib.get::<*const c_void>(name.as_bytes_with_nul()) {
                *loaded
            } else {
                get_proc_address(name.as_ptr().cast())
            }
        })
    };

    // Nothing here presents to a window, so the map draws headless on Mesa's
    // surfaceless platform rather than opening a display server connection.
    const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
    if !egl.GetPlatformDisplayEXT.is_loaded() {
        return Err("eglGetPlatformDisplayEXT is unavailable".into());
    }
    // SAFETY: the entry point is loaded, and the attribute list is terminated.
    let display = unsafe {
        egl.GetPlatformDisplayEXT(
            EGL_PLATFORM_SURFACELESS_MESA,
            egl::DEFAULT_DISPLAY as *mut c_void,
            [egl::NONE as EGLint].as_ptr(),
        )
    };
    if display == egl::NO_DISPLAY {
        // SAFETY: EGL is loaded; GetError needs no live display.
        return Err(format!("eglGetPlatformDisplayEXT failed with 0x{:x}", unsafe {
            egl.GetError()
        })
        .into());
    }
    // SAFETY: display was just obtained.
    if unsafe { egl.Initialize(display, std::ptr::null_mut(), std::ptr::null_mut()) }
        == egl::FALSE
    {
        // SAFETY: EGL is loaded.
        return Err(format!("eglInitialize failed with 0x{:x}", unsafe { egl.GetError() }).into());
    }

    let config_attributes = [
        egl::SURFACE_TYPE as EGLint,
        egl::PBUFFER_BIT as EGLint,
        egl::RENDERABLE_TYPE as EGLint,
        egl::OPENGL_ES3_BIT as EGLint,
        egl::RED_SIZE as EGLint,
        8,
        egl::GREEN_SIZE as EGLint,
        8,
        egl::BLUE_SIZE as EGLint,
        8,
        egl::ALPHA_SIZE as EGLint,
        8,
        egl::DEPTH_SIZE as EGLint,
        24,
        egl::STENCIL_SIZE as EGLint,
        8,
        egl::NONE as EGLint,
    ];
    let mut config: egl::types::EGLConfig = std::ptr::null_mut();
    let mut config_count = 0;
    // SAFETY: display is initialised and the attribute list is terminated.
    if unsafe {
        egl.ChooseConfig(
            display,
            config_attributes.as_ptr(),
            &mut config,
            1,
            &mut config_count,
        )
    } == egl::FALSE
        || config_count == 0
    {
        // SAFETY: EGL is loaded.
        return Err(format!("eglChooseConfig found nothing: 0x{:x}", unsafe {
            egl.GetError()
        })
        .into());
    }

    // SAFETY: EGL is initialised on this display.
    if unsafe { egl.BindAPI(egl::OPENGL_ES_API) } == egl::FALSE {
        // SAFETY: EGL is loaded.
        return Err(format!("eglBindAPI failed with 0x{:x}", unsafe { egl.GetError() }).into());
    }

    let context_attributes = [
        egl::CONTEXT_CLIENT_VERSION as EGLint,
        3,
        egl::NONE as EGLint,
    ];
    // SAFETY: display and config are live, and the attribute list is terminated.
    let context = unsafe {
        egl.CreateContext(
            display,
            config,
            egl::NO_CONTEXT,
            context_attributes.as_ptr(),
        )
    };
    if context == egl::NO_CONTEXT {
        // SAFETY: EGL is loaded.
        return Err(format!("eglCreateContext failed with 0x{:x}", unsafe {
            egl.GetError()
        })
        .into());
    }

    // A pbuffer only so the context has something to be current against; the
    // map renders into the texture the session owns, never into this.
    let surface_attributes = [
        egl::WIDTH as EGLint,
        1,
        egl::HEIGHT as EGLint,
        1,
        egl::NONE as EGLint,
    ];
    // SAFETY: display and config are live, and the attribute list is terminated.
    let surface =
        unsafe { egl.CreatePbufferSurface(display, config, surface_attributes.as_ptr()) };
    if surface == egl::NO_SURFACE {
        // SAFETY: EGL is loaded.
        return Err(format!("eglCreatePbufferSurface failed with 0x{:x}", unsafe {
            egl.GetError()
        })
        .into());
    }
    // Shared ownership means the session joins whatever is current here, so
    // this has to be current before any session attaches.
    // SAFETY: every handle was created on this display.
    if unsafe { egl.MakeCurrent(display, surface, surface, context) } == egl::FALSE {
        // SAFETY: EGL is loaded.
        return Err(format!("eglMakeCurrent failed with 0x{:x}", unsafe { egl.GetError() }).into());
    }

    let handles = EglContext {
        display: display as usize,
        config: config as usize,
        context: context as usize,
    };

    // Deliberate, as on the Vulkan path: the render target borrows these for as
    // long as the thread runs, and tearing them down under a live session would
    // be a use-after-free.
    std::mem::forget(egl);
    std::mem::forget(lib);
    Ok(handles)
}

/// The handles a Vulkan render target borrows.
///
/// Held as plain addresses because `NativePointer` is deliberately `!Send`, and
/// this lives in a static. The instance and device behind them are leaked, so
/// they stay valid for the process — the render target borrows them and a map
/// may outlive any one session.
#[cfg(feature = "vulkan")]
struct VulkanContext {
    instance: usize,
    physical_device: usize,
    device: usize,
    graphics_queue: usize,
    graphics_queue_family_index: u32,
    get_instance_proc_addr: usize,
    get_device_proc_addr: usize,
}

/// The process-wide Vulkan device the render target allocates its texture on.
#[cfg(feature = "vulkan")]
fn vulkan_context() -> &'static VulkanContext {
    static CONTEXT: OnceLock<VulkanContext> = OnceLock::new();
    CONTEXT.get_or_init(|| create_vulkan_context().expect("creating a Vulkan device"))
}

#[cfg(feature = "vulkan")]
fn create_vulkan_context() -> Result<VulkanContext, Box<dyn std::error::Error>> {
    use ash::vk;
    use ash::vk::Handle;
    use std::ffi::CString;

    // SAFETY: loads the system Vulkan loader; the entry is leaked below, so
    // every function pointer taken from it outlives every use.
    let entry = unsafe { ash::Entry::load()? };

    let name = CString::new("osm-sound-demo-slint")?;
    let app_info = vk::ApplicationInfo::default()
        .application_name(&name)
        .engine_name(&name)
        .api_version(vk::API_VERSION_1_0);
    let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
    // SAFETY: instance_info borrows app_info, which outlives this call.
    let instance = unsafe { entry.create_instance(&instance_info, None)? };

    // SAFETY: instance was created above and is live.
    let physical_devices = unsafe { instance.enumerate_physical_devices()? };
    let picked = physical_devices.into_iter().find_map(|physical_device| {
        // SAFETY: physical_device came from this instance.
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        families
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .map(|index| (physical_device, index as u32))
    });
    let Some((physical_device, graphics_queue_family_index)) = picked else {
        // SAFETY: instance is live and has no child objects yet.
        unsafe { instance.destroy_instance(None) };
        return Err("no Vulkan device with a graphics queue".into());
    };

    let priorities = [1.0_f32];
    let queue_info = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(graphics_queue_family_index)
        .queue_priorities(&priorities)];
    let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_info);
    // SAFETY: physical_device and the queue family were taken from this instance.
    let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
        Ok(device) => device,
        Err(error) => {
            // SAFETY: instance is live and has no child objects yet.
            unsafe { instance.destroy_instance(None) };
            return Err(error.into());
        }
    };
    // SAFETY: the device was created with one queue in this family.
    let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family_index, 0) };

    // Every handle and function pointer below is kept valid by leaking the
    // entry, instance and device at the end of this function.
    let context = VulkanContext {
        instance: instance.handle().as_raw() as usize,
        physical_device: physical_device.as_raw() as usize,
        device: device.handle().as_raw() as usize,
        graphics_queue: graphics_queue.as_raw() as usize,
        graphics_queue_family_index,
        get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as *const () as usize,
        get_device_proc_addr: instance.fp_v1_0().get_device_proc_addr as *const () as usize,
    };

    // Deliberate: the render target borrows these for as long as the process
    // runs, and destroying them under a live session would be a use-after-free.
    std::mem::forget(device);
    std::mem::forget(instance);
    std::mem::forget(entry);
    Ok(context)
}

/// The process-wide Metal device the render target allocates its texture on.
/// Leaked deliberately: it outlives every map, and the render target only
/// borrows the pointer.
#[cfg(feature = "metal")]
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
            "fill-extrusion-color": BUILDING_COLOR,
            "fill-extrusion-height": Band::default().height,
            "fill-extrusion-opacity": 0.6,
        },
    })
}

// no per-band colour: the style's light does the colouring, as it did in the
// web demo, so the buildings stay the flat grey it used.

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

impl MapCamera {
    /// This camera with whatever the map reported filled in.
    fn with(self, reported: &CameraOptions) -> Self {
        Self {
            lat: reported.center.map_or(self.lat, |center| center.latitude),
            lon: reported.center.map_or(self.lon, |center| center.longitude),
            zoom: reported.zoom.unwrap_or(self.zoom),
            bearing: reported.bearing.unwrap_or(self.bearing),
            pitch: reported.pitch.unwrap_or(self.pitch),
        }
    }
}

/// Whether a frame is allowed to end the flight the UI believes is in the air.
///
/// Only a frame the render thread drew *after* taking the fly-to on can say it
/// is over. Frames are produced continuously, so one drawn between `fly_to`
/// sending its command and the render thread applying it still reports
/// `flying: false` — and believing it un-suppresses the bearing animation,
/// which then pushes an absolute camera and cancels the flight a tick after it
/// started. That leaves the map at the old zoom, which is what the bug looked
/// like from the outside, and only while a track played, because that is when
/// the animation is running.
fn frame_ends_flight(frame_flight: Option<u64>, frame_flying: bool, waiting_for: u64) -> bool {
    !frame_flying && frame_flight == Some(waiting_for)
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

    /// How long each settle pass gives the runtime. Generous next to the app's
    /// `PUMP_BUDGET`, because a test starts with a cold cache and the style and
    /// its first tiles have to come over the network.
    const SETTLE_PUMP: Duration = Duration::from_millis(50);

    /// How long a settle runs. Wall clock, not a frame count: what it is
    /// waiting for is the network, and a frame count measures the GPU. Ninety
    /// frames is four seconds against a cold cache and eight hundredths of a
    /// second against a warm one, so counting frames made these tests pass or
    /// fail on whether the last run had left the buildings lying around.
    const SETTLE_DEADLINE: Duration = Duration::from_secs(10);

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
        assert_eq!(view.pitch, DEFAULT_PITCH - 30.0);
        assert_eq!(view.bearing, 110.0);
        // The user's own camera is untouched, so the effect decays cleanly.
        assert_eq!(controller.camera.zoom, 16.0);
        assert_eq!(controller.camera.bearing, 10.0);

        // Boosts are clamped to what the map can actually show.
        controller.boost = CameraBoost {
            zoom: 50.0,
            pitch: 90.0,
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
    fn the_buildings_are_flat_grey() {
        // The style's light colours the scene, as it did in the web demo, so
        // the buildings themselves must not be tinted per band.
        let json = building_layer_json(3, "a");
        assert_eq!(
            json["paint"]["fill-extrusion-color"],
            serde_json::json!(BUILDING_COLOR)
        );
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
    fn a_stale_frame_cannot_end_a_flight() {
        // Drawn before the fly-to reached the render thread: it belongs to the
        // flight before this one, or to none at all.
        assert!(!frame_ends_flight(None, false, 1));
        assert!(!frame_ends_flight(Some(0), false, 1));
        // In the air, so not over whatever it says about the flight.
        assert!(!frame_ends_flight(Some(1), true, 1));
        // Drawn after the command landed, and reporting the flight done.
        assert!(frame_ends_flight(Some(1), false, 1));
        // Still true for later frames, so a dropped landing frame is not the
        // only chance the UI gets.
        assert!(frame_ends_flight(Some(1), false, 1));
    }

    #[test]
    fn nearby_bands_do_not_trigger_a_layer_rebuild() {
        let base = Band { height: 100.0 };
        assert!(base.close_to(Band { height: 101.0 }));
        assert!(!base.close_to(Band { height: 110.0 }));
    }

    /// Opt-in: MapLibre Native flies the camera itself now, so check that a
    /// fly-to actually arrives and reports that it finished.
    #[test]
    fn a_fly_to_reaches_its_destination() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            eprintln!("skipped: set OSM_SOUND_DEMO_RENDERER_TESTS=1 to run");
            return;
        }
        // Generous: the loop has to cover the flight in wall-clock time.
        const PATIENCE: u32 = 4000;
        let target = MapCamera {
            lat: 34.7034131,
            lon: 135.4975879,
            zoom: 12.0,
            ..MapCamera::default()
        };

        let mut engine = Engine::new((480, 360));
        // Let the map come up before asking it to travel.
        for _ in 0..30 {
            engine.pump(Some(SETTLE_PUMP));
            engine.mark_dirty();
            engine.render().expect("warm-up frame");
        }

        engine.apply(Command::FlyTo {
            camera: target,
            duration_ms: Some(800.0),
            id: 7,
        });

        let mut moved_midway = false;
        let mut landed_after = None;
        let mut landing = None;
        for frame in 0..PATIENCE {
            engine.pump(Some(Duration::from_millis(4)));
            let Some(rendered) = engine.render() else {
                continue;
            };
            // The map reports its own camera while it is in the air.
            if rendered.flying {
                if let Some(camera) = rendered.camera {
                    // Somewhere between the start and the destination.
                    if camera.lon < MapCamera::default().lon - 0.5 && camera.lon > target.lon + 0.5
                    {
                        moved_midway = true;
                    }
                }
            } else if engine.flight_id.is_none() {
                landed_after = Some(frame);
                landing = Some(rendered);
                break;
            }
        }

        let frames = landed_after.unwrap_or_else(|| {
            panic!(
                "the fly-to never finished: flight_id {:?}, camera {:?}",
                engine.flight_id, engine.camera
            )
        });
        eprintln!("flew in {frames} frames, midway sample seen: {moved_midway}");
        let landing = landing.expect("the landing frame");
        // The engine knowing where it landed is not enough: the UI only ever
        // learns the camera from a frame, so the destination has to be on the
        // one that reports the flight over.
        let reported = landing
            .camera
            .expect("the landing frame reports where the map stopped");
        assert!(
            (reported.zoom - target.zoom).abs() < 0.01,
            "the landing frame reported zoom {}, wanted {}",
            reported.zoom,
            target.zoom
        );
        assert!(
            moved_midway,
            "the camera jumped rather than flying: no midway position was reported"
        );
        assert!(
            (engine.camera.lat - target.lat).abs() < 0.01
                && (engine.camera.lon - target.lon).abs() < 0.01,
            "landed at {:?}, wanted {target:?}",
            engine.camera
        );
        // The zoom is part of the destination: landing over the right place at
        // the wrong scale is still the wrong camera.
        assert!(
            (engine.camera.zoom - target.zoom).abs() < 0.01,
            "landed at zoom {}, wanted {}",
            engine.camera.zoom,
            target.zoom
        );
    }

    /// Opt-in: an absolute camera from the UI has to take the wheel back.
    #[test]
    fn a_camera_command_cancels_a_fly_to() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            return;
        }
        let mut engine = Engine::new((480, 360));
        for _ in 0..30 {
            engine.mark_dirty();
            engine.render().expect("warm-up frame");
        }

        engine.apply(Command::FlyTo {
            camera: MapCamera {
                lat: -1.279803,
                lon: 36.816647,
                zoom: 12.0,
                ..MapCamera::default()
            },
            duration_ms: Some(6000.0),
            id: 11,
        });
        engine.render().expect("a frame starts the flight");
        assert!(engine.flight_id.is_some(), "the flight never started");

        let taken_back = MapCamera {
            lat: 43.06868,
            lon: 141.35079,
            ..MapCamera::default()
        };
        engine.apply(Command::Camera(taken_back));
        assert!(engine.flight_id.is_none(), "the flight was not cancelled");

        engine.render().expect("a frame after the cancel");
        assert!(
            (engine.camera.lon - taken_back.lon).abs() < 0.01,
            "the map kept flying: {:?}",
            engine.camera
        );
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
        // arrive before comparing anything. Pumping is what makes them arrive:
        // the style load and every tile finish on the runtime, so a loop that
        // only renders draws an empty map for as long as it runs.
        let settle = |engine: &mut Engine| {
            let started = Instant::now();
            let mut last = None;
            while started.elapsed() < SETTLE_DEADLINE {
                engine.pump(Some(SETTLE_PUMP));
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

        engine.apply(Command::Bands(Box::new([Band { height: 220.0 }; BINS])));
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
        let warm_up = Instant::now();
        while warm_up.elapsed() < SETTLE_DEADLINE {
            engine.pump(Some(SETTLE_PUMP));
            engine.mark_dirty();
            engine.render().expect("warm-up frame");
        }

        const BUDGET: Duration = Duration::from_secs(5);

        fn rate(engine: &mut Engine, mut step: impl FnMut(&mut Engine, u32)) -> f64 {
            let started = Instant::now();
            let mut frames = 0u32;
            while started.elapsed() < BUDGET {
                step(engine, frames);
                // As the render thread does: without it the map never finishes
                // loading and this times an empty frame.
                engine.pump(Some(PUMP_BUDGET));
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
        let warm_up = Instant::now();
        while warm_up.elapsed() < SETTLE_DEADLINE {
            engine.pump(Some(SETTLE_PUMP));
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
