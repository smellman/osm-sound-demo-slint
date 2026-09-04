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
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use maplibre_native::tile_server_options::TileServerOptions;
use maplibre_native::{
    AnyLayer, CameraUpdate, Continuous, ImageRenderer, ImageRendererBuilder, LatLng,
    ResourceOptions, RunLoopHandle,
};

use crate::Size;
use crate::audio::BINS;

pub const DEFAULT_STYLE_URL: &str =
    "https://tile.openstreetmap.jp/styles/maptiler-toner-ja/style.json";

/// Vector source and source-layer holding building footprints in the
/// OpenMapTiles schema used by tile.openstreetmap.jp.
const BUILDING_SOURCE: &str = "openmaptiles";
const BUILDING_SOURCE_LAYER: &str = "building";

/// Buildings are split into `BINS` layers by their true height, so each
/// frequency band drives its own slice of the skyline.
const MAX_BUILDING_HEIGHT: f64 = 200.0;

/// Rewriting the layer is not free, so a band counts as unchanged until its
/// target moves by more than this many metres, or this many degrees of hue.
const HEIGHT_EPSILON: f64 = 2.0;
const HUE_EPSILON: f64 = 4.0;

/// Floor on how often the band layers are rewritten.
///
/// Rewriting a layer makes MapLibre Native re-run tile layout for the building
/// source, and that is the most expensive thing this demo asks of the map: at
/// 1920x1200 it takes the frame rate from 14.0 fps to 8.2.
///
/// What matters is how often a pass touches the layer set at all, not how many
/// layers it touches — one swap and sixteen measured the same (8.3 fps against
/// 8.1), because either way the whole source is laid out again. So rewriting a
/// few bands per pass, round-robin, bought nothing and is not what this does;
/// the bands all move together, less often. Passes in between then render at
/// the map's own speed.
///
/// The interval has to clear the frame time to do anything: at 8 fps a pass
/// arrives every 125 ms, so anything under that lets every pass rewrite. At
/// 1920x1200 the band load cost 7.0 fps on every pass, 9.8 at this interval,
/// and 11.4 at 400 ms — but 400 ms leaves the skyline stepping 2.5 times a
/// second, which for something following music reads as broken. This is the
/// point where most of the frame rate is back and the animation still moves.
///
/// `OSM_SOUND_DEMO_BAND_INTERVAL_MS` overrides it, `0` rewriting every pass.
const BAND_REWRITE_INTERVAL: Duration = Duration::from_millis(150);

/// How long the map keeps rendering after the last change. Tiles arrive
/// asynchronously, so stopping at the first frame after a move would leave
/// whatever landed later undrawn.
const SETTLE_WINDOW: Duration = Duration::from_secs(3);

/// Run-loop turns per render pass. One, on every platform.
///
/// Draining harder looks like it should help — a pass queues far more work than
/// one turn dispatches, since each of the sixteen layer swaps re-runs tile
/// layout for the building source — but it measures worse, and not only on
/// Darwin, where `RunLoop::runOnce` parks rather than returning straight away
/// and 32 turns cost about 100 ms a pass against 4 ms for one. On Linux, where
/// a turn is `UV_RUN_NOWAIT` and returns immediately, the map idled at 21 fps
/// on one turn, 6 on eight, and 3 on thirty-two: the turns dispatch the layout
/// work a render then waits on anyway, so doing more per pass only moves the
/// wait.
///
/// `OSM_SOUND_DEMO_RUN_LOOP_TICKS` overrides it, which is how that was
/// measured.
const RUN_LOOP_TICKS_PER_FRAME: u32 = 1;

/// How long the render thread waits for a command before ticking MapLibre
/// Native's run loop anyway, so in-flight tile requests keep progressing while
/// the map is otherwise idle.
const IDLE_TICK: Duration = Duration::from_millis(16);

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

    fn cancel_flight(&mut self) {
        self.flight = None;
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
    commands: Sender<Command>,
    frames: Receiver<Frame>,
    /// Frames the render thread has finished, whether or not the UI picked them
    /// up. Compared against the UI's own count, this says which side of the
    /// channel a low frame rate is coming from.
    rendered: Arc<AtomicU64>,
    controller: CameraController,
    size: (u32, u32),
    style_loaded: bool,
    map_idle: bool,
}

impl MapLibre {
    fn new(size: (u32, u32)) -> Self {
        let (commands, command_rx) = std::sync::mpsc::channel();
        // A single slot: if the UI falls behind there is no point queueing stale
        // frames, the newest one is always the one worth showing.
        let (frame_tx, frames) = sync_channel(1);
        let rendered = Arc::new(AtomicU64::new(0));
        std::thread::Builder::new()
            .name("maplibre-render".to_owned())
            .spawn({
                let rendered = Arc::clone(&rendered);
                move || render_thread(size, command_rx, frame_tx, &rendered)
            })
            .expect("spawning the map render thread");

        Self {
            commands,
            frames,
            rendered,
            controller: CameraController::default(),
            size,
            style_loaded: false,
            map_idle: false,
        }
    }

    fn send(&self, command: Command) {
        // The render thread only goes away when the app is shutting down.
        let _ = self.commands.send(command);
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

    pub fn style_loaded(&self) -> bool {
        self.style_loaded
    }

    pub fn map_idle(&self) -> bool {
        self.map_idle
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
        if newest.is_some() {
            self.style_loaded = true;
            self.map_idle = true;
        }
        newest
    }

    /// Applies the style and camera declared on the Slint `MMapView`.
    /// `MAPLIBRE_STYLE_URL` overrides the declared style, matching the
    /// maplibre-native-slint demos.
    pub fn apply_initial(
        &mut self,
        style_url: &str,
        lat: f64,
        lon: f64,
        zoom: f64,
        bearing: f64,
        pitch: f64,
    ) {
        let style_url = std::env::var("MAPLIBRE_STYLE_URL")
            .ok()
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| style_url.to_owned());
        if !style_url.is_empty() {
            self.send(Command::Style(style_url));
        }
        self.controller.cancel_flight();
        self.controller.camera = MapCamera {
            lat: clamp_lat(lat),
            lon: normalize_lon(lon),
            zoom: clamp_zoom(zoom),
            bearing: normalize_bearing(bearing),
            pitch: clamp_pitch(pitch),
        };
        self.push_camera();
    }

    pub fn load_style(&mut self, style_url: &str) {
        self.style_loaded = false;
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

    pub fn set_zoom(&mut self, zoom: f64) {
        self.controller.camera.zoom = clamp_zoom(zoom);
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

pub fn create_map(size: Size) -> Rc<RefCell<MapLibre>> {
    Rc::new(RefCell::new(MapLibre::new(safe_size(size))))
}

/// Owns the MapLibre Native renderer for the lifetime of the render thread.
struct Engine {
    renderer: Option<ImageRenderer<Continuous>>,
    /// MapLibre Native delivers tile loads and layer layouts through the run
    /// loop of the thread that owns the map, so it has to be turned for a
    /// render to pick up anything new.
    run_loop: RunLoopHandle,
    /// How many run-loop turns each pass takes. One turn dispatches one queued
    /// task, and a moving camera plus per-frame layer swaps queue far more than
    /// that, so the loop is drained rather than nudged.
    ticks_per_frame: u32,
    cache: PathBuf,
    size: (u32, u32),
    style_url: String,
    camera: MapCamera,
    bands: [Band; BINS],
    applied: [Option<Band>; BINS],
    /// Shortest gap between rewrites of the layer set.
    rewrite_interval: Duration,
    /// When the layers were last rewritten, for the interval above.
    bands_written: Instant,
    dirty: bool,
    /// Until when the map keeps rendering so late-arriving tiles get drawn.
    settling_until: Instant,
}

impl Engine {
    fn new(size: (u32, u32)) -> Self {
        Self {
            renderer: None,
            run_loop: RunLoopHandle::current(),
            ticks_per_frame: run_loop_ticks(),
            cache: cache_path(),
            size,
            style_url: DEFAULT_STYLE_URL.to_owned(),
            camera: MapCamera::default(),
            bands: [Band::default(); BINS],
            applied: [None; BINS],
            rewrite_interval: band_rewrite_interval(),
            bands_written: Instant::now() - SETTLE_WINDOW,
            dirty: true,
            settling_until: Instant::now() + SETTLE_WINDOW,
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Resize(width, height) => {
                if self.size != (width, height) {
                    self.size = (width, height);
                    // Continuous mode can be resized in place, so the renderer
                    // and its layers survive.
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.set_map_size(maplibre_native::Size { width, height });
                    }
                    self.mark_dirty();
                }
            }
            Command::Style(url) => {
                if self.style_url != url {
                    self.style_url = url;
                    self.renderer = None;
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
        self.settling_until = Instant::now() + SETTLE_WINDOW;
    }

    /// Whether another frame is worth rendering: something changed, or the map
    /// is still settling after the last change.
    ///
    /// MapLibre Native's own signals are no help here: in continuous mode
    /// `needs_repaint` never clears, and `onDidBecomeIdle` never fires (checked
    /// against 0.8.7). So the settle window is a timer.
    fn wants_frame(&self) -> bool {
        self.dirty || self.settling_until > Instant::now()
    }

    /// Turns the run loop, dispatching up to `ticks_per_frame` queued tasks.
    fn tick(&self) {
        for _ in 0..self.ticks_per_frame {
            self.run_loop.tick();
        }
    }

    /// Renders one frame, first syncing any band whose layer has fallen behind.
    fn render(&mut self) -> Option<Frame> {
        self.ensure_renderer()?;
        self.sync_bands();
        self.tick();

        let camera = camera_update(self.camera);
        let renderer = self.renderer.as_mut()?;
        renderer.update_camera(&camera);
        renderer.render_once();

        let image = renderer.read_still_image();
        let size = image.size();
        let buffer = image.buffer();

        self.dirty = false;

        // Slint builds the pixel buffer from the reported dimensions, so a
        // short buffer would panic the UI thread rather than show a bad frame.
        let expected = size.width as usize * size.height as usize * 4;
        if buffer.len() != expected {
            eprintln!(
                "skipping a {}x{} frame: got {} bytes, expected {expected}",
                size.width,
                size.height,
                buffer.len()
            );
            return None;
        }

        Some(Frame {
            width: size.width,
            height: size.height,
            rgba: buffer.to_vec(),
        })
    }

    fn ensure_renderer(&mut self) -> Option<()> {
        if self.renderer.is_some() {
            return Some(());
        }
        let url = match self.style_url.parse() {
            Ok(url) => url,
            Err(error) => {
                eprintln!("invalid style URL {}: {error}", self.style_url);
                return None;
            }
        };
        let mut renderer = build_renderer(self.size, &self.cache);
        if let Err(error) = renderer.load_style_from_url(&url).wait() {
            eprintln!("style load failed: {error}");
        }
        self.renderer = Some(renderer);
        Some(())
    }

    /// Rewrites the band layers that have fallen behind their targets, no more
    /// often than [`Engine::rewrite_interval`].
    ///
    /// Every change to the layer set makes MapLibre Native re-run tile layout
    /// for the layer's source, which restarts whatever tiles are still in
    /// flight. Doing that on every pass kept a loading map permanently at the
    /// start line — flying while a track played left the map blank until the
    /// music stopped — and costs a third of the frame rate besides.
    ///
    /// Giving way for longer than this, while tiles from a move are still
    /// landing, is [`crate::app`]'s job: it knows when a fly-to is in flight,
    /// whereas the engine only sees a stream of camera updates and cannot tell
    /// a fly-to from the demo's own bearing spin.
    fn sync_bands(&mut self) {
        let may_update = self.bands_written.elapsed() >= self.rewrite_interval;
        let mut wrote = false;
        for band in 0..BINS {
            let target = self.bands[band];
            match self.applied[band] {
                Some(applied) if applied.close_to(target) => continue,
                // Holding an update is fine; never having created the layer is
                // not, so a missing one is always built.
                Some(_) if !may_update => continue,
                _ => {}
            }
            if self.set_building_layer(band, target) {
                self.applied[band] = Some(target);
                wrote = true;
            }
        }
        if wrote {
            self.bands_written = Instant::now();
        }
    }

    /// Replaces one band's layer. There is no paint-property setter in the Rust
    /// bindings, so the layer is removed and re-added from JSON; either way the
    /// demo's layers stay on top of the style.
    fn set_building_layer(&mut self, band: usize, spec: Band) -> bool {
        let id = building_layer_id(band);
        let json = building_layer_json(band, &id, spec);
        let layer = match AnyLayer::from_json_value(&json) {
            Ok(layer) => layer,
            Err(error) => {
                eprintln!("building layer {band} is invalid: {error}");
                return false;
            }
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };
        let mut style = renderer.style();
        style.remove_layer(&id);
        if let Err(error) = style.add_layer(layer) {
            eprintln!("adding building layer {band} failed: {error}");
            return false;
        }
        true
    }
}

/// Render thread body: coalesce whatever commands are pending, tick MapLibre
/// Native's run loop, then render at most one frame per pass.
fn render_thread(
    size: (u32, u32),
    commands: Receiver<Command>,
    frames: SyncSender<Frame>,
    rendered: &AtomicU64,
) {
    let mut engine = Engine::new(size);
    loop {
        // Only park when there is nothing to draw. Waiting the idle tick out on
        // a frame the engine already wants added `IDLE_TICK` to every pass,
        // which on its own capped the map at 60 fps and, next to a render that
        // costs a few milliseconds, was most of the frame time. Removing it
        // took the idling map from 16 fps to 21.
        if !engine.wants_frame() {
            match commands.recv_timeout(IDLE_TICK) {
                Ok(command) => engine.apply(command),
                Err(RecvTimeoutError::Timeout) => {}
                // The UI dropped its handle: the app is shutting down.
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        loop {
            match commands.try_recv() {
                Ok(command) => engine.apply(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if !engine.wants_frame() {
            // Nothing to draw, but in-flight tile requests still need the run
            // loop turned so they finish and mark the map dirty.
            engine.tick();
            continue;
        }
        if let Some(frame) = engine.render() {
            rendered.fetch_add(1, Ordering::Relaxed);
            // Drop the frame rather than stall if the UI has not consumed the
            // previous one yet.
            let _ = frames.try_send(frame);
        }
    }
}

/// Shortest gap between rewrites of the band layers,
/// `OSM_SOUND_DEMO_BAND_INTERVAL_MS` overriding the default so the trade
/// against the frame rate can be measured.
fn band_rewrite_interval() -> Duration {
    std::env::var("OSM_SOUND_DEMO_BAND_INTERVAL_MS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .map_or(BAND_REWRITE_INTERVAL, Duration::from_millis)
}

fn building_layer_id(band: usize) -> String {
    format!("3d-buildings-{band}")
}

/// Builds the style-spec JSON for one band's extrusion layer. Each band filters
/// buildings by their true height so the skyline is split into `BINS` slices,
/// exactly as the web demo did.
///
fn building_layer_json(band: usize, id: &str, spec: Band) -> serde_json::Value {
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
            "fill-extrusion-color": band_color(spec),
            "fill-extrusion-height": spec.height,
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

fn resource_options(cache: &Path) -> ResourceOptions {
    ResourceOptions::default()
        .with_tile_server_options(&TileServerOptions::default())
        .with_cache_path(cache.to_path_buf())
}

fn camera_update(camera: MapCamera) -> CameraUpdate {
    CameraUpdate::new()
        .center(LatLng {
            lat: camera.lat,
            lng: camera.lon,
        })
        .zoom(camera.zoom)
        .bearing(camera.bearing)
        .pitch(camera.pitch)
}

fn build_renderer(size: (u32, u32), cache: &Path) -> ImageRenderer<Continuous> {
    ImageRendererBuilder::new()
        .with_size(
            NonZeroU32::new(size.0).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(size.1).unwrap_or(NonZeroU32::MIN),
        )
        .with_pixel_ratio(1.0)
        .with_resource_options(resource_options(cache))
        .build_continuous_renderer()
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

/// Run-loop turns per pass, `OSM_SOUND_DEMO_RUN_LOOP_TICKS` overriding the
/// default so the two can be compared in one sitting.
fn run_loop_ticks() -> u32 {
    std::env::var("OSM_SOUND_DEMO_RUN_LOOP_TICKS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(RUN_LOOP_TICKS_PER_FRAME)
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
        let first = building_layer_json(0, "a", Band::default());
        let last = building_layer_json(BINS - 1, "b", Band::default());
        assert_eq!(first["filter"][1][2], serde_json::json!(0.0));
        assert_eq!(last["filter"][2][2], serde_json::json!(MAX_BUILDING_HEIGHT));
    }

    #[test]
    fn the_building_layers_use_constant_paint() {
        let json = building_layer_json(3, "a", Band::default());
        let paint = &json["paint"];
        // Data-driven paint (a `step` / `get` expression) would be re-evaluated
        // per building every frame and costs about twenty times as much.
        assert!(paint["fill-extrusion-height"].is_number(), "{paint}");
        assert!(paint["fill-extrusion-color"].is_string(), "{paint}");
    }

    #[test]
    fn band_rewrites_are_rate_capped() {
        assert_eq!(band_rewrite_interval(), BAND_REWRITE_INTERVAL);
        // An interval under the frame time lets every pass rewrite, which is
        // the case the cap exists to avoid. 8 fps is 125 ms a pass.
        assert!(
            BAND_REWRITE_INTERVAL >= Duration::from_millis(125),
            "{BAND_REWRITE_INTERVAL:?} is inside a frame"
        );
    }

    #[test]
    fn the_rate_cap_holds_rewrites_well_below_the_tick_rate() {
        // The UI posts bands every 16 ms; the cap must let through far fewer.
        let ticks_per_rewrite = BAND_REWRITE_INTERVAL.as_millis() / 16;
        assert!(
            ticks_per_rewrite >= 3,
            "{ticks_per_rewrite} ticks per rewrite"
        );
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

    /// Opt-in comparison of MapLibre Native's two renderer modes, kept as the
    /// record of why the app uses the continuous one.
    ///
    /// `Static` is `renderStill`: it re-renders from scratch and re-lays out the
    /// building tiles on every change to the layer set. `Continuous` keeps the
    /// map alive between frames.
    #[test]
    fn report_static_vs_continuous() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            return;
        }
        const SIZE: (u32, u32) = (960, 640);
        const ROUNDS: u32 = 5;

        let url = DEFAULT_STYLE_URL.parse().expect("style URL");
        let mut bands = [Band::default(); BINS];

        // --- Static: one still render per frame ---
        let mut still = ImageRendererBuilder::new()
            .with_size(
                NonZeroU32::new(SIZE.0).unwrap(),
                NonZeroU32::new(SIZE.1).unwrap(),
            )
            .with_pixel_ratio(1.0)
            .with_resource_options(resource_options(&cache_path()))
            .build_static_renderer();
        still.load_style_from_url(&url).wait().expect("style loads");
        for (band, spec) in bands.iter().enumerate() {
            let id = building_layer_id(band);
            let layer = AnyLayer::from_json_value(&building_layer_json(band, &id, *spec))
                .expect("layer JSON");
            still.style().add_layer(layer).expect("layer added");
        }
        still
            .render_static(&camera_update(MapCamera::default()))
            .expect("warm-up frame");

        let mut static_bands = std::time::Duration::ZERO;
        for step in 1..=ROUNDS {
            let nudge = f64::from(step);
            bands = [Band {
                height: 20.0 * nudge,
                hue: 30.0 * nudge,
                level: 0.5,
            }; BINS];
            let started = std::time::Instant::now();
            for (band, spec) in bands.iter().enumerate() {
                let id = building_layer_id(band);
                let layer = AnyLayer::from_json_value(&building_layer_json(band, &id, *spec))
                    .expect("layer JSON");
                let mut style = still.style();
                style.remove_layer(&id);
                style.add_layer(layer).expect("layer added");
            }
            still
                .render_static(&camera_update(MapCamera {
                    bearing: nudge * 2.0,
                    ..MapCamera::default()
                }))
                .expect("static frame");
            static_bands += started.elapsed();
        }
        drop(still);

        // --- Continuous: the path the app uses ---
        let mut engine = Engine::new(SIZE);
        for _ in 0..TEST_SETTLE_FRAMES {
            engine.mark_dirty();
            engine.render().expect("warm-up frame");
        }
        let mut continuous_bands = std::time::Duration::ZERO;
        for step in 1..=ROUNDS {
            let nudge = f64::from(step);
            engine.apply(Command::Camera(MapCamera {
                bearing: nudge * 2.0,
                ..MapCamera::default()
            }));
            engine.apply(Command::Bands(Box::new(
                [Band {
                    height: 20.0 * nudge + 100.0,
                    hue: 30.0 * nudge + 5.0,
                    level: 0.5,
                }; BINS],
            )));
            let started = std::time::Instant::now();
            engine.render().expect("continuous frame");
            continuous_bands += started.elapsed();
        }

        eprintln!(
            "960x640, camera + 16 layer swaps per frame — static: {:?}, continuous: {:?}",
            static_bands / ROUNDS,
            continuous_bands / ROUNDS,
        );
    }
}
