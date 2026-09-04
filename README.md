# OSM Sound Demo (Slint)

A native rebuild of [osm-sound-demo](https://github.com/smellman/osm-sound-demo) — the
"dancing buildings" OpenStreetMap visualiser — on Rust, with
[MapLibre Native](https://github.com/maplibre/maplibre-native-rs) for the map,
[Slint](https://slint.dev/) for the UI and [rodio](https://github.com/RustAudio/rodio)
for audio. No browser, no Web Audio, no DOM.

Pick a release from the [Otherman Records](https://www.otherman-records.com/) catalogue,
press play, and the buildings around you rise and fall with the music.

## How to run

The reusable Slint map components come from a submodule, so clone with it:

```bash
git clone --recurse-submodules <this repo>
# or, in an existing clone:
git submodule update --init vendor/maplibre-native-slint
```

Only that submodule's `.slint` files are used, so `--recursive` is not needed — its own
submodules (maplibre-native and friends) stay unfetched.

```bash
cargo run --release
```

The first build compiles MapLibre Native from source through `maplibre_native`, which
takes a while. Debug builds work but render the map at a few frames per second — use
`--release` for anything you actually want to look at.

### Environment

| Variable | Effect |
| --- | --- |
| `MAPLIBRE_STYLE_URL` | Override the initial style URL |
| `MAPLIBRE_FLY_MS` | Fly-to duration in ms (default: 1.5–6 s, scaled by distance) |
| `OSM_SOUND_DEMO_BAND_HOLD_MS` | How long the skyline stays frozen after a fly-to lands (default 2500). `0` restores the old behaviour — see [Why the skyline pauses](#why-the-skyline-pauses-after-a-fly-to) |

## Controls

### Mouse

| | |
| --- | --- |
| Drag | Pan |
| Scroll | Zoom |
| Double-click | Zoom in |
| Fly To | Fly to one of twelve cities |
| ◀◀ / ▶ / ▶▶ | Previous track, play & stop, next track |
| Vol | Output volume |
| Release dropdown | Load a release; the first one loads on startup |
| Go To Release | Open the release page in your browser |

### Gamepad

Plug in a controller and it is picked up automatically — its name appears in the status
line. Anything the pad does, the mouse can still do.

| | |
| --- | --- |
| Start | Play |
| Select | Stop |
| A | The drop — see [the effects](#the-effects) |
| B | The orbit |
| L1 / R1 | Fly to the previous / next city (the dropdown follows) |
| L2 / R2 | Volume down / up (the slider follows) |
| Left stick | Pan |
| Right stick, left/right | Turn |
| Right stick, up/down | Zoom |
| D-pad left / right | Previous / next track |
| D-pad up / down | Previous / next release |

> **The layout above is an Xbox controller's.** gilrs maps whatever is plugged in onto that
> layout through the SDL_GameControllerDB mappings, so the *positions* are what is fixed,
> not the printed letters. On a Nintendo-style pad, A and B are physically swapped, so the
> drop sits under the button marked B. Pads without an entry in the mapping database may
> land buttons somewhere else entirely.

Panning, turning and zooming all cancel a fly-to in progress, so the pad always wins over
the animation. Reaching for the volume does not.

## How it works

```
Otherman Records API ──► release list & tracklist          (ureq, background threads)
MP3 from archive.org ──► rodio Decoder ──► device
                                └──► Tap ──► ring buffer ──► FFT ──► 16 band levels
                                                                        │
                            camera bearing + band heights/hues ◄────────┘
                                              │
                            render thread ──► MapLibre Native (continuous) ──► Slint Image
```

- `src/audio.rs` — playback plus the spectrum analysis. A `Tap` source sits between the
  decoder and the device, copying every frame into a ring buffer; the UI thread runs a
  1024-point FFT over it and folds the result into 16 linear bands, dB-scaled over
  −90..−10 dB like the web demo's `AnalyserNode`.
- `src/map/renderer.rs` — the map. Sixteen `fill-extrusion` layers split buildings into
  height bins, one per frequency band, and each band drives its layer's extrusion height
  and colour.
- `src/otherman.rs` — the release API client. The native build talks to
  otherman-records.com and archive.org directly; the web demo needed a CORS proxy.
- `src/gamepad.rs` — controller input. [gilrs](https://gitlab.com/gilrs-project/gilrs)
  carries the SDL_GameControllerDB mappings, so `Button::Start` really is Start on
  whatever pad is plugged in. Reading raw HID instead would give button *indices* that
  only line up on XInput-style controllers.
- `ui/app.slint` — the window. `MMapView` and `MMapAdapter` are imported as
  `@maplibre-native-slint/maplibre.slint` from the
  [maplibre-native-slint](https://github.com/maplibre/maplibre-native-slint) submodule in
  `vendor/`; `build.rs` wires that alias up.

### Rendering backend

The backend is chosen per platform in `Cargo.toml`, because `maplibre_native`'s backend
features are mutually exclusive:

| Platform | Feature | MapLibre Native backend |
| --- | --- | --- |
| macOS | `metal` | `MLN_WITH_METAL` |
| Linux, others | `wgpu` | `MLN_WITH_WEBGPU` + wgpu-native |

The map runs in MapLibre Native's **continuous** mode, not still (`renderStill`) mode.
That matters a lot here: still mode re-lays out the building tiles on every change to the
layer set, and this demo changes sixteen layers per frame.

### Differences from the web demo

Some of these are deliberate, some are limits of the current Rust bindings.

- **Rendering runs on its own thread.** MapLibre Native drives its work through its own
  run loop, which on macOS is the process CoreFoundation run loop; pumping that from
  inside a Slint callback re-enters winit's event handling and aborts. The UI thread only
  posts camera and band updates and picks up finished frames.
- **The buildings carry the colour, not the light.** The web demo animated
  `map.setLight({ color, intensity })`. The Rust bindings expose no light settings and no
  paint-property setters, so each band's layer is rebuilt from style-spec JSON with a
  rotating hue instead.
- **Fly-to is eased here, not by MapLibre.** The Rust bindings expose only `jumpTo`, so
  `Fly To` interpolates the camera itself, easing the position and arcing the zoom out at
  the midpoint. A jump would land on a blank map: the camera outruns tile loading, which is
  the same reason the [Raspberry Pi port](https://github.com/yuiseki/pi-maplibre-native-slint-touch/tree/main/hdmi)
  defaults its `MAPLIBRE_FLY_MS` to six seconds. As in the web demo, the building animation
  pauses during a fly.
- **Pitch tops out at 60°**, not the web demo's 70°: MapLibre Native clamps the camera there.
- **Vector tiles come from the style's own source**, not from `planet.pmtiles` — there is no
  `pmtiles://` protocol to register on the native side.
- **No VJ mode and no "Locate Me"** yet. Both need platform work rodio does not cover
  (input capture, CoreLocation).

### The effects

**A — the drop.** Two seconds built out of one decaying envelope (`(1 - t)³`, so it lands
hard and settles) driving four things at once: the camera pulls back two and a half zoom
levels, the pitch flattens by 30°, the bearing whips 220° out and back, and the skyline
shoots up 40% taller with its hue racing ten times faster.

**B — the orbit.** Three seconds of a full 360° turn on a smoothstep, with a gentle push in
at the midpoint and the hue running two and a half times faster. Deliberately the opposite
of the drop: a sweep rather than an impact.

Both are transient offsets (`CameraBoost`) kept separate from the camera the user controls,
so an effect can never strand the map somewhere once it decays. Both fire whether or not a
track is playing, and they simply add if you hit them together.

### Why the skyline pauses after a fly-to

Each of the sixteen band layers is rewritten by removing and re-adding it, because the Rust
bindings expose no paint-property setters. Every change to the layer set makes MapLibre
Native re-run tile layout for the building source — which restarts the tiles still in
flight. Sixteen rewrites a frame at 60 Hz therefore keep a *loading* map permanently at the
start line: flying while a track played left the map blank until the music was stopped.

So the animation gives way while a fly-to is in the air and for
`OSM_SOUND_DEMO_BAND_HOLD_MS` after it lands, and the rewrites are rate capped to one batch
per 50 ms the rest of the time.

Only a fly-to counts as a move. Keying this on "the camera changed" instead does not work:
the demo spins the bearing on every tick while a track plays, so the window never expires
and the buildings stop moving altogether.

### Frame cost

Measured in a release build at 960×640 on an Apple Silicon Mac, one frame carrying both a
camera change and all sixteen band layers being swapped:

| Renderer mode | per frame |
| --- | --- |
| Still (`render_static`) | ~58 ms |
| Continuous (`render_once`) | ~15 ms |

In continuous mode the layer swaps are effectively free — a steady frame costs about 4 ms
whether zero, one or sixteen bands moved, so the bands are simply synced every frame. In
still mode any change to the layer set cost a fixed ~40 ms re-layout, which capped the demo
at roughly 22 fps and needed a batching heuristic to hide. The app now holds 60 fps at
1024×720 while playing.

`report_frame_costs` and `report_static_vs_continuous` re-measure both on your machine.

## Tests

```bash
cargo test
```

Two suites are opt-in because they need the network (and, for the renderer, a graphics
device):

```bash
OSM_SOUND_DEMO_NETWORK_TESTS=1 cargo test          # download and decode a real track
OSM_SOUND_DEMO_RENDERER_TESTS=1 cargo test --release -- --nocapture
```

## Licence

MIT, as with the original demo.

All music in this demo is from [Otherman Records](https://www.otherman-records.com/) and is
licensed CC BY-NC 2.1 JP. Map data © OpenStreetMap contributors; tiles by
tile.openstreetmap.jp (© OpenMapTiles). The dancing-buildings idea comes from
[Mapbox's example](https://docs.mapbox.com/mapbox-gl-js/example/dancing-buildings/).
