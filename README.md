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
cargo run --release --features opengl
```

A rendering backend has to be named — `opengl`, `vulkan` or `metal`. See
[Rendering backend](#rendering-backend) for which to pick and why there is no default.

Debug builds work but render the map at a few frames per second, so use `--release` for
anything you actually want to look at. The toolchain is pinned by `rust-toolchain.toml`
and rustup will fetch it on first build; the pin is not a preference, and the file says
what breaks without it.

### Prerequisites

`maplibre-native-ffi` downloads a prebuilt native artifact rather than building MapLibre
Native from source, so there is no C++ toolchain to set up and a clean build takes about a
minute. What is still needed is the system libraries the Rust crates link against and the
one that generates the FFI bindings. On Debian and Raspberry Pi OS:

```bash
sudo apt install libfontconfig-dev libasound2-dev libudev-dev libssl-dev clang libclang-dev
```

| Package | Wanted by |
| --- | --- |
| `libfontconfig-dev` | Slint, to find the fonts it draws the UI text with |
| `libasound2-dev` | ALSA, which `rodio` plays through |
| `libudev-dev` | `gilrs`, to enumerate gamepads |
| `libssl-dev` | OpenSSL, pulled in through `ureq`'s `native-tls` |
| `clang`, `libclang-dev` | `bindgen`, which generates the FFI bindings from the native library's C header at build time |

That is the list a Raspberry Pi OS image needed in practice.

### iOS

The `metal` feature builds for iOS, following Slint's
[iOS guide](https://docs.slint.dev/latest/docs/slint/guide/platforms/mobile/ios/). The
simulator needs nothing beyond the target:

```bash
cargo build --release --features metal --target=aarch64-apple-ios-sim
```

The device triple needs the native library built from source, because
`maplibre-native-ffi` publishes a prebuilt artifact for `ios-simulator-arm64` and none for
`aarch64-apple-ios`.

What gets built is **maplibre-native-ffi**, not MapLibre Native. MapLibre Native arrives as
that repository's `third_party/maplibre-native` submodule, with five patches from
`patches/maplibre-native/` applied on top, and `MAPLIBRE_NATIVE_C_INSTALL_DIR` points at
the prefix holding the FFI project's own C API — `include/maplibre_native_c.h` and
`lib/libmaplibre-native-c.a`. A standalone MapLibre Native checkout has neither.

Clone it beside this one and check out the commit `Cargo.lock` resolves
`maplibre-native-ffi` to. The build script feeds the prefix's headers straight to bindgen
when the variable is set, with no version check of its own, so a prefix built from a
different commit is how the C API and the Rust crate drift apart:

```bash
git -C ../maplibre-native-ffi checkout --detach <the rev from Cargo.lock>
# Checks out the pinned submodule and applies the patches. The submodule is marked
# `update = none` so that Cargo skips it, so plain `git submodule update` will not do.
bash ../maplibre-native-ffi/.mise/bin/sync-submodules
cmake --workflow --preset ios-arm64-metal   # run from ../maplibre-native-ffi
```

That needs CMake and Ninja, takes a few minutes, and installs into
`build/ios-arm64-metal/install`. Then:

```bash
MAPLIBRE_NATIVE_C_INSTALL_DIR=$PWD/../maplibre-native-ffi/build/ios-arm64-metal/install \
  cargo build --release --features metal --target=aarch64-apple-ios
```

Set that variable on the command itself and nowhere else. The build script reads it for
whatever target is being built, so exporting it in a shell profile silently points a macOS
or simulator build at the iOS device archive.

The deployment target comes from `.cargo/config.toml`. Rust's default for the triple
predates `___chkstk_darwin`'s arrival in libSystem, which the native archive calls, so
without it the link fails on an undefined symbol.

`project.yml` and `build_for_ios_with_cargo.bash` drive the app bundle, following the same
guide. `xcodegen generate` writes the Xcode project and the `Info.plist`; the script picks
the triple from what Xcode is building and supplies the device prefix above, defaulting to
the sibling checkout and taking `MAPLIBRE_NATIVE_C_INSTALL_DIR` as an override.

To put it on a device, with the device connected and `DEVELOPMENT_TEAM` in `project.yml`
set to yours:

```bash
xcodegen generate
xcrun devicectl list devices            # take the identifier of the one you want
xcodebuild -project "OpenStreetMap Sound Demo.xcodeproj" -scheme OSMSoundDemo \
  -configuration Release -destination 'id=<device>' \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration \
  -derivedDataPath build/ios build
xcrun devicectl device install app --device <device> \
  build/ios/Build/Products/Release-iphoneos/OSMSoundDemo.app
xcrun devicectl device process launch --device <device> org.smellman.OSMSoundDemo
```

Both provisioning flags matter: the first lets Xcode create the profile, and without the
second a device that is not already in the developer account is refused rather than
registered. On a free personal team the installed app stops launching after seven days,
and building again is what renews it.

Touch drives the map, and the keyboard bindings need a hardware keyboard. Links open in
Safari through `UIApplication` rather than `open`, and VJ mode works because
`NSMicrophoneUsageDescription` is in the generated `Info.plist` — without it iOS kills the
app the moment it starts listening.

Gamepads work through the fork `Cargo.toml` points at. Released gilrs has no iOS backend —
it falls through to a stub, so `Gilrs::new` reports `NotImplemented` and no pad is ever
seen — and its macOS backend cannot cover iOS because that one reads IOKit HID, which iOS
does not expose. The fork adds a backend on GameController.framework instead, pending
[the merge request](https://gitlab.com/smellman/gilrs/-/merge_requests).

### Environment

| Variable | Effect |
| --- | --- |
| `MAPLIBRE_STYLE_URL` | Override the initial style URL |
| `MAPLIBRE_FLY_MS` | Fly-to duration in ms (default: 1.5–6 s, scaled by distance) |
| `OSM_SOUND_DEMO_WINDOWED` | Set to open in a window rather than full screen |
| `OSM_SOUND_DEMO_HOME` | `lat,lon` for Locate Me, answering without the network |
| `OSM_SOUND_DEMO_INPUT` | VJ mode's input device, matched on a substring of its name |
| `OSM_SOUND_DEMO_PREFETCH` | Override MapLibre Native's `prefetch_zoom_delta`; `0` turns prefetching off. Unset by default, and measuring says leave it that way — see [Tile prefetching](#tile-prefetching) |
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
| Locate Me | Fly to where this machine appears to be |
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
| Y | Switch how the skyline is coloured — see [the effects](#the-effects) |
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
  2048-point FFT over it and folds the result into 16 logarithmic bands, dB-scaled over
  −90..−10 dB like the web demo's `AnalyserNode`. See [The bands](#the-bands) for why they
  are logarithmic and why the window is that size.
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
Rust binding. Its backend features are mutually exclusive — the crate ships a separate
prebuilt native artifact for each — so one has to be named at build time:

| Feature | Platform | Device created by |
| --- | --- | --- |
| `opengl` | Linux | EGL on Mesa's surfaceless platform: an ES 3 context on the render thread, which the session joins as a share group |
| `vulkan` | Linux | `ash`, headless: instance, physical device with a graphics queue, and a one-queue logical device — no surface and no swapchain |
| `metal` | macOS | `MTLCreateSystemDefaultDevice` |

```bash
cargo run --release --features opengl
```

There is no default, because defaulting to one would silently break the platforms it does
not suit; naming none, or naming two, stops the build with a message rather than a linker
error.

**On this hardware OpenGL is several times faster than Vulkan.** Measured at 1920x1200 in
release on an AMD RENOIR integrated GPU, with `report_playing_frame_rate`:

| Backend | still | camera only | camera + 16 bands |
| --- | --- | --- | --- |
| `opengl` | 19.8 fps | 27.3 fps | **25.1 fps** |
| `vulkan` | 6.5 fps | 6.5 fps | 5.8 fps |

The whole app agrees: about 20 fps against Vulkan's 5.9, both read with
`OSM_SOUND_DEMO_FPS=1`. Whether that gap is this GPU's Vulkan driver or something the
binding does on the Vulkan path has not been chased down; the numbers are simply what
this machine does, and worth re-measuring on other hardware before reading anything
general into them.

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


### Tile prefetching

`prefetch_zoom_delta` is how many zoom levels above the current one MapLibre Native may
pull a coarse parent tile from, so there is something to draw over ground whose own tile
has not arrived. It only fires while the map is moving, and only in `Continuous` mode.

The app does not set it. That is a measured decision rather than an oversight: the default
of 4 beat both alternatives. `report_prefetch_effect` jumps between six cities with a
cleared cache and reads how much of the frame is not flat background after 1.2 s at each,
which is what prefetching is supposed to improve:

| `OSM_SOUND_DEMO_PREFETCH` | filled | fps |
| --- | --- | --- |
| 0 (off) | 42.8%, 34.9%, 50.4% | 320, 374, 282 |
| unset (MapLibre Native's 4) | 51.2%, 52.6%, 52.6% | 283, 239, 248 |
| 8 | 21.5% | 1199 |

Three runs each at 1280x800 on Metal. The default is not only better but steady, where
turning prefetching off swings between 35% and 50% depending on which tiles happen to
arrive first. The higher frame rate at `0` and `8` is not a win — it is the map drawing
less, and at `8` drawing almost nothing: asking for zoom-8 tiles under a zoom-16 camera
floods the connection and starves the tiles actually being looked at.

`set_tile_options` also carries the LOD controls (`lod_min_radius`, `lod_scale`,
`lod_pitch_threshold`, `lod_zoom_shift`, `lod_mode`). Those go untested here; at a pitch of
85° the horizon covers a lot of distant tiles, so they are the next thing to measure if
this ever needs more frame rate.


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
- **Locate Me asks a service, not the OS.** The web demo asked the browser, which asks the
  OS; CoreLocation on macOS would mean shipping an app bundle with a usage description. So
  the button looks the machine up by its public address instead — **pressing it sends that
  address to `ipinfo.io`**, and the answer is accurate to about a city. Setting
  `OSM_SOUND_DEMO_HOME=lat,lon` answers from that instead and never touches the network,
  which is what to use at a venue or offline.
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

**Y — the colouring.** Not an effect but a switch between two of them.

*Lit*, the default, is the web demo's: one light over the whole scene, its colour and
intensity following the mean band level, over flat grey buildings.

*By height* gives each of the sixteen bands a colour of its own, so the skyline reads as a
gradient from the low buildings up to the towers. The hues are spread evenly around the
wheel from a starting point that differs every time — evenly rather than sixteen
independent draws, because independent draws clump and two neighbouring bands landing on
the same colour is exactly what this is meant to tell apart.

It also swaps the music's light for a steady white one at intensity 0.5, and that is not
cosmetic. With no light at all a scene is lit flatly from every direction: two buildings
side by side in one height band come out as a single solid block and the skyline loses its
shape. A light puts a different value on each face, which is what a boundary is. White,
because the light's colour multiplies into the buildings' — measured over the palette at
480x360:

| light | distinct shades | frame still coloured |
| --- | --- | --- |
| none | 281 | 84% |
| white, intensity 0.3 | 560 | 84% |
| white, intensity 0.5 | 682 | 84% |
| white, intensity 0.7 | 742 | 81% |
| white, intensity 1.0 | 631 | 41% |

0.5 is where the shading has arrived and the colour has not started to wash out.

Switching costs sixteen `fill-extrusion-color` sets and no layer churn, and it is done by
hand rather than animated, so it does not touch the per-frame path. The hue goes on
accumulating while painted, so going back to lit resumes the light's animation instead of
jumping to a new phase.

`a_palette_colours_the_buildings` holds both ends of that: the style underneath is toner
and the resting buildings are grey, so the map is monochrome until something colours it.
Coloured subpixels go from 0.0% to 84.0% when the palette goes on and back down when it
comes off, and the frame carries 682 distinct shades rather than one per band.

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

### The bands

The sixteen bands are spread by equal *ratios* — 30 Hz up to Nyquist, each band about
1.5× the one below — and not by equal widths, which is what this did at first and what the
web demo did before it.

Equal widths put every frequency a listener would call bass into a single band and gave
the top three quarters of the skyline to 5 kHz and above, where music has little to say.
Measured over four Otherman tracks, ten seconds each, taking the standard deviation of
every band's level over time — movement, not loudness, because a band with a high mean and
no spread is a tall building standing still:

| track | equal widths | equal ratios | |
| --- | --- | --- | --- |
| Ca5 — cyberSP | 0.216 | 0.280 | +30% |
| NTDSK — あの娘の誕生日 | 0.125 | 0.199 | +59% |
| miii — live@20080406 | 0.048 | 0.116 | +140% |
| iserobin — live@netlabelwarfare | 0.045 | 0.099 | +118% |

The quiet, live recordings gain most: equal widths left them barely moving at all. Under
the old split a 100 Hz, a 440 Hz and a 1 kHz tone all landed in band 0; now they land in
bands 3, 6 and 8, which `the_bands_are_logarithmic` holds in place.

The window went from 1024 points to 2048 to pay for it. At 44.1 kHz that is a bin every
21.5 Hz rather than every 43, and 43 was too coarse to keep the lowest bands apart — two
of them fell on the same bin and read the same level for ever. Even at 2048 the bottom two
bands are one bin wide each, so a pure bass tone spreads over its neighbours; that is the
resolution talking, and `a_low_tone_stays_in_the_low_bands` pins down what it does. The
FFT costs 8.6 µs a frame, against a 16.7 ms budget at 60 fps.


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

The hold is gone with it. What remains is the freeze during a fly-to itself, which is not
a workaround but what the web demo does: its `draw` returns early while a `flyTo` is in
flight and resumes on `moveend`.


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
