//! Headless MapLibre Native rendering, driven from a dedicated thread.
//!
//! Adapted from the Rust reference implementation in
//! <https://github.com/maplibre/maplibre-native-slint>, extended with the
//! sound-reactive 3D building layers of the original web demo.
//!
//! MapLibre Native completes a still render by pumping its own run loop, which
//! on macOS is the process CoreFoundation run loop. Doing that from inside a
//! Slint callback re-enters winit's event handling and aborts the process, so
//! the renderer lives on its own thread: the UI thread only posts commands and
//! picks up finished frames.

use std::cell::RefCell;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, sync_channel};

use maplibre_native::tile_server_options::TileServerOptions;
use maplibre_native::{
    AnyLayer, CameraUpdate, ImageRenderer, ImageRendererBuilder, LatLng, ResourceOptions,
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

/// Re-adding a layer is not free, so a band is only rebuilt once its target
/// moved by more than this many metres, or this many degrees of hue.
const HEIGHT_EPSILON: f64 = 2.0;
const HUE_EPSILON: f64 = 4.0;

/// Any change to the layer set makes MapLibre Native re-lay out the building
/// tiles, and that costs the same whether one band moves or all sixteen do
/// (measured at roughly 40 ms against a 6 ms plain render, see
/// `report_frame_costs`). So bands are always swapped in one batch, and a batch
/// is held back until the bands have accumulated this much movement — in metres
/// summed over all of them — to be worth a re-layout.
const SWAP_DRIFT_THRESHOLD: f64 = 24.0;

const MIN_ZOOM: f64 = 0.0;
const MAX_ZOOM: f64 = 22.0;
const MIN_PITCH: f64 = 0.0;
/// MapLibre Native clamps the camera at 60°, so the web demo's 70° is not
/// reachable here.
const MAX_PITCH: f64 = 60.0;
const MAX_ABS_LAT: f64 = 85.0;
const WHEEL_STEP: f64 = 0.5;
const DOUBLE_CLICK_STEP: f64 = 1.0;

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
    Bands([Band; BINS]),
}

#[derive(Debug)]
struct DragState {
    x: f32,
    y: f32,
}

/// Camera state and the pointer interactions that change it. Kept free of any
/// rendering so it can be exercised in tests.
#[derive(Debug, Default)]
struct CameraController {
    camera: MapCamera,
    drag_state: Option<DragState>,
}

impl CameraController {
    fn fly_to(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.camera.lat = clamp_lat(lat);
        self.camera.lon = normalize_lon(lon);
        self.camera.zoom = clamp_zoom(zoom);
        self.drag_state = None;
    }

    fn mouse_moved(&mut self, x: f32, y: f32) -> bool {
        let Some(last) = self.drag_state.as_mut() else {
            return false;
        };
        let dx = f64::from(x - last.x);
        let dy = f64::from(y - last.y);
        last.x = x;
        last.y = y;

        // Screen-space drag has to be un-rotated by the current bearing,
        // otherwise the map runs off sideways while the demo spins the camera.
        let bearing = self.camera.bearing.to_radians();
        let east = dx * bearing.cos() - dy * bearing.sin();
        let north = -dx * bearing.sin() - dy * bearing.cos();

        let (lon_per_px, lat_per_px) = degrees_per_pixel(self.camera.zoom, self.camera.lat);
        self.camera.lon = normalize_lon(self.camera.lon - east * lon_per_px);
        self.camera.lat = clamp_lat(self.camera.lat - north * lat_per_px);
        true
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
    commands: Sender<Command>,
    frames: Receiver<Frame>,
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
        std::thread::Builder::new()
            .name("maplibre-render".to_owned())
            .spawn(move || render_thread(size, command_rx, frame_tx))
            .expect("spawning the map render thread");

        Self {
            commands,
            frames,
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
        self.send(Command::Camera(self.controller.camera));
    }

    pub fn camera(&self) -> MapCamera {
        self.controller.camera
    }

    pub fn style_loaded(&self) -> bool {
        self.style_loaded
    }

    pub fn map_idle(&self) -> bool {
        self.map_idle
    }

    /// Takes the newest finished frame, if the render thread produced one since
    /// the last call.
    pub fn take_frame(&mut self) -> Option<Frame> {
        let mut newest = None;
        loop {
            match self.frames.try_recv() {
                Ok(frame) => newest = Some(frame),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if newest.is_some() {
            self.style_loaded = true;
            self.map_idle = true;
        }
        newest
    }

    /// Applies the style and camera declared on the Slint `MMapView`.
    pub fn apply_initial(
        &mut self,
        style_url: &str,
        lat: f64,
        lon: f64,
        zoom: f64,
        bearing: f64,
        pitch: f64,
    ) {
        if !style_url.is_empty() {
            self.send(Command::Style(style_url.to_owned()));
        }
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

    pub fn fly_to(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.controller.fly_to(lat, lon, zoom);
        self.push_camera();
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
    pub fn apply_levels(&mut self, levels: &[f32; BINS], hue_offset: f64) {
        let mut bands = [Band::default(); BINS];
        for (band, (slot, level)) in bands.iter_mut().zip(levels.iter()).enumerate() {
            let level = f64::from(*level);
            *slot = Band {
                height: 10.0 + 4.0 * band as f64 + level * 255.0,
                hue: (hue_offset + band as f64 * 6.0).rem_euclid(360.0),
                level,
            };
        }
        self.send(Command::Bands(bands));
    }

    /// Resets every band back to the flat, unlit state used when nothing plays.
    pub fn reset_levels(&mut self) {
        self.send(Command::Bands([Band::default(); BINS]));
    }
}

pub fn create_map(size: Size) -> Rc<RefCell<MapLibre>> {
    Rc::new(RefCell::new(MapLibre::new(safe_size(size))))
}

/// Owns the MapLibre Native renderer for the lifetime of the render thread.
struct Engine {
    renderer: Option<ImageRenderer<maplibre_native::Static>>,
    size: (u32, u32),
    style_url: String,
    camera: MapCamera,
    bands: [Band; BINS],
    applied: [Option<Band>; BINS],
    dirty: bool,
}

impl Engine {
    fn new(size: (u32, u32)) -> Self {
        Self {
            renderer: None,
            size,
            style_url: DEFAULT_STYLE_URL.to_owned(),
            camera: MapCamera::default(),
            bands: [Band::default(); BINS],
            applied: [None; BINS],
            dirty: true,
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Resize(width, height) => {
                if self.size != (width, height) {
                    self.size = (width, height);
                    // The still renderer is fixed-size, so it has to be rebuilt.
                    self.renderer = None;
                    self.applied = [None; BINS];
                    self.dirty = true;
                }
            }
            Command::Style(url) => {
                if self.style_url != url {
                    self.style_url = url;
                    self.renderer = None;
                    self.applied = [None; BINS];
                    self.dirty = true;
                }
            }
            Command::Camera(camera) => {
                if self.camera != camera {
                    self.camera = camera;
                    self.dirty = true;
                }
            }
            Command::Bands(bands) => {
                if self.bands != bands {
                    self.bands = bands;
                    self.dirty = true;
                }
            }
        }
    }

    /// Renders one frame, first syncing the bands whose targets moved far
    /// enough to be worth rebuilding their layers.
    fn render(&mut self) -> Option<Frame> {
        self.ensure_renderer()?;
        self.sync_bands();

        let camera = camera_update(self.camera);
        let renderer = self.renderer.as_mut()?;
        match renderer.render_static(&camera) {
            Ok(image) => {
                self.dirty = false;
                let buffer = image.as_image();
                Some(Frame {
                    width: buffer.width(),
                    height: buffer.height(),
                    rgba: buffer.as_raw().clone(),
                })
            }
            Err(error) => {
                eprintln!("map render failed: {error}");
                self.dirty = false;
                None
            }
        }
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
        let mut renderer = build_renderer(self.size);
        if let Err(error) = renderer.load_style_from_url(&url).wait() {
            eprintln!("style load failed: {error}");
        }
        self.renderer = Some(renderer);
        Some(())
    }

    /// Rebuilds every band whose layer has fallen behind, but only once their
    /// combined movement is worth the re-layout it triggers.
    fn sync_bands(&mut self) {
        let mut stale = Vec::new();
        let mut total_drift = 0.0;
        for band in 0..BINS {
            let target = self.bands[band];
            match self.applied[band] {
                Some(applied) if applied.close_to(target) => {}
                Some(applied) => {
                    total_drift += drift(applied, target);
                    stale.push(band);
                }
                // A band with no layer yet has to be created regardless.
                None => {
                    total_drift = f64::INFINITY;
                    stale.push(band);
                }
            }
        }
        if stale.is_empty() || total_drift < SWAP_DRIFT_THRESHOLD {
            // Skipping is safe: the UI posts fresh bands every tick, so the
            // next command marks the map dirty again.
            return;
        }
        for band in stale {
            let target = self.bands[band];
            if self.set_building_layer(band, target) {
                self.applied[band] = Some(target);
            }
        }
    }

    /// Replaces one building layer. There is no paint-property setter in the
    /// Rust bindings, so the layer is removed and re-added from JSON; either way
    /// the demo's layers stay on top of the style.
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

/// Render thread body: coalesce whatever commands are pending, then render at
/// most one frame per batch. Blocks when there is nothing to do.
fn render_thread(size: (u32, u32), commands: Receiver<Command>, frames: SyncSender<Frame>) {
    let mut engine = Engine::new(size);
    loop {
        // The first command of a batch blocks, so an idle map costs nothing.
        match commands.recv() {
            Ok(command) => engine.apply(command),
            // The UI dropped its handle: the app is shutting down.
            Err(_) => return,
        }
        loop {
            match commands.try_recv() {
                Ok(command) => engine.apply(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if !engine.dirty {
            continue;
        }
        if let Some(frame) = engine.render() {
            // Drop the frame rather than stall if the UI has not consumed the
            // previous one yet.
            let _ = frames.try_send(frame);
        }
    }
}

/// How far a band's applied layer has drifted from its target, in metres, with
/// hue folded in so a colour-only change still eventually gets rebuilt.
fn drift(applied: Band, target: Band) -> f64 {
    let hue_delta = (applied.hue - target.hue).abs();
    (applied.height - target.height).abs() + hue_delta.min(360.0 - hue_delta)
}

fn building_layer_id(band: usize) -> String {
    format!("3d-buildings-{band}")
}

/// Builds the style-spec JSON for one band's extrusion layer. Each band filters
/// buildings by their true height so the skyline is split into `BINS` slices,
/// exactly as the web demo did.
fn building_layer_json(band: usize, id: &str, spec: Band) -> serde_json::Value {
    let bin_width = MAX_BUILDING_HEIGHT / BINS as f64;
    let low = band as f64 * bin_width;
    let high = (band + 1) as f64 * bin_width;
    // Silent bands stay neutral grey (the web demo used a flat "#aaa");
    // saturation and lightness rise with the band level.
    let color = hsl_to_hex(spec.hue, spec.level * 80.0, 50.0 + spec.level * 15.0);
    serde_json::json!({
        "id": id,
        "type": "fill-extrusion",
        "source": BUILDING_SOURCE,
        "source-layer": BUILDING_SOURCE_LAYER,
        "filter": ["all", [">", "render_height", low], ["<=", "render_height", high]],
        "paint": {
            "fill-extrusion-color": color,
            "fill-extrusion-height": spec.height,
            "fill-extrusion-opacity": 0.6,
        },
    })
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
    ((size.width as u32).max(1), (size.height as u32).max(1))
}

fn resource_options() -> ResourceOptions {
    ResourceOptions::default()
        .with_tile_server_options(&TileServerOptions::default())
        .with_cache_path(cache_path())
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

fn build_renderer(size: (u32, u32)) -> ImageRenderer<maplibre_native::Static> {
    ImageRendererBuilder::new()
        .with_size(
            NonZeroU32::new(size.0).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(size.1).unwrap_or(NonZeroU32::MIN),
        )
        .with_pixel_ratio(1.0)
        .with_resource_options(resource_options())
        .build_static_renderer()
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

    fn controller_at(lat: f64, lon: f64, zoom: f64) -> CameraController {
        let mut controller = CameraController::default();
        controller.fly_to(lat, lon, zoom);
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
        let json = building_layer_json(0, "a", Band::default());
        assert_eq!(
            json["paint"]["fill-extrusion-color"],
            serde_json::json!("#808080")
        );
    }

    #[test]
    fn building_layer_bins_cover_the_height_range() {
        let first = building_layer_json(0, "a", Band::default());
        let last = building_layer_json(BINS - 1, "b", Band::default());
        assert_eq!(first["filter"][1][2], serde_json::json!(0.0));
        assert_eq!(last["filter"][2][2], serde_json::json!(MAX_BUILDING_HEIGHT));
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
        let started = std::time::Instant::now();
        let flat = engine.render().expect("the flat frame renders");
        eprintln!("first frame took {:?}", started.elapsed());

        engine.apply(Command::Bands(
            [Band {
                height: 220.0,
                hue: 200.0,
                level: 1.0,
            }; BINS],
        ));
        let started = std::time::Instant::now();
        let tall = engine.render().expect("the extruded frame renders");
        eprintln!("second frame took {:?}", started.elapsed());

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

    /// Opt-in timing probe: isolates the cost of a still render from the cost
    /// of swapping extrusion layers, and of how many are swapped at once.
    #[test]
    fn report_frame_costs() {
        if std::env::var_os("OSM_SOUND_DEMO_RENDERER_TESTS").is_none() {
            return;
        }
        let mut engine = Engine::new((960, 640));
        // Warm up so every band has a layer and the tiles are cached.
        for _ in 0..3 {
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
            engine.apply(Command::Bands(bands));
            one_band += time(&mut engine);

            let bands = [Band {
                height: 20.0 * nudge,
                hue: 30.0 * nudge,
                level: 0.5,
            }; BINS];
            engine.apply(Command::Bands(bands));
            all_bands += time(&mut engine);
        }
        eprintln!(
            "960x640 per frame — camera only: {:?}, 1 band swapped: {:?}, {} bands swapped: {:?}",
            camera_only / ROUNDS,
            one_band / ROUNDS,
            BINS,
            all_bands / ROUNDS,
        );
    }

    #[test]
    fn drift_folds_in_hue_and_wraps_around_the_colour_wheel() {
        let base = Band {
            height: 100.0,
            hue: 10.0,
            level: 0.5,
        };
        assert_eq!(drift(base, base), 0.0);
        assert_eq!(drift(base, Band { height: 130.0, ..base }), 30.0);
        // 350° is 20° away from 10°, not 340°.
        assert_eq!(drift(base, Band { hue: 350.0, ..base }), 20.0);
    }
}
