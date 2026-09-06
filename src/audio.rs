//! Audio playback (rodio) plus the spectrum analysis that drives the map.
//!
//! The web demo fed a `MediaElementAudioSourceNode` into an `AnalyserNode` with
//! 16 frequency bins. Here the same shape is rebuilt by tapping the decoded
//! sample stream on its way to the device and running an FFT over the most
//! recent window on the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::decoder::DecoderBuilder;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Sample, Source};

use crate::stream::StreamingRead;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// iOS makes no sound until the app has set up an audio session, and cpal does
/// not do it: its iOS backend touches `AVAudioSession` only to ask for a buffer
/// duration, never to choose a category or activate the session. A session
/// starts out inactive and in the `SoloAmbient` category, so the RemoteIO unit
/// runs and plays to nothing — the transport looks like it is playing and
/// nothing comes out of the speaker.
///
/// `Playback` rather than `SoloAmbient` also means the ring switch does not
/// silence it, which is what a demo playing music wants.
#[cfg(target_os = "ios")]
mod session {
    use objc2_avf_audio::{
        AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord,
        AVAudioSessionCategoryPlayback,
    };

    pub fn playback() {
        // SAFETY: the shared session is a framework singleton, and the category
        // is one of the framework's own constants.
        unsafe {
            let session = AVAudioSession::sharedInstance();
            let Some(category) = AVAudioSessionCategoryPlayback else {
                return report("AVAudioSessionCategoryPlayback is unavailable");
            };
            if let Err(error) = session.setCategory_error(category) {
                return report(&format!("setting the audio session category: {error}"));
            }
            activate(&session);
        }
    }

    /// VJ mode listens as well as plays, which is a different category.
    /// `DefaultToSpeaker` because without it `PlayAndRecord` routes playback to
    /// the receiver rather than the speaker.
    pub fn play_and_record() {
        // SAFETY: as above.
        unsafe {
            let session = AVAudioSession::sharedInstance();
            let Some(category) = AVAudioSessionCategoryPlayAndRecord else {
                return report("AVAudioSessionCategoryPlayAndRecord is unavailable");
            };
            if let Err(error) = session.setCategory_withOptions_error(
                category,
                AVAudioSessionCategoryOptions::DefaultToSpeaker,
            ) {
                return report(&format!("setting the audio session category: {error}"));
            }
            activate(&session);
        }
    }

    unsafe fn activate(session: &AVAudioSession) {
        // SAFETY: the caller holds the shared session.
        if let Err(error) = unsafe { session.setActive_error(true) } {
            report(&format!("activating the audio session: {error}"));
        }
    }

    /// Reported rather than returned: the session failing is worth saying out
    /// loud, but it is not a reason to refuse to start.
    fn report(what: &str) {
        eprintln!("audio session: {what}; playback will be silent");
    }
}

/// Every other platform plays without being asked.
#[cfg(not(target_os = "ios"))]
mod session {
    pub fn playback() {}
    pub fn play_and_record() {}
}

/// Number of frequency bands, matching the web demo's `fftSize = BINS * 2`
/// analyser and the number of building layers on the map.
pub const BINS: usize = 16;

/// FFT window length. Larger than the web demo's 32-point analyser so the bands
/// are stable enough to look good at map frame rates.
const WINDOW: usize = 1024;

/// Decibel range mapped onto 0.0..=1.0, same as `AnalyserNode`'s defaults.
const MIN_DB: f32 = -90.0;
const MAX_DB: f32 = -10.0;

/// Exponential smoothing applied to each band, weighting the previous value.
const SMOOTHING: f32 = 0.35;

/// Ring buffer of the most recent mono frames, written by the audio thread.
#[derive(Debug)]
pub struct Spectrum {
    ring: Mutex<Ring>,
}

#[derive(Debug)]
struct Ring {
    samples: Box<[f32; WINDOW]>,
    write: usize,
}

impl Spectrum {
    fn new() -> Self {
        Self {
            ring: Mutex::new(Ring {
                samples: Box::new([0.0; WINDOW]),
                write: 0,
            }),
        }
    }

    fn push(&self, sample: f32) {
        let Ok(mut ring) = self.ring.lock() else {
            return;
        };
        let write = ring.write;
        ring.samples[write] = sample;
        ring.write = (write + 1) % WINDOW;
    }

    /// Copies the window out in chronological order (oldest first).
    fn snapshot(&self, out: &mut [f32; WINDOW]) {
        let Ok(ring) = self.ring.lock() else {
            return;
        };
        let (head, tail) = ring.samples.split_at(ring.write);
        out[..tail.len()].copy_from_slice(tail);
        out[tail.len()..].copy_from_slice(head);
    }

    fn clear(&self) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.samples.fill(0.0);
            ring.write = 0;
        }
    }
}

/// Wraps a rodio source, copying every frame into [`Spectrum`] as it is pulled
/// by the mixer. Interleaved channels are averaged down to mono.
struct Tap<S> {
    inner: S,
    spectrum: Arc<Spectrum>,
    frame_sum: f32,
    frame_len: u16,
}

impl<S> Tap<S> {
    fn new(inner: S, spectrum: Arc<Spectrum>) -> Self {
        Self {
            inner,
            spectrum,
            frame_sum: 0.0,
            frame_len: 0,
        }
    }
}

impl<S: Source> Iterator for Tap<S> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let sample = self.inner.next()?;
        let channels = self.inner.channels().get();
        self.frame_sum += sample;
        self.frame_len += 1;
        if self.frame_len >= channels {
            self.spectrum
                .push(self.frame_sum / f32::from(self.frame_len));
            self.frame_sum = 0.0;
            self.frame_len = 0;
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

/// Turns the tapped samples into `BINS` normalized band levels.
pub struct Analyzer {
    spectrum: Arc<Spectrum>,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    levels: [f32; BINS],
}

impl Analyzer {
    fn new(spectrum: Arc<Spectrum>) -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(WINDOW);
        // Hann window, to keep leakage from smearing across the coarse bands.
        let window = (0..WINDOW)
            .map(|i| {
                let phase = std::f32::consts::TAU * i as f32 / (WINDOW as f32 - 1.0);
                0.5 * (1.0 - phase.cos())
            })
            .collect();
        Self {
            spectrum,
            fft,
            window,
            scratch: vec![Complex::new(0.0, 0.0); WINDOW],
            levels: [0.0; BINS],
        }
    }

    /// Recomputes the band levels from the newest window and returns them,
    /// each in `0.0..=1.0`.
    pub fn poll(&mut self) -> [f32; BINS] {
        let mut samples = [0.0f32; WINDOW];
        self.spectrum.snapshot(&mut samples);

        for (slot, (sample, weight)) in self
            .scratch
            .iter_mut()
            .zip(samples.iter().zip(self.window.iter()))
        {
            *slot = Complex::new(sample * weight, 0.0);
        }
        self.fft.process(&mut self.scratch);

        let half = WINDOW / 2;
        let per_band = half / BINS;
        for band in 0..BINS {
            let start = band * per_band;
            let magnitude = self.scratch[start..start + per_band]
                .iter()
                .map(|c| c.norm())
                .sum::<f32>()
                / per_band as f32
                / half as f32;
            let db = 20.0 * magnitude.max(1e-10).log10();
            let level = ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0);
            self.levels[band] = SMOOTHING * self.levels[band] + (1.0 - SMOOTHING) * level;
        }
        self.levels
    }
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Owns the output device and the playback queue.
pub struct AudioPlayer {
    // Playback stops as soon as the device sink is dropped.
    _sink: MixerDeviceSink,
    player: Player,
    spectrum: Arc<Spectrum>,
}

impl AudioPlayer {
    pub fn new() -> Result<(Self, Analyzer), Error> {
        // Before the device is opened, so the output unit is built against the
        // session this app wants rather than the default one.
        session::playback();
        let mut sink = DeviceSinkBuilder::open_default_sink()?;
        // Quitting drops this on purpose; rodio's warning about it is noise.
        sink.log_on_drop(false);
        let player = Player::connect_new(sink.mixer());
        let spectrum = Arc::new(Spectrum::new());
        let analyzer = Analyzer::new(Arc::clone(&spectrum));
        Ok((
            Self {
                _sink: sink,
                player,
                spectrum,
            },
            analyzer,
        ))
    }

    /// Replaces whatever is queued with the given stream and starts it.
    ///
    /// The decoder is told the stream is not seekable even though the reader
    /// can seek: with `is_seekable` set, symphonia seeks to the end to measure
    /// the stream, which on a partially arrived download means blocking until
    /// the whole track is in — exactly what streaming is meant to avoid. The
    /// declared length is still passed on, so duration is known without it.
    pub fn play(&self, stream: StreamingRead) -> Result<(), Error> {
        self.stop();
        let byte_len = stream.byte_len();
        let mut builder = DecoderBuilder::new().with_hint("mp3").with_data(stream);
        if let Some(len) = byte_len {
            builder = builder.with_byte_len(len);
        }
        // `with_byte_len` turns seeking on, so this has to come after it.
        let decoder = builder.with_seekable(false).build()?;
        self.player
            .append(Tap::new(decoder, Arc::clone(&self.spectrum)));
        self.player.play();
        Ok(())
    }

    pub fn stop(&self) {
        self.player.clear();
        self.spectrum.clear();
    }

    pub fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
    }

    /// True once the queued track has played to its end.
    pub fn finished(&self) -> bool {
        self.player.empty()
    }
}

/// VJ mode: the map follows what an input device hears rather than a track.
///
/// The web demo did this with `getUserMedia`, feeding the live stream into the
/// same analyser. Here a thread pulls the input and pushes it into the same
/// [`Spectrum`] the tracks use, so everything downstream — bands, light,
/// effects — is unchanged. Nothing is played back: the sound is already coming
/// out of whatever is being mixed.
///
/// Route the sound into an input first. On macOS that means Loopback.app, on
/// Linux Helvum with PipeWire; a plain microphone works too, and reacts to the
/// room.
pub struct VjMode {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    device: String,
}

impl VjMode {
    /// The input being listened to.
    pub fn device(&self) -> &str {
        &self.device
    }
}

impl Drop for VjMode {
    fn drop(&mut self) {
        // Back to a category that only plays, now that nothing is listening.
        session::playback();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The reader parks on the input's poll interval, so this returns
            // promptly.
            let _ = thread.join();
        }
    }
}

impl AudioPlayer {
    /// Starts listening to an input device.
    ///
    /// `wanted` picks the device by a case-insensitive substring of its name;
    /// without it the system default is used.
    pub fn start_vj(&self, wanted: Option<&str>) -> Result<VjMode, Error> {
        session::play_and_record();
        let builder = rodio::microphone::MicrophoneBuilder::new();
        let (device, name) = match wanted {
            Some(wanted) => {
                let wanted = wanted.to_lowercase();
                let inputs = rodio::microphone::available_inputs()?;
                let found = inputs
                    .into_iter()
                    .find(|input| input.to_string().to_lowercase().contains(&wanted))
                    .ok_or_else(|| format!("no input device matching {wanted:?}"))?;
                let name = found.to_string();
                (builder.device(found)?, name)
            }
            None => {
                let device = builder.default_device()?;
                (device, "default input".to_owned())
            }
        };
        let microphone = device.default_config()?.open_stream()?;

        let stop = Arc::new(AtomicBool::new(false));
        let spectrum = Arc::clone(&self.spectrum);
        let thread = std::thread::Builder::new()
            .name("vj-input".to_owned())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    // The same tap the tracks go through, so channel folding
                    // and the ring buffer behave identically.
                    let mut tap = Tap::new(microphone, spectrum);
                    while !stop.load(Ordering::Relaxed) {
                        if tap.next().is_none() {
                            break;
                        }
                    }
                }
            })?;

        Ok(VjMode {
            stop,
            thread: Some(thread),
            device: name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZero;

    const TEST_RATE: u32 = 44_100;

    /// A finite sine wave, used instead of a real decoder so the analysis chain
    /// can be checked without an audio device or a network fetch.
    struct Tone {
        phase: f32,
        step: f32,
        channels: u16,
        remaining: usize,
    }

    impl Tone {
        fn new(frequency: f32, channels: u16, frames: usize) -> Self {
            Self {
                phase: 0.0,
                step: std::f32::consts::TAU * frequency / TEST_RATE as f32,
                channels,
                remaining: frames * channels as usize,
            }
        }
    }

    impl Iterator for Tone {
        type Item = Sample;

        fn next(&mut self) -> Option<Sample> {
            if self.remaining == 0 {
                return None;
            }
            self.remaining -= 1;
            let value = self.phase.sin();
            // Advance once per frame so both channels carry the same sample.
            if self.remaining.is_multiple_of(self.channels as usize) {
                self.phase += self.step;
            }
            Some(value)
        }
    }

    impl Source for Tone {
        fn current_span_len(&self) -> Option<usize> {
            None
        }

        fn channels(&self) -> rodio::ChannelCount {
            NonZero::new(self.channels).unwrap()
        }

        fn sample_rate(&self) -> rodio::SampleRate {
            NonZero::new(TEST_RATE).unwrap()
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    /// Drains a source through the tap and settles the exponential smoothing.
    fn levels_of(source: Tone) -> [f32; BINS] {
        let spectrum = Arc::new(Spectrum::new());
        let mut tap = Tap::new(source, Arc::clone(&spectrum));
        while tap.next().is_some() {}

        let mut analyzer = Analyzer::new(spectrum);
        let mut levels = [0.0; BINS];
        for _ in 0..64 {
            levels = analyzer.poll();
        }
        levels
    }

    #[test]
    fn silence_leaves_every_band_at_zero() {
        let spectrum = Arc::new(Spectrum::new());
        let mut analyzer = Analyzer::new(spectrum);
        let levels = analyzer.poll();
        assert!(levels.iter().all(|level| *level == 0.0), "{levels:?}");
    }

    #[test]
    fn a_low_tone_only_excites_the_lowest_band() {
        // Each band spans (44100 / 2) / 16 ≈ 1378 Hz, so 440 Hz is band 0.
        let levels = levels_of(Tone::new(440.0, 1, WINDOW * 4));
        assert!(levels[0] > 0.5, "band 0 was {}", levels[0]);
        assert!(
            levels[1..].iter().all(|level| *level < levels[0] / 2.0),
            "{levels:?}"
        );
    }

    #[test]
    fn a_higher_tone_moves_to_a_higher_band() {
        // 5 kHz falls in band 3 (4134 Hz .. 5512 Hz).
        let levels = levels_of(Tone::new(5_000.0, 1, WINDOW * 4));
        let loudest = levels
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(band, _)| band);
        assert_eq!(loudest, Some(3), "{levels:?}");
    }

    #[test]
    fn interleaved_stereo_is_analysed_as_mono() {
        let mono = levels_of(Tone::new(440.0, 1, WINDOW * 4));
        let stereo = levels_of(Tone::new(440.0, 2, WINDOW * 4));
        for (band, (mono, stereo)) in mono.iter().zip(stereo.iter()).enumerate() {
            assert!(
                (mono - stereo).abs() < 0.05,
                "band {band}: mono {mono}, stereo {stereo}"
            );
        }
    }

    /// End-to-end check against the real catalogue: open a track's stream,
    /// decode it and confirm the tap produces band levels. Opt-in, because it
    /// needs the network.
    #[test]
    fn a_real_track_produces_band_levels() {
        if std::env::var_os("OSM_SOUND_DEMO_NETWORK_TESTS").is_none() {
            eprintln!("skipped: set OSM_SOUND_DEMO_NETWORK_TESTS=1 to run");
            return;
        }

        let release = crate::otherman::fetch_release("OTMN001").expect("fetching OTMN001");
        let track = release.tracklist.first().expect("OTMN001 has tracks");
        let url = crate::otherman::absolute_url(&track.url);

        let opened = std::time::Instant::now();
        let stream = crate::otherman::stream(&url).expect("opening the stream");
        let byte_len = stream.byte_len();
        let buffered = stream.buffered();
        eprintln!(
            "stream opened in {:?} with {buffered} of {byte_len:?} bytes buffered",
            opened.elapsed()
        );

        // The point of streaming: playback may begin long before the last byte
        // lands. Only asserted for a track big enough that the whole thing
        // could not plausibly have arrived during the prebuffer wait.
        if let Some(len) = byte_len.filter(|len| *len > 4 * 1024 * 1024) {
            assert!(
                (buffered as u64) < len,
                "waited for the whole {len}-byte track before returning"
            );
        }

        let decoder = DecoderBuilder::new()
            .with_hint("mp3")
            .with_data(stream)
            .with_seekable(false)
            .build()
            .expect("decoding the track");
        let spectrum = Arc::new(Spectrum::new());
        let mut tap = Tap::new(decoder, Arc::clone(&spectrum));
        let mut analyzer = Analyzer::new(Arc::clone(&spectrum));

        // The track has near-silent gaps, so sample the analyser repeatedly and
        // take the loudest reading over the first few seconds.
        let mut loudest = [0.0f32; BINS];
        for chunk in 0..200 {
            for _ in 0..WINDOW {
                assert!(tap.next().is_some(), "the track ended after {chunk} chunks");
            }
            let levels = analyzer.poll();
            for (peak, level) in loudest.iter_mut().zip(levels.iter()) {
                *peak = peak.max(*level);
            }
        }

        assert!(
            loudest.iter().any(|level| *level > 0.4),
            "no audible band: {loudest:?}"
        );
        assert!(
            loudest[0] > 0.0 && loudest[BINS - 1] > 0.0,
            "bands at the edges never moved: {loudest:?}"
        );
    }

    /// Opt-in: opens a real input device, since there is no way to fake one.
    /// Reports what it hears rather than asserting on it — a quiet room is a
    /// valid reading.
    #[test]
    fn vj_mode_opens_an_input() {
        if std::env::var_os("OSM_SOUND_DEMO_AUDIO_TESTS").is_none() {
            eprintln!("skipped: set OSM_SOUND_DEMO_AUDIO_TESTS=1 to run");
            return;
        }
        for input in rodio::microphone::available_inputs().expect("listing inputs") {
            eprintln!("input: {input}");
        }

        let (player, mut analyzer) = AudioPlayer::new().expect("opening the audio device");
        // `OSM_SOUND_DEMO_INPUT` picks the device, as it does in the app.
        let wanted = std::env::var("OSM_SOUND_DEMO_INPUT").ok();
        let vj = player
            .start_vj(wanted.as_deref().filter(|name| !name.is_empty()))
            .expect("opening the input");
        eprintln!("listening to {}", vj.device());

        let mut loudest = [0.0f32; BINS];
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let levels = analyzer.poll();
            for (peak, level) in loudest.iter_mut().zip(levels.iter()) {
                *peak = peak.max(*level);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        eprintln!("loudest bands over two seconds: {loudest:?}");

        // Dropping it must stop the reader thread rather than leave it running.
        let stopped = std::time::Instant::now();
        drop(vj);
        assert!(
            stopped.elapsed() < Duration::from_secs(2),
            "the input thread took {:?} to stop",
            stopped.elapsed()
        );
    }
}
