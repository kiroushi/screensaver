//! When each keyframe shows. A keyframe replaces the one before it in a single step, and
//! nothing on screen changes in between.

use crate::{Look, Rng};

/// Time from one keyframe to the next.
const PERIOD: f64 = 10.0;

fn keyframe_rng(seed: u64, index: u64) -> Rng {
    Rng::new(seed ^ index.wrapping_mul(0xA076_1D64_78BD_642F))
}

/// The look of keyframe `index` in a run seeded with `seed`.
pub fn keyframe_look(seed: u64, index: u64) -> Look {
    draw_look(&mut keyframe_rng(seed, index))
}

/// Draws a look from a keyframe's generator.
fn draw_look(rng: &mut Rng) -> Look {
    Look {
        seed: rng.next_u64() as u32,
        hue_shift: rng.range(0.0, 2.0) as f32,
    }
}

/// Which keyframe is on screen and when the next one is due.
#[derive(Clone, Debug)]
pub struct Timeline {
    seed: u64,
    /// The keyframe on screen and the time it counts from.
    shown: Option<(u64, f64)>,
}

impl Timeline {
    pub fn new(seed: u64) -> Self {
        Self { seed, shown: None }
    }

    /// The look of keyframe `index`.
    pub fn look(&self, index: u64) -> Look {
        keyframe_look(self.seed, index)
    }

    /// The keyframe on screen.
    pub fn shown(&self) -> Option<u64> {
        self.shown.map(|(index, _)| index)
    }

    /// The keyframe that shows next.
    pub fn next(&self) -> u64 {
        self.shown.map_or(0, |(index, _)| index + 1)
    }

    /// When the next keyframe is due, in seconds. The first one is due at once.
    pub fn due(&self) -> f64 {
        self.shown.map_or(0.0, |(_, start)| start + PERIOD)
    }

    /// Records that the next keyframe appeared at `time`. A keyframe that appears within a
    /// period of its due time counts from the due time, so the time to draw it does not add up
    /// from keyframe to keyframe. After a longer sleep the count starts again.
    pub fn show(&mut self, time: f64) {
        let due = self.due();
        let start = if time - due < PERIOD { due } else { time };
        self.shown = Some((self.next(), start));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::field::Probe;

    #[test]
    fn keyframes_keep_to_the_grid_and_restart_it_after_a_long_sleep() {
        let mut timeline = Timeline::new(5);
        assert_eq!((timeline.next(), timeline.due()), (0, 0.0));
        timeline.show(0.04);
        assert_eq!((timeline.shown(), timeline.due()), (Some(0), PERIOD));
        timeline.show(PERIOD + 0.05);
        assert_eq!((timeline.shown(), timeline.due()), (Some(1), 2.0 * PERIOD));
        timeline.show(3600.0);
        assert_eq!(
            (timeline.shown(), timeline.due()),
            (Some(2), 3600.0 + PERIOD)
        );
    }

    #[test]
    fn three_days_of_keyframes_all_differ() {
        let keyframes = (3.0 * 86_400.0 / PERIOD) as u64;
        let seeds: std::collections::HashSet<u32> = (keyframes - 100..keyframes)
            .chain(0..100)
            .map(|index| keyframe_look(11, index).seed)
            .collect();
        assert_eq!(seeds.len(), 200, "keyframe seeds repeat");
    }

    const WEAR_SCREEN: [usize; 2] = [64, 27];
    const WEAR_SECONDS: f64 = 6.0 * 3600.0;

    struct Wear {
        /// Time-averaged light per screen cell and channel, from 0 to 1.
        mean: Vec<[f64; 3]>,
        /// Longest time any cell spent outside the masses, in seconds.
        longest_dark: f64,
    }

    /// Six hours of keyframes on a 64x27 screen, each shown for its 10 s. Stars are left out,
    /// because at this size each would cover whole cells.
    fn wear() -> &'static Wear {
        static WEAR: OnceLock<Wear> = OnceLock::new();
        WEAR.get_or_init(|| {
            let [width, height] = WEAR_SCREEN;
            let mut timeline = Timeline::new(0x5eed);
            let mut sum = vec![[0.0; 3]; width * height];
            let mut dark_since: Vec<Option<f64>> = vec![None; width * height];
            let mut longest_dark: f64 = 0.0;
            let mut keyframes = 0;
            while timeline.due() < WEAR_SECONDS {
                let time = timeline.due();
                timeline.show(time);
                let look = timeline.look(timeline.shown().expect("a keyframe shows"));
                let probe = Probe::draw(look, width, height, height as u32, false);
                let end = time + PERIOD;
                for (cell, pixel) in probe.pixels.chunks_exact(4).enumerate() {
                    for (k, sum) in sum[cell].iter_mut().enumerate() {
                        *sum += f64::from(pixel[2 - k]) / 255.0;
                    }
                    if probe.density[cell] < 0.5 {
                        let since = *dark_since[cell].get_or_insert(time);
                        longest_dark = longest_dark.max(end - since);
                    } else {
                        dark_since[cell] = None;
                    }
                }
                keyframes += 1;
            }
            Wear {
                mean: sum
                    .iter()
                    .map(|s| s.map(|v| v / f64::from(keyframes)))
                    .collect(),
                longest_dark,
            }
        })
    }

    // The timeline measures 1.110, 1.129 and 1.072 for red, green and blue.
    #[test]
    fn every_cell_wears_evenly() {
        let wear = wear();
        for channel in 0..3 {
            let (min, max) = wear
                .mean
                .iter()
                .map(|m| m[channel])
                .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
            let ratio = max / min;
            assert!(
                ratio < 1.22,
                "channel {channel}: time-averaged max/min {ratio:.3}"
            );
        }
    }

    // The timeline measures 130 s.
    #[test]
    fn no_cell_stays_dark_for_long() {
        let longest = wear().longest_dark;
        assert!(
            longest < 240.0,
            "a cell stayed outside the masses for {longest:.0} s"
        );
    }
}
