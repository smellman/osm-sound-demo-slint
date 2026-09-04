//! Application state: release browsing, transport controls, and the frame loop
//! that couples the audio spectrum to the map.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::audio::{Analyzer, AudioPlayer};
use crate::gamepad::{Action, Gamepads};
use crate::map::{self, CameraBoost, MapLibre};
use crate::otherman::{self, ListItem, Release};
use crate::{AppWindow, MMapAdapter};

/// Fly-to destinations, same set as the web demo's dropdown.
const PLACES: &[(&str, f64, f64)] = &[
    ("Tokyo / Japan", 35.680655, 139.767165),
    ("Osaka / Japan", 34.7034131, 135.4975879),
    ("Sapporo / Japan", 43.06868, 141.35079),
    ("Hiroshima / Japan", 34.394377, 132.455486),
    ("Fukuoka / Japan", 33.5898988, 130.4017509),
    ("Sendai / Japan", 38.260128, 140.883518),
    ("Kyoto / Japan", 34.985034, 135.759535),
    ("Shimane / Japan", 35.463968, 133.064008),
    ("Firenze / Italy", 43.777424, 11.248662),
    ("Prishtina / Kosovo", 42.663895, 21.163569),
    ("Nairobi / Kenya", -1.279803, 36.816647),
    ("Manila / Philippines", 14.656875, 121.067019),
];

const FLY_TO_ZOOM: f64 = 16.0;

/// Degrees of bearing per second, matching the web demo's `now / 500`.
const BEARING_DEG_PER_SEC: f64 = 2.0;
/// Degrees of hue per second, matching the web demo's `now / 100`.
const HUE_DEG_PER_SEC: f64 = 10.0;

/// A freshly started track reports an empty queue for a moment; ignore
/// end-of-track detection until it has had a chance to fill.
const END_OF_TRACK_GRACE: Duration = Duration::from_millis(750);

/// How long the skyline stays frozen after a fly-to lands, by default.
///
/// Rewriting the band layers makes MapLibre Native re-run tile layout for the
/// building source, which restarts whatever tiles are still in flight. Doing
/// that sixteen times a frame kept a map that had just flown somewhere new
/// permanently at the start line — it stayed blank until playback stopped. So
/// the animation gives way until the new location has had time to load.
///
/// Only a fly-to counts: the demo spins the bearing continuously while playing,
/// so treating every camera change as a move would freeze the skyline for good.
/// `OSM_SOUND_DEMO_BAND_HOLD_MS` overrides this; `0` restores the old
/// behaviour, which is how to A/B the fix.
const HOLD_AFTER_FLIGHT: Duration = Duration::from_millis(2500);

/// Whether `OSM_SOUND_DEMO_FPS` asked for the frame rate on stderr. The status
/// line only shows it while a track plays, which is no help when the question
/// is why the map is slow in the first place.
fn fps_logging() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OSM_SOUND_DEMO_FPS").is_some())
}

/// Whether the band animation should give way: a fly-to is in progress, or one
/// landed recently enough that its tiles may still be arriving.
fn animation_gives_way(flying: bool, landed: Option<Instant>) -> bool {
    flying || landed.is_some_and(|landed| landed.elapsed() < hold_after_flight())
}

fn hold_after_flight() -> Duration {
    static HOLD: OnceLock<Duration> = OnceLock::new();
    *HOLD.get_or_init(|| {
        std::env::var("OSM_SOUND_DEMO_BAND_HOLD_MS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .map_or(HOLD_AFTER_FLIGHT, Duration::from_millis)
    })
}

/// The status line is refreshed on a timer rather than every tick: it is only
/// read by a human, and rebuilding it 60 times a second repaints the overlay
/// for nothing.
const STATUS_INTERVAL: Duration = Duration::from_millis(200);

/// The gamepad's A button fires a "drop": the camera recoils and whips round
/// while the skyline shoots up, then everything settles back. All of it decays
/// from a hard hit, so the effect reads as an impact rather than a wobble.
const DROP_LENGTH: Duration = Duration::from_millis(2200);
/// Zoom levels pulled back at the moment of impact.
const DROP_ZOOM_OUT: f64 = 2.5;
/// Degrees of pitch flattened at the moment of impact.
const DROP_PITCH: f64 = 30.0;
/// Peak extra bearing, swung out and back over the drop.
const DROP_BEARING: f64 = 220.0;
/// Extra building height and hue speed at the moment of impact, as multipliers.
const DROP_HEIGHT_GAIN: f64 = 1.4;
const DROP_HUE_GAIN: f64 = 9.0;

/// Gamepad sensitivity. Panning is expressed in screen pixels per second so it
/// feels the same at every zoom level, exactly as a drag does.
const STICK_PAN_PX_PER_SEC: f64 = 700.0;
/// Zoom levels per second at full right-stick deflection.
const STICK_ZOOM_PER_SEC: f64 = 1.2;
/// Degrees per second at full right-stick deflection.
const STICK_TURN_DEG_PER_SEC: f64 = 120.0;
/// Volume travelled per second with a trigger held down.
const TRIGGER_VOLUME_PER_SEC: f32 = 0.7;

/// The B button fires an orbit: a full turn around where you are standing,
/// easing in and out, with a gentle push in. Deliberately smooth, where the
/// drop is an impact.
const ORBIT_LENGTH: Duration = Duration::from_millis(3000);
const ORBIT_ZOOM_IN: f64 = 0.8;
/// Hue speed multiplier at the midpoint of an orbit.
const ORBIT_HUE_GAIN: f64 = 2.5;

struct State {
    map: Rc<RefCell<MapLibre>>,
    audio: AudioPlayer,
    analyzer: Analyzer,
    gamepads: Gamepads,
    /// Which entry of `PLACES` L1/R1 steps through.
    place: usize,
    /// When the last fly-to landed, so the skyline can give way while the new
    /// location loads.
    flight_landed: Option<Instant>,
    /// When the current drop effect started, if one is running.
    drop_started: Option<Instant>,
    /// When the current orbit effect started, if one is running.
    orbit_started: Option<Instant>,
    /// Which release the D-pad steps through.
    release_index: usize,
    volume: f32,
    /// Accumulated hue rotation, so the drop can speed it up without the phase
    /// jumping when it decays.
    hue: f64,
    releases: Vec<ListItem>,
    release: Option<Release>,
    track_index: usize,
    playing: bool,
    /// Bumped on every load so a slow download for an abandoned track can be
    /// discarded when it finally arrives.
    generation: u64,
    busy: bool,
    track_started: Instant,
    last_tick: Instant,
    frames: u32,
    /// `rendered_count` at the last frame-rate report, for the delta.
    rendered_shown: u64,
    fps_window: Instant,
    fps: f32,
    status_shown: Instant,
}

impl State {
    fn current_track_title(&self) -> String {
        match (&self.release, self.current_track()) {
            (Some(release), Some(track)) => {
                format!("{} / {}", track.title, release.artists())
            }
            _ => "No track selected, please select a release.".to_string(),
        }
    }

    fn current_track(&self) -> Option<&otherman::Track> {
        self.release
            .as_ref()
            .and_then(|release| release.tracklist.get(self.track_index))
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let ui = AppWindow::new()?;
    // Full screen by default; `OSM_SOUND_DEMO_WINDOWED` is for running it
    // alongside other work.
    if std::env::var_os("OSM_SOUND_DEMO_WINDOWED").is_some() {
        ui.set_windowed(true);
    }
    let map = map::create_map(ui.get_map_size());
    let (audio, analyzer) = AudioPlayer::new().map_err(|e| format!("audio device: {e}"))?;

    let now = Instant::now();
    let state = Rc::new(RefCell::new(State {
        map: Rc::clone(&map),
        audio,
        analyzer,
        gamepads: Gamepads::new(),
        place: 0,
        flight_landed: None,
        drop_started: None,
        orbit_started: None,
        release_index: 0,
        volume: 1.0,
        hue: 0.0,
        releases: Vec::new(),
        release: None,
        track_index: 0,
        playing: false,
        generation: 0,
        busy: false,
        track_started: now,
        last_tick: now,
        frames: 0,
        rendered_shown: 0,
        fps_window: now,
        fps: 0.0,
        status_shown: now,
    }));

    map::init(&ui, &map);
    setup_places(&ui);
    connect_transport(&ui, &state);
    connect_tick(&ui, &state);
    load_release_list(&ui);

    ui.run()?;

    // The event loop has returned, whether from Q, Cmd+Q or the close button.
    // Release everything here rather than leaving it to process exit: dropping
    // the state stops the audio device, and dropping the last handle to the map
    // waits for the render thread, which is what lets MapLibre Native close its
    // tile cache properly.
    STATE.with(|slot| *slot.borrow_mut() = None);
    drop(ui);
    drop(state);
    drop(map);
    Ok(())
}

fn setup_places(ui: &AppWindow) {
    let labels: Vec<SharedString> = PLACES
        .iter()
        .map(|(name, _, _)| SharedString::from(*name))
        .collect();
    ui.set_places(ModelRc::new(VecModel::from(labels)));

    ui.on_place_selected({
        let ui_handle = ui.as_weak();
        move |index| {
            if let Some(ui) = ui_handle.upgrade() {
                fly_to_place(&ui, index.max(0) as usize);
            }
        }
    });
}

/// Flies to one of `PLACES` and keeps the dropdown in step, so the map and the
/// UI agree however the choice was made.
fn fly_to_place(ui: &AppWindow, index: usize) {
    let Some((_, lat, lon)) = PLACES.get(index) else {
        return;
    };
    let _ = with_state(|state| state.place = index);
    ui.set_place_index(index as i32);
    ui.global::<MMapAdapter>()
        .invoke_request_fly_to(*lat as f32, *lon as f32, FLY_TO_ZOOM as f32);
}

/// Steps through `PLACES` for the gamepad's L1 / R1 bumpers.
fn step_place(ui: &AppWindow, delta: isize) {
    let Some(current) = with_state(|state| state.place) else {
        return;
    };
    let next = (current as isize + delta).rem_euclid(PLACES.len() as isize) as usize;
    fly_to_place(ui, next);
}

/// Steps through the release catalogue for the gamepad's D-pad.
fn step_release(ui: &AppWindow, delta: isize) {
    let Some((current, count)) = with_state(|state| (state.release_index, state.releases.len()))
    else {
        return;
    };
    if count == 0 {
        return;
    }
    let next = (current as isize + delta).rem_euclid(count as isize) as usize;
    select_release(ui, next);
}

/// Dispatches one gamepad action.
fn run_action(ui: &AppWindow, state: &Rc<RefCell<State>>, action: Action) {
    match action {
        Action::Play => {
            if !state.borrow().playing {
                start(ui, state);
            }
        }
        Action::Stop => {
            if state.borrow().playing {
                stop(ui, state);
            }
        }
        Action::PreviousPlace => step_place(ui, -1),
        Action::NextPlace => step_place(ui, 1),
        Action::PreviousTrack => step_track(ui, state, -1),
        Action::NextTrack => step_track(ui, state, 1),
        Action::PreviousRelease => step_release(ui, -1),
        Action::NextRelease => step_release(ui, 1),
        Action::Drop => state.borrow_mut().drop_started = Some(Instant::now()),
        Action::Orbit => state.borrow_mut().orbit_started = Some(Instant::now()),
    }
}

/// The orbit envelope: how far through the turn, and how strongly it is
/// pushing. Returns `None` once it has finished.
fn orbit_envelope(started: Instant) -> Option<(f64, f64)> {
    let elapsed = started.elapsed();
    if elapsed >= ORBIT_LENGTH {
        return None;
    }
    let t = elapsed.as_secs_f64() / ORBIT_LENGTH.as_secs_f64();
    // Smoothstep, so the turn starts and finishes gently.
    Some((t * t * (3.0 - 2.0 * t), t))
}

/// The drop envelope: how hard the effect is hitting right now, and how far
/// through it is. Returns `None` once it has finished.
fn drop_envelope(started: Instant) -> Option<(f64, f64)> {
    let elapsed = started.elapsed();
    if elapsed >= DROP_LENGTH {
        return None;
    }
    let t = elapsed.as_secs_f64() / DROP_LENGTH.as_secs_f64();
    // Cubic decay from a hard hit, so it lands and then settles.
    let punch = (1.0 - t).powi(3);
    Some((punch, t))
}

/// Fetches the release catalogue in the background and fills the dropdown.
fn load_release_list(ui: &AppWindow) {
    let ui_handle = ui.as_weak();
    std::thread::spawn(move || {
        let result = otherman::fetch_all_releases();
        let _ = ui_handle.upgrade_in_event_loop(move |ui| match result {
            Ok(releases) => {
                let labels: Vec<SharedString> = releases
                    .iter()
                    .map(|release| SharedString::from(release.label()))
                    .collect();
                ui.set_releases(ModelRc::new(VecModel::from(labels)));
                with_state(|state| state.releases = releases);
                // The dropdown already shows the first entry, so load it to
                // match rather than leaving the transport disabled.
                select_release(&ui, 0);
            }
            Err(error) => {
                eprintln!("fetching the release list failed: {error}");
                ui.set_track_title("Could not load the Otherman Records catalogue.".into());
            }
        });
    });
}

// The application state is reachable from any callback on the UI thread, so
// background results can find it after `upgrade_in_event_loop`.
thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

/// Runs `f` against the application state, returning whatever it produced, or
/// `None` if the state is gone (only during shutdown).
fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> Option<T> {
    STATE.with(|slot| {
        let state = slot.borrow().clone()?;
        Some(f(&mut state.borrow_mut()))
    })
}

fn connect_transport(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    STATE.with(|slot| *slot.borrow_mut() = Some(Rc::clone(state)));

    ui.on_release_selected({
        let ui_handle = ui.as_weak();
        move |index| {
            let Some(ui) = ui_handle.upgrade() else {
                return;
            };
            select_release(&ui, index.max(0) as usize);
        }
    });

    ui.on_toggle_play({
        let ui_handle = ui.as_weak();
        let state = Rc::clone(state);
        move || {
            let Some(ui) = ui_handle.upgrade() else {
                return;
            };
            let playing = state.borrow().playing;
            if playing {
                stop(&ui, &state);
            } else {
                start(&ui, &state);
            }
        }
    });

    ui.on_next_track({
        let ui_handle = ui.as_weak();
        let state = Rc::clone(state);
        move || {
            if let Some(ui) = ui_handle.upgrade() {
                step_track(&ui, &state, 1);
            }
        }
    });

    ui.on_prev_track({
        let ui_handle = ui.as_weak();
        let state = Rc::clone(state);
        move || {
            if let Some(ui) = ui_handle.upgrade() {
                step_track(&ui, &state, -1);
            }
        }
    });

    ui.on_volume_changed({
        let state = Rc::clone(state);
        move |volume| {
            let mut state = state.borrow_mut();
            state.volume = volume;
            state.audio.set_volume(volume);
        }
    });

    ui.on_quit({
        let state = Rc::clone(state);
        move || {
            // Silence the output before the window goes, so quitting does not
            // leave a tail of audio playing through the teardown.
            let mut state = state.borrow_mut();
            state.audio.stop();
            state.playing = false;
            // Any download still in flight is now nobody's track.
            state.generation += 1;
            drop(state);
            if let Err(error) = slint::quit_event_loop() {
                eprintln!("could not quit cleanly: {error}");
            }
        }
    });

    ui.on_open_release({
        let state = Rc::clone(state);
        move || {
            let state = state.borrow();
            if let Some(release) = state.release.as_ref() {
                open_in_browser(&format!("{}{}", otherman::RELEASE_LINK_BASE, release.id));
            }
        }
    });
}

fn select_release(ui: &AppWindow, index: usize) {
    let id = {
        let borrowed = STATE.with(|slot| slot.borrow().clone());
        let Some(state) = borrowed else { return };
        let state = state.borrow();
        match state.releases.get(index) {
            Some(item) if state.release.as_ref().is_some_and(|r| r.id == item.id) => return,
            Some(item) => item.id.clone(),
            None => return,
        }
    };

    ui.set_busy(true);
    let ui_handle = ui.as_weak();
    std::thread::spawn(move || {
        let result = otherman::fetch_release(&id);
        let _ = ui_handle.upgrade_in_event_loop(move |ui| {
            ui.set_busy(false);
            match result {
                Ok(release) => {
                    with_state(|state| {
                        state.audio.stop();
                        state.playing = false;
                        state.busy = false;
                        state.generation += 1;
                        state.release = Some(release);
                        state.release_index = index;
                        state.track_index = 0;
                    });
                    ui.set_release_index(index as i32);
                    ui.set_playing(false);
                    ui.set_has_release(true);
                    refresh_title(&ui);
                }
                Err(error) => {
                    eprintln!("fetching release {id} failed: {error}");
                    ui.set_track_title(format!("Could not load release {id}: {error}").into());
                }
            }
        });
    });
}

fn refresh_title(ui: &AppWindow) {
    let title = STATE
        .with(|slot| slot.borrow().clone())
        .map(|state| state.borrow().current_track_title())
        .unwrap_or_default();
    ui.set_track_title(title.into());
}

/// Starts (or restarts) the current track.
///
/// A worker thread opens the stream and waits only for the first few
/// kilobytes, then hands the reader to the UI thread, which owns the player.
/// The rest of the track keeps arriving while it plays.
fn start(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let (url, generation) = {
        let mut state = state.borrow_mut();
        let Some(track) = state.current_track() else {
            return;
        };
        let url = otherman::absolute_url(&track.url);
        state.generation += 1;
        state.busy = true;
        (url, state.generation)
    };

    ui.set_busy(true);
    let ui_handle = ui.as_weak();
    std::thread::spawn(move || {
        let result = otherman::stream(&url);
        let _ = ui_handle.upgrade_in_event_loop(move |ui| {
            let outcome = with_state(|state| {
                if state.generation != generation {
                    // Superseded while downloading: the newer request owns the
                    // busy flag and the UI, so leave both alone.
                    return None;
                }
                state.busy = false;
                state.playing = false;
                let failure = match result {
                    Ok(reader) => match state.audio.play(reader) {
                        Ok(()) => {
                            state.playing = true;
                            state.track_started = Instant::now();
                            None
                        }
                        Err(error) => Some(format!("Could not play this track: {error}")),
                    },
                    Err(error) => Some(format!("Could not stream this track: {error}")),
                };
                Some((state.playing, failure))
            });

            let Some(Some((playing, failure))) = outcome else {
                return;
            };
            ui.set_busy(false);
            ui.set_playing(playing);
            if let Some(failure) = failure {
                eprintln!("{failure} ({url})");
                ui.set_track_title(failure.into());
            }
        });
    });
}

fn stop(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    {
        let mut state = state.borrow_mut();
        state.audio.stop();
        state.playing = false;
        state.busy = false;
        // Invalidate any download still in flight.
        state.generation += 1;
        state.map.borrow_mut().reset_levels();
    }
    ui.set_playing(false);
    ui.set_busy(false);
}

fn step_track(ui: &AppWindow, state: &Rc<RefCell<State>>, delta: isize) {
    let was_playing = {
        let mut state = state.borrow_mut();
        let Some(release) = state.release.as_ref() else {
            return;
        };
        let len = release.tracklist.len();
        if len == 0 {
            return;
        }
        state.track_index = (state.track_index as isize + delta).rem_euclid(len as isize) as usize;
        state.playing
    };

    refresh_title(ui);
    if was_playing {
        stop(ui, state);
        start(ui, state);
    }
}

fn connect_tick(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let ui_handle = ui.as_weak();
    let state = Rc::clone(state);
    ui.global::<MMapAdapter>().on_tick(move || {
        let Some(ui) = ui_handle.upgrade() else {
            return;
        };
        // Button presses are dispatched outside the state borrow below, since
        // they re-enter through the same transport helpers the UI uses.
        let actions = state.borrow_mut().gamepads.poll();
        for action in actions {
            run_action(&ui, &state, action);
        }

        let mut state = state.borrow_mut();

        let now = Instant::now();
        let delta = now.duration_since(state.last_tick);
        state.last_tick = now;
        let seconds = delta.as_secs_f64();
        let levels = state.analyzer.poll();
        let map = Rc::clone(&state.map);

        // Sticks and D-pad, held rather than pressed.
        let sticks = state.gamepads.sample();
        if sticks.moves_map() {
            let mut map = map.borrow_mut();
            let travel = STICK_PAN_PX_PER_SEC * seconds;
            map.pan_by(
                f64::from(sticks.pan.0) * travel,
                f64::from(sticks.pan.1) * travel,
            );
            map.nudge_zoom(f64::from(sticks.zoom) * STICK_ZOOM_PER_SEC * seconds);
            map.nudge_bearing(f64::from(sticks.turn) * STICK_TURN_DEG_PER_SEC * seconds);
        }
        if sticks.volume != 0.0 {
            let volume = (state.volume + sticks.volume * TRIGGER_VOLUME_PER_SEC * seconds as f32)
                .clamp(0.0, 1.0);
            if volume != state.volume {
                state.volume = volume;
                state.audio.set_volume(volume);
                ui.set_volume(volume * 100.0);
            }
        }

        let flying = {
            let mut map = map.borrow_mut();
            map.advance_flight(delta);
            map.flying()
        };
        if flying {
            state.flight_landed = Some(now);
        }
        let loading = animation_gives_way(flying, state.flight_landed);

        // The drop is applied whether or not a track is playing, so the A
        // button always does something.
        let envelope = state.drop_started.and_then(drop_envelope);
        if envelope.is_none() {
            state.drop_started = None;
        }
        let (punch, drop_t) = envelope.unwrap_or((0.0, 0.0));

        let orbit = state.orbit_started.and_then(orbit_envelope);
        if orbit.is_none() {
            state.orbit_started = None;
        }
        let (turn, orbit_t) = orbit.unwrap_or((0.0, 0.0));

        // Both effects are transient offsets, so they simply add.
        map.borrow_mut().set_boost(CameraBoost {
            zoom: -DROP_ZOOM_OUT * punch + ORBIT_ZOOM_IN * (std::f64::consts::PI * orbit_t).sin(),
            pitch: -DROP_PITCH * punch,
            bearing: DROP_BEARING * (std::f64::consts::PI * drop_t).sin() + 360.0 * turn,
        });

        // The web demo froze the animation during a fly-to too; here it also
        // stays frozen for a moment after landing.
        if state.playing && !loading {
            let mut map = map.borrow_mut();
            map.nudge_bearing(seconds * BEARING_DEG_PER_SEC);
            let hue_gain = 1.0
                + DROP_HUE_GAIN * punch
                + ORBIT_HUE_GAIN * (std::f64::consts::PI * orbit_t).sin();
            state.hue += seconds * HUE_DEG_PER_SEC * hue_gain;
            let gain = 1.0 + DROP_HEIGHT_GAIN * punch;
            map.apply_levels(&levels, state.hue, gain);
        }

        if map::push_state(&ui, &mut map.borrow_mut()) {
            state.frames += 1;
        }

        let window = state.fps_window.elapsed();
        if window >= Duration::from_secs(1) {
            state.fps = state.frames as f32 / window.as_secs_f32();
            if fps_logging() {
                // Two rates, because a low one means different things on either
                // side of the frame channel: the render thread not finishing
                // frames, or the UI not keeping up with the ones it finished.
                let rendered = map.borrow().rendered_count();
                let produced = rendered - state.rendered_shown;
                state.rendered_shown = rendered;
                eprintln!(
                    "fps: {:.1} shown, {:.1} rendered",
                    state.fps,
                    produced as f32 / window.as_secs_f32(),
                );
            }
            state.frames = 0;
            state.fps_window = Instant::now();
        }
        if state.status_shown.elapsed() >= STATUS_INTERVAL {
            state.status_shown = Instant::now();
            let camera = map.borrow().camera();
            // The still renderer only redraws on change, so frames per second
            // is only meaningful while the animation is running.
            let rate = if state.playing {
                format!(" · {:.0} fps", state.fps)
            } else {
                String::new()
            };
            // Naming the pad is the only feedback that it was picked up at all.
            let input = match state.gamepads.name() {
                Some(name) => format!("🎮 {name}"),
                None => "drag to pan, scroll to zoom".to_owned(),
            };
            ui.set_status(
                format!(
                    "{:.4}, {:.4} · z{:.1}{rate} · {input}",
                    camera.lat, camera.lon, camera.zoom
                )
                .into(),
            );
        }

        // Auto-advance at the end of a track, like the web demo's `ended` event.
        let ended = state.playing
            && !state.busy
            && state.track_started.elapsed() > END_OF_TRACK_GRACE
            && state.audio.finished();
        drop(state);
        if ended {
            step_to_next_after_end(&ui);
        }
    });
}

fn step_to_next_after_end(ui: &AppWindow) {
    let Some(state) = STATE.with(|slot| slot.borrow().clone()) else {
        return;
    };
    // `playing` is still set here, so `step_track` restarts on the next track.
    step_track(ui, &state, 1);
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let command = ("xdg-open", vec![url]);

    if let Err(error) = std::process::Command::new(command.0)
        .args(command.1)
        .spawn()
    {
        eprintln!("could not open {url}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_animation_gives_way_only_around_a_fly_to() {
        // Nothing has flown yet, so nothing holds the skyline.
        assert!(!animation_gives_way(false, None));

        // Mid-flight, and for a moment after landing, it gives way.
        assert!(animation_gives_way(true, None));
        assert!(animation_gives_way(false, Some(Instant::now())));

        // Once the new location has had time to load, it animates again. This
        // is the regression that froze the buildings: the demo spins the
        // bearing every tick while playing, so anything keyed on "the camera
        // moved" never expires.
        let long_ago = Instant::now() - hold_after_flight() - Duration::from_millis(1);
        assert!(!animation_gives_way(false, Some(long_ago)));
    }
}
