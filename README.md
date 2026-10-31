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
and rustup will fetch it on first build; the file says why it once had to match the FFI's
own toolchain and no longer does.

To read the map's tiles from disk rather than from tile.openstreetmap.jp, fetch the
archives first:

```bash
./download-pmtiles.sh                    # planet, takeshima and hoppo into pmtiles/
./download-pmtiles.sh takeshima hoppo    # just these
```

`planet.pmtiles` is about 85 GB. An interrupted download resumes, and an archive that
matches the server's size and date is skipped, so running it again is how to pick up a
newer planet. Without the archives the map reads them from the server, as before.

### Prerequisites

`maplibre-native-ffi` downloads a prebuilt native artifact rather than building MapLibre
Native from source, so there is no C++ toolchain to set up and a clean build takes about a
minute. What is still needed is the system libraries the Rust crates link against. On
Debian and Raspberry Pi OS:

```bash
sudo apt install libfontconfig-dev libasound2-dev libudev-dev libssl-dev
```

| Package | Wanted by |
| --- | --- |
| `libfontconfig-dev` | Slint, to find the fonts it draws the UI text with |
| `libasound2-dev` | ALSA, which `rodio` plays through |
| `libudev-dev` | `gilrs`, to enumerate gamepads |
| `libssl-dev` | OpenSSL, pulled in through `ureq`'s `native-tls` |

That is the list a Raspberry Pi OS image needed in practice. It used to include `clang` and
`libclang-dev` for `bindgen`, which the FFI crate ran at build time; the crates.io release
ships its bindings pre-generated, so nothing on a Linux build runs `bindgen` any more.

### iOS

The `metal` feature builds for iOS, following Slint's
[iOS guide](https://docs.slint.dev/latest/docs/slint/guide/platforms/mobile/ios/). The
simulator needs nothing beyond the target:

```bash
cargo build --release --features metal --target=aarch64-apple-ios-sim
```

The device builds the same way, with `--target=aarch64-apple-ios`: `maplibre-native-ffi`
publishes a prebuilt artifact for both triples, and the crate pins it by version and
checksum.

To link a native library built from source instead — to try a patch to MapLibre Native,
say — check out the [maplibre-native-ffi](https://github.com/maplibre/maplibre-native-ffi)
tag matching the version in `Cargo.toml` (`v0.202610.3` at the time of writing), build its
`ios-arm64-metal` preset, and name the install prefix in `MAPLIBRE_NATIVE_FFI_LOCAL_BUILD`
(an absolute path). The crate's build script checks that the prefix was built for the
target and backend being built. A prefix from a different tag is how the C API and the
Rust crate drift apart.

The deployment target comes from `.cargo/config.toml`. Rust's default for the triple
predates `___chkstk_darwin`'s arrival in libSystem, which the native archive calls, so
without it the link fails on an undefined symbol.

`project.yml` and `build_for_ios_with_cargo.bash` drive the app bundle, following the same
guide. `xcodegen generate` writes the Xcode project and the `Info.plist`; the script picks
the triple from what Xcode is building.

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

### Android

One script does the whole thing:

```bash
./build_for_android.sh                 # arm64-v8a, what a phone runs
ABI=x86_64 ./build_for_android.sh      # what an emulator on an Intel host wants
adb install -r target/release/apk/OSMSoundDemo.apk
```

It needs the Android SDK and NDK, plus `cargo-apk`; it checks for each and says what is missing rather than failing deep inside a build. Set
`ANDROID_HOME` if the SDK is not at `~/Library/Android/sdk` — note that is the directory
holding `platform-tools` and `ndk`, one below the `~/Library/Android` Android Studio shows.

Two things the script settles that are easy to trip over:

**The native library is downloaded**, as everywhere else. It used to be built from source,
because the published `android-arm64` artifact was a floating snapshot of upstream's latest
commit while the binding came from the commit `Cargo.lock` pinned, and the two drifted
apart — the snapshot once renamed `MLN_LOG_EVENT_OPENGL` to `MLN_LOG_EVENT_GRAPHICS_BACKEND`
under a binding that still asked for the old name. The binding now comes from crates.io and
pins its artifact by version and checksum, so that cannot happen.

**bindgen has to be told the triple separately.** `cargo-apk` sets a bare `clang` as the C
compiler and passes the API version in `CFLAGS`, which bindgen does not read; the NDK's
`sys/cdefs.h` then refuses the unversioned target outright. `BINDGEN_EXTRA_CLANG_ARGS_*`
carries it.

`min_sdk_version` is 26 because cpal plays through AAudio and `libaaudio.so` first appears
in that sysroot. The APK is signed with a development key the script generates into
`build/`; it is not a release signature and `build/` is ignored by git.

The app is a `cdylib` here rather than a binary — an android-activity app has no `main`, the
platform loads a shared library and calls `android_main` — which is why `src/lib.rs` holds
everything and `src/main.rs` is a thin desktop wrapper.

Two things differ from the other platforms at runtime. **MapLibre Native's own requests are
answered by this app** (`src/map/fetch.rs`), because Android has no certificate store a C++
library can read: verification goes out to Java through a class `cargo-apk` cannot compile,
and every HTTPS request fails with "failed to call native verifier". Serving the requests
through the `ureq` this app already uses avoids both that and a move to Gradle. **The system
bar insets are measured here too**, from the activity's content rect, because Slint reads
them through a Java helper that is not in the APK for the same reason.

Gamepads do not work: `gilrs` has no Android backend, as it had none for iOS.

### Environment

| Variable | Effect |
| --- | --- |
| `MAPLIBRE_STYLE_URL` | Load this style URL instead of the bundled `style/style.json` |
| `MAPLIBRE_FLY_MS` | Fly-to duration in ms (default: 1.5–6 s, scaled by distance) |
| `OSM_SOUND_DEMO_WINDOWED` | Set to open in a window rather than full screen |
| `OSM_SOUND_DEMO_HOME` | `lon,lat` for Locate Me, answering without the network. Longitude first, as MapLibre and GeoJSON order a position |
| `OSM_SOUND_DEMO_INPUT` | VJ mode's input device, matched on a substring of its name |
| `OSM_SOUND_DEMO_MUSIC` | The directory Local Music reads, instead of `~/Music`. On macOS Local Music is off unless this is set |
| `OSM_SOUND_DEMO_TILE_LOD` | `default` puts MapLibre Native's own tile LOD back in place of the distance-based one the app sets, to A/B what it buys — see [Frame rate](#frame-rate) |
| `OSM_SOUND_DEMO_PREFETCH` | Override MapLibre Native's `prefetch_zoom_delta`; `0` turns prefetching off. Unset by default, and measuring says leave it that way — see [Tile prefetching](#tile-prefetching) |
| `OSM_SOUND_DEMO_FPS` | Print `shown` and `rendered` frame rates to stderr every second. A gap between them means frames are being dropped at the channel; no gap means the render thread is the limit |
| `OSM_SOUND_DEMO_RENDERER_TESTS` | Run the opt-in renderer tests, which need a GPU and the network |
| `OSM_SOUND_DEMO_RENDER_SIZE` | Size the renderer probes measure at, `<width>x<height>` (default 960x640) |
| `OSM_SOUND_DEMO_RENDER_SCALE` | Render the map at this fraction of its on-screen size and let Slint scale it up (default 1.0, floor 0.25). The single biggest thing you can trade for frame rate — see [Frame rate](#frame-rate) |

## Controls

The window opens **full screen**: this is something to stand in front of and drive from a
gamepad, not a window to keep alongside other work. A full-screen window has no title bar,
so **Escape** leaves full screen, **F** toggles it, and **Q** quits.

The four face-button effects are on the keyboard as well, as a diamond under the right
hand that matches the one on the pad rather than the buttons' letters:

```
      I          Y — switch the colouring
    J   L        X — switch how finely the skyline is cut  /  B — the orbit
      K          A — the drop
```

The shape is what carries over, so a hand that knows the pad knows these. Both routes go
through one mapping — `the_keyboard_and_the_pad_agree_on_the_face_buttons` checks the keys
against the pad's own bindings rather than against a second copy of the answer.

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
| I / J / K / L | The pad's Y / X / A / B, laid out as they sit on the pad |
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
| X | Switch how finely the skyline is cut, 16 slices or 64 |
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
- `src/map/renderer.rs` — the map, and the only file that touches MapLibre Native. Sixty-four
  `fill-extrusion` layers split buildings into height slices; how many are shown is what X
  switches, and each shown layer drives its own extrusion height and colour.
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

### Window renderer

On macOS the window is drawn with Slint's Skia renderer rather than its default, FemtoVG.
FemtoVG draws through OpenGL there, which holds a 120 Hz display to 60: on an M2 Pro's
built-in display, full screen and playing, FemtoVG showed 61–65 fps while the render
thread finished 74–75, and Skia showed 72–76 of 72–77. Other platforms keep Slint's
default. `SLINT_BACKEND=winit-femtovg` puts FemtoVG back to compare.

### Rendering backend

The map runs on [maplibre-native-ffi](https://github.com/maplibre/maplibre-native-ffi)'s
Rust binding. Its backend features are mutually exclusive — the crate ships a separate
prebuilt native artifact for each — so one has to be named at build time:

| Feature | Platform | Device created by |
| --- | --- | --- |
| `opengl` | Linux, Android | EGL: an ES 3 context on the render thread, which the session joins as a share group. Linux asks for Mesa's surfaceless platform, which Android has no equivalent of — there the default display is used, since nothing binds a window surface to it either way |
| `vulkan` | Linux | `ash`, headless: instance, physical device with a graphics queue, and a one-queue logical device — no surface and no swapchain |
| `metal` | macOS, iOS | `MTLCreateSystemDefaultDevice` |

```bash
cargo run --release --features opengl
```

There is no default, because defaulting to one would silently break the platforms it does
not suit; naming none, or naming two, stops the build with a message rather than a linker
error.

**On AC power Vulkan is well ahead of OpenGL.** Measured at 1920x1200 in release on an AMD
RENOIR integrated GPU with `report_playing_frame_rate`, on AC with the `performance`
platform profile, taking turns between the backends for three runs each:

| Backend | still | camera only | camera + 16 bands |
| --- | --- | --- | --- |
| `opengl` | 33.9 fps | 35.8 fps | 28.9 fps |
| `vulkan` | 64.9 fps | 63.6 fps | **48.4 fps** |

Background load moves these too: with another application busy on one core, the same
build gave Vulkan 44.9 fps for camera + 16 bands and OpenGL 29.0, so measure on an
otherwise idle machine and read the rows against each other rather than the cells
exactly.

Most of Vulkan's lead comes from the order in which the render thread queues its work.
The session runs driver work in the order it arrives, and the thread used to arm the
next frame demand before asking for the readback of the frame it had just been handed.
Whenever an update was already waiting, that demand drew a whole frame first, about
10 ms, while the readback itself takes about 3.5 ms. The demand is now armed only after
the readback is queued, so the next frame draws while this one is copied out for Slint.
That took Vulkan from 32 to about 53 fps for camera + 16 bands when it went in. OpenGL
gains nothing from it, because its driver work runs on the render thread anyway.

Earlier measurements had Vulkan far behind: 17.8, 25.3 and 23.0 fps against OpenGL's
36.2, 35.3 and 30.3, and before the switch to distance-based tile LOD (see
[Frame rate](#frame-rate)) 5.8 fps against 25.1 for camera + 16 bands. Their power state
was not recorded, and that gap did not come back on AC. In the slow state, sampling the
stacks put almost half of MapLibre Native's render thread in the `memcmp` that
`mln::vulkan::BufferResource::update` runs against a buffer mapped from uncached memory;
on AC that call never showed up. So write down whether the machine is on battery, and its
platform profile (`/sys/firmware/acpi/platform_profile`), next to any numbers, and compare
the backends within one session rather than against this table.

Unlike the older `maplibre_native` crate, this one hands the graphics plumbing to the
caller: there is no headless renderer that makes its own device. `src/map/renderer.rs`
creates the device, attaches an *owned texture* render target at the map's size, and reads
the frame back as premultiplied RGBA8 with `RenderSessionHandle::read_texture` for Slint.
The binding downloads a prebuilt native artifact, so a clean build takes well under a
minute rather than compiling MapLibre Native from source.

The device is created once and leaked on purpose: a render target borrows those handles,
and a map outlives any one session. The handles are held as plain addresses, since raw
pointers are not `Send` and the Vulkan ones live in a static.

The map runs in `MapMode::Continuous` on the runtime's own worker threads. Commands go in
without waiting, and what comes back arrives on callbacks: the runtime's event handler
says when the style has loaded, which is when the band layers can be added, and a camera
transition's end handler says when a flight has landed. The render thread keeps one frame
demand armed with `FrameDemandFlag::WAIT_FOR_UPDATE`, so every update the map publishes —
tiles arriving, a flight stepping — is drawn without being asked for; `needs_repaint` on a
frame result, set during a paint transition, is answered with a repaint. On OpenGL the
session shares the render thread's EGL context and its driver work is serviced there; on
Vulkan and Metal it drives itself.


### Frame rate

What costs this demo its frame rate is measurable with `report_playing_frame_rate` (see
the environment table above for how to run it). Numbers are release builds on an AMD
RENOIR integrated GPU.

Almost all of a frame is MapLibre Native drawing on the GPU. Sending the camera and the
band updates costs microseconds, and the readback and copy for Slint about 6 ms at
960x640 — what a background-only style costs end to end.

**The tile LOD.** By default MapLibre Native draws every visible tile at the camera's
zoom. At a pitch of 70° that fills the ground with z16 tiles right up to the horizon,
where they are too small to read and show as noise; on OpenGL at 960x640 a frame took
31 ms at 70° against 10 ms at 45°. `TileLodMode::Distance` draws coarser tiles further
away, and the app sets it. The foreground is identical either way — only the horizon
changes, and reads better for it. `OSM_SOUND_DEMO_TILE_LOD=default` puts the old mode
back for comparison. Camera + 16 bands:

| Render size | `opengl` default | `opengl` distance | `vulkan` default | `vulkan` distance |
| --- | --- | --- | --- | --- |
| 1920x1200 | 15.5 fps | **30.3 fps** | 14.9 fps | **23.0 fps** |
| 1440x900 | 21.9 fps | **42.4 fps** | 12.9 fps | **29.1 fps** |
| 1280x800 | 23.9 fps | **56.4 fps** | 13.9 fps | **30.2 fps** |
| 960x600 | 20.0 fps | **48.6 fps** | 15.8 fps | **40.4 fps** |

One run each, and runs vary — 960x600 coming out below 1280x800 on OpenGL is that noise,
and a Vulkan run at 1920x1200 once gave 32 fps where two repeats gave 23 — so read the
columns against each other rather than the cells exactly. The machine's power state was
not recorded for these runs either (see [Rendering backend](#rendering-backend)), so the
Vulkan columns in particular describe one state of the machine, not Vulkan in general.

**The pixels.** Everything the map does scales with the area it covers, so after the LOD
the render size is the next thing to trade. `OSM_SOUND_DEMO_RENDER_SCALE` buys frame rate
at the cost of a soft, upscaled map.

**The buildings.** With no band layers at all a frame at 960x640 on OpenGL took 17 ms
(default LOD); the first fill-extrusion layer adds about 10 ms and each further visible one
about 0.3 ms. Drawing them at opacity 1.0 rather than 0.6 saved another 7 ms, since a
translucent extrusion is drawn in two passes, but the see-through look is the web demo's
and is kept.

`OSM_SOUND_DEMO_RENDER_SCALE` is 1.0 by default: a Mac on Metal does not need it, and
nobody should have the map go blurry without asking.

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

`set_tile_options` also carries the LOD controls. `lod_mode` is set to `Distance`, which
is worth two to three times the frame rate at this pitch — see [Frame rate](#frame-rate).
The finer knobs (`lod_min_radius`, `lod_scale`, `lod_pitch_threshold`, `lod_zoom_shift`)
are left at MapLibre Native's defaults and untested.


### Differences from the web demo

Some of these are deliberate, some are limits of the current Rust bindings.

- **Rendering runs on its own thread.** Reading a frame back blocks until it lands, and on
  OpenGL the render session's driver work has to run on the thread whose EGL context it
  shares; neither belongs in a Slint callback. The UI thread only posts camera and band
  updates and picks up finished frames.
- **Fly-to is MapLibre Native's own.** `Fly To` hands the camera over with
  a `CameraUpdateMode::Fly` camera update and follows along until the transition's end
  handler fires; MapLibre picks
  the duration from the distance, as the web demo's `flyTo` did, unless `MAPLIBRE_FLY_MS`
  says otherwise. Anything that moves the camera — a drag, the sticks, an effect — cancels
  the flight. As in the web demo, the building animation pauses during a fly.
- **No "hash"**: the web demo kept the camera in the URL, which a native binary has no use
  for.
- **Pitch opens at 70°**, as the web demo did, and goes to 85°. MapLibre Native clamps at
  60° unless `BoundOptions::max_pitch` is raised first — asking for more without that
  silently gives 60 back.
- **Vector tiles come from `planet.pmtiles`**, as in the web demo. The style is
  tile.openstreetmap.jp's toner-ja, bundled as `style/style.json` with its three sources —
  `openmaptiles`, `takeshima` and `hoppo` — pointed at `pmtiles://asset://pmtiles/*.pmtiles`.
  MapLibre Native reads PMTiles itself, so there is no protocol to register as the web
  demo had to. `asset://` is the way to write a relative path here: the style is handed over
  as JSON, so nothing resolves a URL against its location, and `file://` takes what follows
  it as a path from the working directory. `asset://` resolves against the runtime's asset
  path, which the desktop build sets to this project. An archive that is not there — not
  downloaded yet, or on Android and iOS, where the asset path is not this project — is read
  from `https://tile.openstreetmap.jp/static/` instead; the check runs on every style load.
- **Glyphs and the sprite are compiled in.** `style/fonts` (`migu1c-regular` and
  `migu2m-regular`) and `style/sprites` are copied from
  [osmfj/tileserver-gl-site](https://github.com/osmfj/tileserver-gl-site), which is what
  tile.openstreetmap.jp serves them from. The style still names that server, and
  `src/map/bundled.rs` answers those URLs from the copies before they reach the network, so
  anything not bundled — or a style from `MAPLIBRE_STYLE_URL` — is fetched as before. The
  city and primary-road labels asked for Nunito and Noto Sans; the repository has no Nunito
  (the server substitutes another font) and Noto Sans is about 35 MB a weight, so both use
  `migu1c-regular` here. Migu is under the IPA Font License Agreement v1.0; the license
  texts are in `style/fonts/licenses`, described in `style/fonts/LICENSE.md`, and have to go
  with any build that is handed out.
- **Locate Me asks a service, not the OS.** The web demo asked the browser, which asks the
  OS; CoreLocation on macOS would mean shipping an app bundle with a usage description. So
  the button looks the machine up by its public address instead — **pressing it sends that
  address to `ipinfo.io`**, and the answer is accurate to about a city. Setting
  `OSM_SOUND_DEMO_HOME=lon,lat` answers from that instead and never touches the network,
  which is what to use at a venue or offline. Longitude first, as MapLibre and GeoJSON
  order a position — `139.7672,35.6807` is Tokyo, not the other way round. A pair is
  refused when its second number cannot be a latitude, which catches the Google Maps order
  for most of the world but not all of it: `-1.2798,36.8166` is Nairobi written backwards
  and every number in it is in range, so it is accepted and lands in open water off Spain.
  Check the status line after pressing the button.
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

### Local Music

The **Local Music** button switches from Otherman's catalogue to the albums under
`~/Music` (or `OSM_SOUND_DEMO_MUSIC`): the release dropdown lists albums, the dropdown
beside it the album's tracks, and Go To Release becomes Open Folder. Picking a track plays
it; the transport, auto-advance and the D-pad work as they do for releases. Switching back
stops what is playing, and each side keeps the release or album it had.

The directory is scanned in the background at startup, and the button only appears once it
has found something to play. `src/library.rs` reads FLAC, MP3 and AAC tags with
[lofty](https://github.com/Serial-ATA/lofty-rs) — headers only, which put 1215 FLAC files
into 98 albums in about 30 ms here — and groups tracks by album artist (or artist) and
album, falling back to the directory for untagged files. Tracks sort by disc, track number
and file name.

Playback is the same rodio player and tap as a streamed track, reading the file directly,
so the skyline follows a local track exactly as it follows a stream. That is also why this
does not go through mpd, which was the other option: mpd plays to its own output, past the
analyser, and getting its samples back means configuring a FIFO output (or running VJ mode
off a monitor input) on top of setting up mpd and its database.

It is for Linux. The phones have no music directory to list, and macOS's `~/Music` is
Music.app's library — its own database and layout — so there it stays off unless
`OSM_SOUND_DEMO_MUSIC` names a plain directory of FLAC, MP3 or AAC files. AAC is read from
`.m4a` or `.aac` files, as AAC-LC: Bandcamp's AAC downloads are MP4 files under either
name. ALAC is not decoded.

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

**X — how finely the skyline is cut.** Sixteen height slices or sixty-four. The style
carries all sixty-four layers from the moment it loads and the coarse setting hides all but
every fourth, widening those to cover the gap. Hiding rather than removing is the point:
changing the layer set is what makes MapLibre Native re-run tile layout for the building
source, which is what once left the map blank while a track played.

The sixteen frequency bands are spread over however many slices are shown by
interpolation, not by repeating each band four times — repeating gives four identical
slices in a row and the skyline comes out as steps, which is what a finer cut is supposed
to remove.

Sixty-four costs about 11% of the frame rate, not the two thirds a first guess suggested:
the slices do not overlap, so the same buildings are drawn either way and what is paid is
per-layer overhead rather than drawing anything twice.

| | 16 slices | 64 slices |
| --- | --- | --- |
| 1280x800 | 89.8, 89.6 fps | 79.7, 80.8 fps |
| 1920x1200 | 71.0, 73.2 fps | 61.6, 66.5 fps |

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

The edges are in hertz, so they are worked out for the rate of what is playing: a bin is
`rate / 2048` Hz. They used to assume 44.1 kHz for everything, which is what Otherman's
MP3s are, but Local Music's FLACs are as often 48 kHz — every band about 9% high — and
occasionally 96 kHz, an octave high. The range stays 30 Hz to 22.05 kHz at any rate. A file
at 88.2 kHz or above is also averaged down by a whole factor before the analyser sees it,
2 for 96 kHz and 4 for 192, because at 96 kHz a 2048-point window would be 47 Hz a bin and
the bottom bands would run together; only the analyser's copy is averaged, not what plays.
`a_tone_lands_in_the_same_band_whatever_the_rate` checks a tone from 50 Hz to 12 kHz lands
in the same band at 44.1, 48, 88.2, 96 and 192 kHz.


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
