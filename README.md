# OSM Sound Demo (Slint)

A native rebuild of [osm-sound-demo](https://github.com/smellman/osm-sound-demo) — the
"dancing buildings" OpenStreetMap visualiser — on Rust, with
[MapLibre Native](https://github.com/maplibre/maplibre-native-rs) for the map,
[Slint](https://slint.dev/) for the UI and [rodio](https://github.com/RustAudio/rodio)
for audio. No browser, no Web Audio, no DOM.

Pick a release from the [Otherman Records](https://www.otherman-records.com/) catalogue,
press play, and the buildings around you rise and fall with the music.

## How to run

```bash
cargo run --release
```

The first build compiles MapLibre Native from source through `maplibre_native`, which
takes a while. Debug builds work but render the map at a few frames per second — use
`--release` for anything you actually want to look at.

## Controls

| | |
| --- | --- |
| Drag | Pan |
| Scroll | Zoom |
| Double-click | Zoom in |
| Fly To | Jump to one of twelve cities |
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
                            render thread ──► MapLibre Native still render ──► Slint Image
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
- `ui/app.slint` — the window. `ui/maplibre/` is a vendored copy of the reusable Slint
  components from [maplibre-native-slint](https://github.com/maplibre/maplibre-native-slint).

### Differences from the web demo

Some of these are deliberate, some are limits of the current Rust bindings.

- **Rendering runs on its own thread.** MapLibre Native finishes a still render by pumping
  its own run loop, which on macOS is the process CoreFoundation run loop; doing that from
  inside a Slint callback re-enters winit's event handling and aborts. The UI thread only
  posts camera and band updates and picks up finished frames.
- **The buildings carry the colour, not the light.** The web demo animated
  `map.setLight({ color, intensity })`. The Rust bindings expose no light settings and no
  paint-property setters, so each band's layer is rebuilt from style-spec JSON with a
  rotating hue instead.
- **Pitch tops out at 60°**, not the web demo's 70°: MapLibre Native clamps the camera there.
- **Vector tiles come from the style's own source**, not from `planet.pmtiles` — there is no
  `pmtiles://` protocol to register on the native side.
- **No VJ mode and no "Locate Me"** yet. Both need platform work rodio does not cover
  (input capture, CoreLocation).

### Frame cost

Changing the layer set makes MapLibre Native re-lay out the building tiles, and that costs
the same whether one band moves or all sixteen do. Measured in a release build at 960×640:

| | per frame |
| --- | --- |
| Camera change only | ~7 ms |
| Any number of bands swapped | ~44 ms |

So bands are always swapped in one batch, and a batch is held back until the bands have
moved enough to be worth the re-layout (`SWAP_DRIFT_THRESHOLD`). In practice the app runs
at around 40 fps at 1024×720 while playing. `report_frame_costs` re-measures this on your
machine.

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
