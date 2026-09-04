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

## Controls

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
