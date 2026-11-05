//! Wave mode: the skyline cut into rings around the map's centre instead of
//! into height slices, with the beat travelling outwards through them.
//!
//! This is the web demo's wave mode. It added a second set of layers for the
//! rings; here the height slices' own layers are refiltered instead, since a
//! change to the layer set is what makes MapLibre Native re-run tile layout
//! (see `sync_bands`). Each ring is a `distance` filter from the centre, which
//! MapLibre Native evaluates against polygons as well as points and lines.

use std::collections::VecDeque;
use std::time::Instant;

use crate::audio::BINS;

/// How many rings the buildings are cut into. The outermost has no outer edge,
/// so it takes everything beyond the others.
pub const RINGS: usize = 24;

/// How wide each ring is, in metres.
const RING_WIDTH: f64 = 80.0;

/// How long the beat takes to travel one ring outwards. The web demo moved it
/// three frames a ring, which is 50 ms at 60 fps; it is measured in time here
/// because this app's frame rate is not fixed.
const RING_DELAY: f64 = 0.05;

/// How many of the lowest bands count as the beat.
const BEAT_BANDS: usize = 4;

/// How far the camera has to drift from the centre before the rings are drawn
/// around a new one, and how long it has to have stopped there. Redrawing them
/// refilters every ring, which re-runs tile layout for the building source, so
/// it waits for the map to settle — the web demo waited for `moveend`.
const RECENTRE_METRES: f64 = RING_WIDTH / 4.0;
const RECENTRE_SETTLE: f64 = 0.3;

/// The style-spec filter for one ring around `centre`, longitude first.
pub fn ring_filter(ring: usize, centre: (f64, f64)) -> serde_json::Value {
    let point = serde_json::json!({ "type": "Point", "coordinates": [centre.0, centre.1] });
    let inner = ring as f64 * RING_WIDTH;
    let mut filter = vec![
        serde_json::json!("all"),
        serde_json::json!([">=", ["distance", point], inner]),
    ];
    if ring < RINGS - 1 {
        filter.push(serde_json::json!(["<", ["distance", point], inner + RING_WIDTH]));
    }
    serde_json::Value::Array(filter)
}

/// The wave's state on the UI side: where its centre is, and the recent beat
/// the outer rings are still to feel.
#[derive(Debug)]
pub struct Wave {
    origin: Instant,
    centre: (f64, f64),
    /// The beat level, 0–255 as the web demo read it, against when it was
    /// heard in seconds since `origin`. Oldest first.
    history: VecDeque<(f64, f64)>,
    /// Where the camera was last seen away from the centre, and since when.
    away: Option<((f64, f64), f64)>,
}

impl Wave {
    pub fn new(centre: (f64, f64)) -> Self {
        Self {
            origin: Instant::now(),
            centre,
            history: VecDeque::new(),
            away: None,
        }
    }

    pub fn centre(&self) -> (f64, f64) {
        self.centre
    }

    /// Takes the camera's centre for this frame, and returns a new centre for
    /// the rings once the camera has moved away from the old one and stopped.
    pub fn follow(&mut self, camera: (f64, f64)) -> Option<(f64, f64)> {
        self.follow_at(camera, self.origin.elapsed().as_secs_f64())
    }

    fn follow_at(&mut self, camera: (f64, f64), now: f64) -> Option<(f64, f64)> {
        if metres_between(camera, self.centre) < RECENTRE_METRES {
            self.away = None;
            return None;
        }
        match self.away {
            Some((seen, since)) if metres_between(camera, seen) < 1.0 => {
                if now - since < RECENTRE_SETTLE {
                    return None;
                }
                self.centre = camera;
                self.away = None;
                Some(camera)
            }
            _ => {
                self.away = Some((camera, now));
                None
            }
        }
    }

    /// One frame of the wave: each ring's extrusion height, innermost first.
    pub fn heights(&mut self, levels: &[f32; BINS]) -> [f64; RINGS] {
        self.heights_at(levels, self.origin.elapsed().as_secs_f64())
    }

    fn heights_at(&mut self, levels: &[f32; BINS], now: f64) -> [f64; RINGS] {
        let beat = levels[..BEAT_BANDS]
            .iter()
            .map(|level| f64::from(*level) * 255.0)
            .sum::<f64>()
            / BEAT_BANDS as f64;
        self.history.push_back((now, beat));
        // Keep one sample older than the outermost ring needs, so it still has
        // something to read.
        let horizon = now - RINGS as f64 * RING_DELAY;
        while self.history.len() > 1 && self.history[1].0 <= horizon {
            self.history.pop_front();
        }

        std::array::from_fn(|ring| {
            let heard = now - ring as f64 * RING_DELAY;
            let delayed = self
                .history
                .iter()
                .rev()
                .find(|(at, _)| *at <= heard)
                .map_or(0.0, |(_, beat)| *beat);
            // The web demo's ripple: `sin(now / 200 - i * 0.5)`, `now` in ms.
            let ripple = (now * 5.0 - ring as f64 * 0.5).sin() * (delayed / 255.0) * 30.0;
            (10.0 + delayed * 0.8 + ripple).max(0.0)
        })
    }

    /// Forgets the beat, for when the music stops.
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

/// Distance between two points, longitude first, in metres. Flat-earth, which
/// over a few hundred metres is far closer than this needs.
fn metres_between(a: (f64, f64), b: (f64, f64)) -> f64 {
    let latitude = ((a.1 + b.1) / 2.0).to_radians();
    let dx = (a.0 - b.0) * 111_320.0 * latitude.cos();
    let dy = (a.1 - b.1) * 110_540.0;
    dx.hypot(dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: (f64, f64) = (139.767165, 35.680655);

    #[test]
    fn rings_are_contiguous_and_the_last_is_open() {
        let first = ring_filter(0, TOKYO);
        assert_eq!(first[1][2], 0.0);
        assert_eq!(first[2][2], RING_WIDTH);
        let second = ring_filter(1, TOKYO);
        assert_eq!(second[1][2], RING_WIDTH);
        let last = ring_filter(RINGS - 1, TOKYO);
        assert_eq!(last.as_array().map(Vec::len), Some(2));
        assert_eq!(
            first[1][1][1]["coordinates"],
            serde_json::json!([TOKYO.0, TOKYO.1])
        );
    }

    #[test]
    fn the_beat_reaches_the_outer_rings_later() {
        let mut wave = Wave::new(TOKYO);
        let mut loud = [0.0; BINS];
        loud[..BEAT_BANDS].fill(1.0);
        // Silence, then a beat: only the inner ring has heard it.
        wave.heights_at(&[0.0; BINS], 0.0);
        let heights = wave.heights_at(&loud, 1.0);
        assert!(heights[0] > 100.0);
        assert!(heights[1] <= 10.0 + f64::EPSILON);
        // Half a second on, the beat is ten rings out and no further.
        let heights = wave.heights_at(&loud, 1.52);
        assert!(heights[10] > 100.0);
        assert!(heights[11] <= 10.0 + f64::EPSILON);
    }

    #[test]
    fn silence_rests_every_ring_at_the_base() {
        let mut wave = Wave::new(TOKYO);
        assert!(
            wave.heights_at(&[0.0; BINS], 0.0)
                .iter()
                .all(|height| *height == 10.0)
        );
    }

    #[test]
    fn the_history_keeps_only_what_the_outer_ring_needs() {
        let mut wave = Wave::new(TOKYO);
        for frame in 0..1000 {
            wave.heights_at(&[0.5; BINS], frame as f64 / 60.0);
        }
        assert!(wave.history.len() <= (RINGS as f64 * RING_DELAY * 60.0) as usize + 2);
    }

    #[test]
    fn the_centre_follows_the_camera_once_it_settles() {
        let mut wave = Wave::new(TOKYO);
        let moved = (TOKYO.0 + 0.01, TOKYO.1);
        // Close by: nothing to do.
        assert_eq!(wave.follow_at((TOKYO.0 + 0.0001, TOKYO.1), 0.0), None);
        // Moved away, but still moving.
        assert_eq!(wave.follow_at((TOKYO.0 + 0.005, TOKYO.1), 0.0), None);
        assert_eq!(wave.follow_at(moved, 0.1), None);
        assert_eq!(wave.follow_at(moved, 0.2), None);
        // Stopped long enough.
        assert_eq!(wave.follow_at(moved, 0.45), Some(moved));
        assert_eq!(wave.centre(), moved);
        assert_eq!(wave.follow_at(moved, 1.0), None);
    }

    #[test]
    fn metres_are_about_right() {
        // One thousandth of a degree of latitude is about 110 m.
        let metres = metres_between(TOKYO, (TOKYO.0, TOKYO.1 + 0.001));
        assert!((metres - 110.5).abs() < 1.0, "{metres}");
    }
}
