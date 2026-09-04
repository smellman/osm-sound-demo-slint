//! Application state: release browsing, transport controls, and the frame loop
//! that couples the audio spectrum to the map.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::audio::{Analyzer, AudioPlayer};
use crate::map::{self, MapLibre};
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

struct State {
    map: Rc<RefCell<MapLibre>>,
    audio: AudioPlayer,
    analyzer: Analyzer,
    releases: Vec<ListItem>,
    release: Option<Release>,
    track_index: usize,
    playing: bool,
    /// Bumped on every load so a slow download for an abandoned track can be
    /// discarded when it finally arrives.
    generation: u64,
    busy: bool,
    track_started: Instant,
    started: Instant,
    frames: u32,
    fps_window: Instant,
    fps: f32,
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
    let map = map::create_map(ui.get_map_size());
    let (audio, analyzer) = AudioPlayer::new().map_err(|e| format!("audio device: {e}"))?;

    let now = Instant::now();
    let state = Rc::new(RefCell::new(State {
        map: Rc::clone(&map),
        audio,
        analyzer,
        releases: Vec::new(),
        release: None,
        track_index: 0,
        playing: false,
        generation: 0,
        busy: false,
        track_started: now,
        started: now,
        frames: 0,
        fps_window: now,
        fps: 0.0,
    }));

    map::init(&ui, &map);
    setup_places(&ui);
    connect_transport(&ui, &state);
    connect_tick(&ui, &state);
    load_release_list(&ui);

    ui.run()?;
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
            let Some(ui) = ui_handle.upgrade() else {
                return;
            };
            let Some((_, lat, lon)) = PLACES.get(index.max(0) as usize) else {
                return;
            };
            ui.global::<MMapAdapter>().invoke_request_fly_to(
                *lat as f32,
                *lon as f32,
                FLY_TO_ZOOM as f32,
            );
        }
    });
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

fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            f(&mut state.borrow_mut());
        }
    });
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
        move |volume| state.borrow().audio.set_volume(volume)
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
                        state.generation += 1;
                        state.release = Some(release);
                        state.track_index = 0;
                    });
                    ui.set_playing(false);
                    ui.set_has_release(true);
                    refresh_title(&ui);
                }
                Err(error) => {
                    eprintln!("fetching release {id} failed: {error}");
                    ui.set_track_title(format!("Could not load release {id}.").into());
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

/// Starts (or restarts) the current track: the encoded audio is fetched on a
/// worker thread and handed back to the UI thread, which owns the player.
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
        let result = otherman::download(&url);
        let _ = ui_handle.upgrade_in_event_loop(move |ui| {
            with_state(|state| {
                if state.generation != generation {
                    // Superseded by a newer request while downloading.
                    return;
                }
                state.busy = false;
                match result {
                    Ok(bytes) => match state.audio.play(bytes) {
                        Ok(()) => {
                            state.playing = true;
                            state.track_started = Instant::now();
                        }
                        Err(error) => eprintln!("decoding {url} failed: {error}"),
                    },
                    Err(error) => eprintln!("downloading {url} failed: {error}"),
                }
            });
            let playing = STATE
                .with(|slot| slot.borrow().clone())
                .is_some_and(|state| state.borrow().playing);
            ui.set_busy(false);
            ui.set_playing(playing);
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
        state.track_index =
            (state.track_index as isize + delta).rem_euclid(len as isize) as usize;
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
        let mut state = state.borrow_mut();

        let elapsed = state.started.elapsed().as_secs_f64();
        let levels = state.analyzer.poll();
        let map = Rc::clone(&state.map);

        if state.playing {
            let mut map = map.borrow_mut();
            map.set_bearing(elapsed * BEARING_DEG_PER_SEC);
            map.apply_levels(&levels, elapsed * HUE_DEG_PER_SEC);
        }

        if map::push_state(&ui, &mut map.borrow_mut()) {
            state.frames += 1;
        }

        let window = state.fps_window.elapsed();
        if window >= Duration::from_secs(1) {
            state.fps = state.frames as f32 / window.as_secs_f32();
            state.frames = 0;
            state.fps_window = Instant::now();
        }
        let camera = map.borrow().camera();
        // The still renderer only redraws on change, so frames per second is
        // only meaningful while the animation is running.
        let rate = if state.playing {
            format!(" · {:.0} fps", state.fps)
        } else {
            String::new()
        };
        ui.set_status(
            format!(
                "{:.4}, {:.4} · z{:.1}{rate} · drag to pan, scroll to zoom",
                camera.lat, camera.lon, camera.zoom
            )
            .into(),
        );

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

    if let Err(error) = std::process::Command::new(command.0).args(command.1).spawn() {
        eprintln!("could not open {url}: {error}");
    }
}
