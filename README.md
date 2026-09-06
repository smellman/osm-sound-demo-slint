# OSM Sound Demo (Slint)

A native rebuild of [osm-sound-demo](https://github.com/smellman/osm-sound-demo) — the
"dancing buildings" OpenStreetMap visualiser — on Rust, with
[MapLibre Native](https://github.com/maplibre/maplibre-native-ffi) for the map,
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

That C++ build is memory-hungry, and Cargo hands CMake one job per core. On a 16-core,
38 GB machine the default parallelism runs the box out of memory and the build is killed,
so cap it:

```bash
CMAKE_BUILD_PARALLEL_LEVEL=4 cargo build -j 4 --release
```

### Environment

| Variable | Effect |
| --- | --- |
| `MAPLIBRE_STYLE_URL` | Override the initial style URL |
| `MAPLIBRE_FLY_MS` | Fly-to duration in ms (default: 1.5–6 s, scaled by distance) |
| `OSM_SOUND_DEMO_WINDOWED` | Set to open in a window rather than full screen |
| `OSM_SOUND_DEMO_HOME` | `lat,lon` for Locate Me |
| `OSM_SOUND_DEMO_INPUT` | VJ mode's input device, matched on a substring of its name |
| `OSM_SOUND_DEMO_BAND_HOLD_MS` | How long the skyline stays frozen after a fly-to lands (default 2500). Nothing needs this any more — see [The band animation](#the-band-animation) |
| `OSM_SOUND_DEMO_FPS` | Print `shown` and `rendered` frame rates to stderr every second. A gap between them means frames are being dropped at the channel; no gap means the render thread is the limit |
| `OSM_SOUND_DEMO_RENDERER_TESTS` | Run the opt-in renderer tests, which need a GPU and the network |
| `OSM_SOUND_DEMO_RENDER_SIZE` | Size the renderer probes measure at, `<width>x<height>` (default 960x640) |
| `OSM_SOUND_DEMO_RENDER_SCALE` | Render the map at this fraction of its on-screen size and let Slint scale it up (default 1.0, floor 0.25). The single biggest thing you can trade for frame rate — see [Frame rate](#frame-rate) |

## Controls

The window opens **full screen**: this is something to stand in front of and drive from a
gamepad, not a window to keep alongside other work. A full-screen window has no title bar,
so **Escape** leaves full screen, **F** toggles it, and **Q** quits.

Q, Cmd+Q and the close button all shut down the same way: playback stops, any download
still in flight is abandoned, and the render thread is waited for so MapLibre Native closes
its tile cache properly rather than being cut off mid-write.

### Mouse

| | |
| --- | --- |
| Drag | Pan |
| Scroll | Zoom |
| Double-click | Zoom in |
| Escape / F | Leave full screen / toggle it |
| Q | Quit |
| Fly To | Fly to one of twelve cities |
| Locate Me | Fly to `OSM_SOUND_DEMO_HOME` |
| VJ Mode | Follow an input device instead of a track |
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
MP3 from archive.org ──► StreamingRead ──► rodio Decoder ──► device
                         (still arriving)          └──► Tap ──► FFT ──► 16 band levels
                                                                        │
                            camera bearing + band heights/hues ◄────────┘
                                              │
                            render thread ──► MapLibre Native (continuous) ──► Slint Image
```

- `src/audio.rs` — playback plus the spectrum analysis. A `Tap` source sits between the
  decoder and the device, copying every frame into a ring buffer; the UI thread runs a
  1024-point FFT over it and folds the result into 16 linear bands, dB-scaled over
  −90..−10 dB like the web demo's `AnalyserNode`.
- `src/map/renderer.rs` — the map, and the only file that touches MapLibre Native. Sixteen
  `fill-extrusion` layers split buildings into height bins, one per frequency band, and
  each band drives its layer's extrusion height and colour.
- `src/otherman.rs` — the release API client. The native build talks to
  otherman-records.com and archive.org directly; the web demo needed a CORS proxy.
- `src/stream.rs` — tracks are streamed, not downloaded first. rodio's decoder needs
  `Read + Seek`, so `StreamingRead` keeps what has arrived in memory and blocks a read
  that runs past the write head. Playback opens on a 256 KB prebuffer: measured against a
  6.8 MB track, that is 2.5 s to first sound instead of waiting for the lot.
- `src/gamepad.rs` — controller input. [gilrs](https://gitlab.com/gilrs-project/gilrs)
  carries the SDL_GameControllerDB mappings, so `Button::Start` really is Start on
  whatever pad is plugged in. Reading raw HID instead would give button *indices* that
  only line up on XInput-style controllers.
- `ui/app.slint` — the window. `ui/map-view.slint` holds `MapView` and `MapAdapter`: the
  frame the renderer draws into and the pointer input that drives it. They started as the
  reusable components from
  [maplibre-native-slint](https://github.com/maplibre/maplibre-native-slint) and were
  trimmed to what this app uses when the map moved to the FFI binding — that project's
  contract is meant to be filled by its own C++ backend, so there was nothing left to
  track.

### Rendering backend

The map runs on [maplibre-native-ffi](https://github.com/maplibre/maplibre-native-ffi)'s
Rust binding. Its backend is chosen per platform in `Cargo.toml`, because the crate's
backend features are mutually exclusive:

| Platform | Feature | Device created by |
| --- | --- | --- |
| macOS | `metal` | `MTLCreateSystemDefaultDevice` |
| Linux, others | `vulkan` | `ash`, headless: instance, physical device with a graphics queue, and a one-queue logical device — no surface and no swapchain |

Unlike the older `maplibre_native` crate, this one hands the graphics plumbing to the
caller: there is no headless renderer that makes its own device. `src/map/renderer.rs`
creates the device, attaches an *owned texture* render target at the map's size, and reads
the frame back with
`read_premultiplied_rgba8_into` for Slint. The binding downloads a prebuilt native
artifact, so a clean build takes well under a minute rather than compiling MapLibre Native
from source.

The device is created once and leaked on purpose: a render target borrows those handles,
and a map outlives any one session. On the Vulkan side the handles are held as plain
addresses rather than `NativePointer`, which is deliberately `!Send` and so cannot live in
a static.

The map runs in `MapMode::Continuous`, and the render thread drives it with
`RuntimeHandle::pump` plus `drain_events`. Those events are what tell it whether to draw
again — `MapRenderUpdateAvailable`, and `needs_repaint` on `MapRenderFrameFinished` — and
when the style has loaded, which is when the band layers can be added.


### Frame rate

Two things cost this demo its frame rate, and both are measurable with
`report_playing_frame_rate` (see the environment table above for how to run it). Numbers
are Vulkan, release, on an AMD RENOIR integrated GPU.

**The pixels.** Everything the map does scales with the area it covers, tile layout
included, and on a large display that dominates:

| Render size | camera only | camera + 16 bands |
| --- | --- | --- |
| 1920x1200 | 14.0 fps | 8.2 fps |
| 1440x900 | 19.4 fps | 11.3 fps |
| 1280x800 | 23.9 fps | 14.8 fps |
| 960x600 | 33.6 fps | 28.2 fps |

`OSM_SOUND_DEMO_RENDER_SCALE` buys frame rate here at the cost of a soft, upscaled map.
It is 1.0 by default: a Mac on Metal does not need it, and nobody should have the map go
blurry without asking.

**The band layers — under the old bindings.** Rewriting one made MapLibre Native re-run
tile layout for the building source, and what mattered was whether a pass touched the
layer set at all, not how many layers it touched: rewriting one band per pass and
rewriting all sixteen measured the same, 8.3 fps against 8.1. Holding the bands to one
batch per 150 ms took 1920x1200 from 7.0 fps to 9.8, and 1280x800 to 20.4.

None of that applies now. `set_layer_property` does not touch the layer set, so the bands
update every frame and the interval knob is gone. The numbers are kept here because they
are why the app moved to the FFI binding.

Things that turned out not to be the problem, in case they look tempting: turning
MapLibre Native's run loop more times per pass (worse — 21 fps at one turn, 6 at eight,
3 at thirty-two), and dropping frames at the render-thread channel (never happened;
`OSM_SOUND_DEMO_FPS` shows `shown` and `rendered` matching).


### Differences from the web demo

Some of these are deliberate, some are limits of the current Rust bindings.

- **Rendering runs on its own thread.** MapLibre Native drives its work through its own
  run loop, which on macOS is the process CoreFoundation run loop; pumping that from
  inside a Slint callback re-enters winit's event handling and aborts. The UI thread only
  posts camera and band updates and picks up finished frames.
- **Fly-to is MapLibre Native's own.** `Fly To` hands the camera over with
  `MapHandle::fly_to` and follows along until the transition-finished event; MapLibre picks
  the duration from the distance, as the web demo's `flyTo` did, unless `MAPLIBRE_FLY_MS`
  says otherwise. Anything that moves the camera — a drag, the sticks, an effect — cancels
  the flight. As in the web demo, the building animation pauses during a fly.
- **No "hash"**: the web demo kept the camera in the URL, which a native binary has no use
  for.
- **Pitch opens at 70°**, as the web demo did, and goes to 85°. MapLibre Native clamps at
  60° unless `BoundOptions::max_pitch` is raised first — asking for more without that
  silently gives 60 back.
- **Vector tiles come from the style's own source**, not from `planet.pmtiles` — there is no
  `pmtiles://` protocol to register on the native side.
- **Locate Me reads a coordinate**, `OSM_SOUND_DEMO_HOME`, rather than asking the OS. The
  web demo asked the browser; CoreLocation on macOS would mean shipping an app bundle with
  a usage description.
- **No QR code.** It pointed at the web version; About links to the source instead.

### VJ mode

The map can follow what an input device hears rather than a track, so it reacts to a live
mix. The web demo did this with `getUserMedia`; here a thread pulls
[rodio's](https://github.com/RustAudio/rodio) `Microphone` and pushes it through the same
tap the tracks go through, into the same analyser — so bands, light and effects are
unchanged. Nothing is played back: the sound is already coming out of whatever is being
mixed.

Route the sound into an input first, then pick it with `OSM_SOUND_DEMO_INPUT`:

- **macOS**: [Loopback.app](https://rogueamoeba.com/loopback/), or BlackHole
- **Linux**: Helvum with PipeWire

A plain microphone works too, and reacts to the room. Turning VJ mode on stops any track,
since both would be feeding the same analyser.

On macOS the input is behind the microphone permission, and a binary started from a
terminal inherits that terminal's grant. Without it the device opens and delivers silence
rather than failing, so a flat skyline with a device named in the status line means the
permission, not the routing.

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

### Streaming, not downloading

The decoder is pulled from the audio device's callback thread, which is why `StreamingRead`
does two things that a plain buffer would not:

- It reports the stream as **not seekable**, even though it can seek. With `is_seekable`
  set, symphonia seeks to the end to measure the stream — on a partially arrived download
  that means blocking until the whole track is in, exactly what streaming avoids. The
  `Content-Length` is still passed through, so duration is known without a seek.
- A read waits at most ten seconds. Blocking the audio callback is what a dropout sounds
  like, so a stalled download ends the track — and the app moves to the next — rather than
  wedging playback. The buffer normally runs far ahead of the playhead, since the body is
  fetched as fast as the network allows rather than in real time.

Dropping the reader stops the download, so skipping tracks does not leave fetches running.

### The band animation

Each of the sixteen band layers is created once, and the animation then sets
`fill-extrusion-height` and `fill-extrusion-color` on it with `set_layer_property`.

That is the reason this app moved to the FFI binding. The older `maplibre_native` crate had
no paint-property setter, so a band update meant removing and re-adding the layer — and
every change to the layer set makes MapLibre Native re-run tile layout for the building
source. Sixteen of those a frame starved tile loading outright: flying somewhere new while
a track played left the map blank until the music was stopped. Working around it cost a
rate cap on the animation and a hold after every fly-to. A property set has none of that
behind it, so the cap is gone and the bands update every frame.

`OSM_SOUND_DEMO_BAND_HOLD_MS` still freezes the skyline after a fly-to, but nothing needs
it any more; it is left in place pending a look at whether the animation now rides through
a fly cleanly.


### Frame cost

Measured on an Apple Silicon Mac in release, at 1024x720 with a track playing and the band
animation running every frame: **58 fps**, including the CPU read-back of every frame.


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
