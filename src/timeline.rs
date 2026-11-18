//! The life of each keyframe: when it shows and how it fades in over the keyframe before it.
//!
//! Nothing on screen changes between two fades, so the client sleeps there and the compositor
//! repaints nothing. A fade is a fixed number of alpha levels, one repaint each.

use crate::{Look, Rng};

/// Time from the start of one keyframe to the start of the next.
const PERIOD: f64 = 10.0;
/// Alpha levels a keyframe goes through as it fades in, from `1 / FADE_STEPS` to 1. Each level
/// is one repaint of the screen.
pub const FADE_STEPS: u32 = 30;
/// Time between two alpha levels. A change of brightness in steps looks continuous when the
/// steps come this fast and each is small. With 30 levels the largest step between two
/// unrelated keyframes is 8 of 255 codes, the size of the steps in a one-second film dissolve.
const STEP: f64 = 1.0 / 30.0;
/// How long before a keyframe is due its drawing starts. Its buffer exists from then on.
const DRAW_LEAD: f64 = 3.0;

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

/// A keyframe on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub index: u64,
    /// The buffer and surface that hold the keyframe.
    pub slot: usize,
    /// Alpha level from 1 to [`FADE_STEPS`].
    pub level: u32,
}

impl Layer {
    pub fn alpha(self) -> f64 {
        f64::from(self.level) / f64::from(FADE_STEPS)
    }
}

#[derive(Clone, Copy, Debug)]
struct Shown {
    index: u64,
    start: f64,
}

/// Which keyframes are on screen at the current time, and how far the top one has faded in.
///
/// A keyframe starts once it is drawn, and no sooner than [`PERIOD`] after the one before. It
/// fades in over the keyframe before it, or from black for the first keyframe, and the keyframe
/// under it leaves with the last level. Keyframe `n` lives in slot `n % 2`. Its drawing starts
/// [`DRAW_LEAD`] before it is due, by which time keyframe `n - 2` has left the slot.
#[derive(Clone, Debug)]
pub struct Timeline {
    seed: u64,
    time: f64,
    /// Keyframes `0..drawn` are drawn.
    drawn: u64,
    /// Keyframes `0..retired` have left the screen.
    retired: u64,
    /// Keyframes on screen from bottom to top.
    shown: Vec<Shown>,
}

impl Timeline {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            time: 0.0,
            drawn: 0,
            retired: 0,
            shown: Vec::with_capacity(2),
        }
    }

    pub fn time(&self) -> f64 {
        self.time
    }

    /// The look of keyframe `index`.
    pub fn look(&self, index: u64) -> Look {
        keyframe_look(self.seed, index)
    }

    /// The next keyframe to draw, once its slot is free and it is due within [`DRAW_LEAD`].
    pub fn wanted(&self) -> Option<u64> {
        let due = match self.shown.last() {
            None => self.drawn == 0,
            Some(top) => self.drawn == top.index + 1 && self.time >= top.start + PERIOD - DRAW_LEAD,
        };
        (due && self.drawn < self.retired + 2).then_some(self.drawn)
    }

    /// Records that keyframe `index`, the one [`Timeline::wanted`] named, is completely drawn.
    pub fn mark_drawn(&mut self, index: u64) {
        assert_eq!(Some(index), self.wanted(), "keyframes are drawn in order");
        self.drawn += 1;
    }

    /// Moves the timeline to `time` in seconds, which never goes back. A keyframe that is due
    /// starts at that time, so a client that wakes late still shows the whole fade.
    pub fn advance_to(&mut self, time: f64) {
        self.time = self.time.max(time);
        match self.shown.last().copied() {
            None if self.drawn > 0 => self.start(0),
            Some(top) if self.drawn > top.index + 1 && self.time >= top.start + PERIOD => {
                self.start(top.index + 1);
            }
            _ => {}
        }
        if let [_, top] = self.shown[..]
            && self.level(top) == FADE_STEPS
        {
            self.shown.remove(0);
            self.retired += 1;
        }
    }

    fn start(&mut self, index: u64) {
        self.shown.push(Shown {
            index,
            start: self.time,
        });
    }

    /// Alpha level of a keyframe on screen. It starts at 1, so no repaint shows nothing new.
    fn level(&self, shown: Shown) -> u32 {
        // The margin keeps a wake at the exact time of a level from falling just short of it.
        let steps = ((self.time - shown.start) / STEP + 1e-6).floor();
        (steps as u32).saturating_add(1).min(FADE_STEPS)
    }

    /// The keyframes on screen from bottom to top.
    pub fn layers(&self) -> impl Iterator<Item = Layer> + '_ {
        self.shown.iter().map(|&shown| Layer {
            index: shown.index,
            slot: (shown.index % 2) as usize,
            level: self.level(shown),
        })
    }

    /// When the screen next changes, if that only waits for time to pass. It is the current
    /// time when a drawn keyframe waits to start.
    pub fn next_change(&self) -> Option<f64> {
        let Some(&top) = self.shown.last() else {
            return (self.drawn > 0).then_some(self.time);
        };
        let level = self.level(top);
        if level < FADE_STEPS {
            Some(top.start + f64::from(level) * STEP)
        } else if self.drawn > top.index + 1 {
            Some((top.start + PERIOD).max(self.time))
        } else {
            None
        }
    }

    /// When the drawing of the next keyframe starts, if that only waits for time to pass.
    pub fn next_drawing(&self) -> Option<f64> {
        let top = self.shown.last()?;
        (self.drawn == top.index + 1).then_some(top.start + PERIOD - DRAW_LEAD)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::field::Probe;

    /// Drives a timeline like the client does: it wakes only when the timeline asks, and it
    /// finishes each keyframe `delay` seconds after the timeline wants it.
    struct Run<F: Fn(u64) -> f64> {
        timeline: Timeline,
        delay: F,
        pending: Option<(u64, f64)>,
        /// Times at which the screen changed.
        changes: Vec<f64>,
    }

    impl<F: Fn(u64) -> f64> Run<F> {
        fn new(seed: u64, delay: F) -> Self {
            Self {
                timeline: Timeline::new(seed),
                delay,
                pending: None,
                changes: Vec::new(),
            }
        }

        /// Runs until the next wake after `until` and returns the time of that wake.
        fn run(&mut self, until: f64) -> f64 {
            let mut time = self.timeline.time();
            loop {
                let before: Vec<Layer> = self.timeline.layers().collect();
                self.timeline.advance_to(time);
                if self.pending.is_none()
                    && let Some(index) = self.timeline.wanted()
                {
                    self.pending = Some((index, time + (self.delay)(index)));
                }
                if let Some((index, at)) = self.pending
                    && time >= at
                {
                    self.timeline.mark_drawn(index);
                    self.pending = None;
                    self.timeline.advance_to(time);
                }
                if before != self.timeline.layers().collect::<Vec<Layer>>() {
                    self.changes.push(time);
                }
                let wakes = [
                    self.timeline.next_change(),
                    self.pending.map(|(_, at)| at),
                    self.timeline
                        .next_drawing()
                        .filter(|_| self.pending.is_none()),
                ];
                let next = wakes
                    .into_iter()
                    .flatten()
                    .filter(|&wake| wake > time)
                    .fold(f64::INFINITY, f64::min);
                assert!(
                    next.is_finite(),
                    "the client would sleep for ever at {time}"
                );
                if next > until {
                    return next;
                }
                time = next;
            }
        }
    }

    fn instant(_: u64) -> f64 {
        0.0
    }

    /// Drawing times from nothing to well past the 3 s a keyframe has before it is due.
    fn late(seed: u64) -> impl Fn(u64) -> f64 {
        move |index| keyframe_rng(seed ^ 0x0de1_a7ed, index).range(0.0, 14.0)
    }

    // A fade is 30 changes at most 1/30 s apart, each one level higher, and nothing changes
    // between fades.
    #[test]
    fn one_or_two_keyframes_show_and_the_top_fades_in_by_single_levels() {
        for seed in 0..8 {
            let mut run = Run::new(seed, late(seed));
            let mut previous: Option<Layer> = None;
            let mut time = 0.0;
            while time < 600.0 {
                time = run.run(time);
                let layers: Vec<Layer> = run.timeline.layers().collect();
                let Some(&top) = layers.last() else {
                    assert!(previous.is_none(), "seed {seed}: the screen went blank");
                    continue;
                };
                assert!(layers.len() <= 2);
                if let [bottom, _] = layers[..] {
                    assert_eq!(
                        bottom.level, FADE_STEPS,
                        "seed {seed}: the bottom is see-through"
                    );
                    assert!(
                        top.level < FADE_STEPS,
                        "seed {seed}: the bottom outlived the fade"
                    );
                }
                match previous {
                    Some(last) if last.index == top.index => {
                        assert!(
                            top.level <= last.level + 1,
                            "seed {seed}: a level was skipped"
                        );
                    }
                    Some(last) => {
                        assert_eq!(top.index, last.index + 1);
                        assert_eq!(last.level, FADE_STEPS, "seed {seed}: a fade was cut short");
                        assert_eq!(top.level, 1, "seed {seed}: the new keyframe popped in");
                    }
                    None => assert_eq!(top.level, 1, "seed {seed}: no fade from black"),
                }
                previous = Some(top);
            }
            let keyframes = previous.expect("keyframes showed").index + 1;
            assert!(run.changes.len() as u64 <= keyframes * u64::from(FADE_STEPS));
        }
    }

    #[test]
    fn three_days_change_the_screen_only_in_fades() {
        let mut run = Run::new(11, instant);
        run.run(3.0 * 86_400.0 + 5.0);
        let top = run.timeline.layers().last().expect("a keyframe shows");
        assert_eq!(top.index, 3 * 86_400 / PERIOD as u64);
        assert_eq!(
            run.changes.len() as u64,
            (top.index + 1) * u64::from(FADE_STEPS)
        );
        for pair in run.changes.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                !(STEP + 1e-6..=PERIOD - 1.0).contains(&gap),
                "the screen changed {gap} s after the change before"
            );
        }
        let seeds: std::collections::HashSet<u32> = (top.index - 50..top.index + 50)
            .chain(0..100)
            .map(|index| keyframe_look(11, index).seed)
            .collect();
        assert_eq!(seeds.len(), 200, "keyframe seeds repeat");
    }

    // A keyframe has 3 s to be drawn before it is due.
    #[test]
    fn at_most_two_keyframes_are_alive_and_each_starts_on_time() {
        let mut run = Run::new(3, |_| DRAW_LEAD - 0.5);
        let mut last: Option<(u64, f64)> = None;
        let mut time = 0.0;
        while time < 300.0 {
            time = run.run(time);
            let timeline = &run.timeline;
            let alive = timeline.drawn + u64::from(run.pending.is_some()) - timeline.retired;
            assert!(alive <= 2, "{alive} keyframes are alive");
            let Some(&top) = timeline.shown.last() else {
                continue;
            };
            if let Some((index, start)) = last
                && index != top.index
            {
                let gap = top.start - start;
                assert!(
                    (gap - PERIOD).abs() < 1e-9,
                    "keyframe {} started {gap} s after the one before",
                    top.index
                );
            }
            last = Some((top.index, top.start));
        }
        assert!(last.expect("keyframes showed").0 >= 29);
    }

    // A client that slept through several periods shows one fade when it wakes, in full.
    #[test]
    fn a_long_sleep_does_not_skip_or_repeat_a_fade() {
        let mut timeline = Timeline::new(5);
        timeline.mark_drawn(0);
        timeline.advance_to(0.0);
        timeline.advance_to(8.0);
        timeline.mark_drawn(1);
        timeline.advance_to(3600.0);
        let layers: Vec<Layer> = timeline.layers().collect();
        assert_eq!(layers.len(), 2);
        assert_eq!((layers[1].index, layers[1].level), (1, 1));
        assert_eq!(timeline.next_change(), Some(3600.0 + STEP));
    }

    const WEAR_SCREEN: [usize; 2] = [64, 27];
    const WEAR_SECONDS: f64 = 6.0 * 3600.0;

    struct Wear {
        /// Time-averaged light per screen cell and channel, from 0 to 1.
        mean: Vec<[f64; 3]>,
        /// Longest time any cell spent outside the masses, in seconds.
        longest_dark: f64,
    }

    /// Six hours of keyframes on a 64x27 screen through the real fades, sampled every two
    /// seconds. Stars are left out, because at this size each would cover whole cells.
    fn wear() -> &'static Wear {
        static WEAR: OnceLock<Wear> = OnceLock::new();
        WEAR.get_or_init(|| {
            let [width, height] = WEAR_SCREEN;
            let mut run = Run::new(0x5eed, instant);
            let mut images: [(u64, Vec<[f32; 4]>); 2] =
                [(u64::MAX, Vec::new()), (u64::MAX, Vec::new())];
            let mut sum = vec![[0.0; 3]; width * height];
            let mut dark_since: Vec<Option<f64>> = vec![None; width * height];
            let mut longest_dark: f64 = 0.0;
            let mut samples = 0;
            let mut light = vec![[0.0f64; 4]; width * height];
            let mut time = 0.0;
            while time < WEAR_SECONDS {
                time += 2.0;
                run.run(time);
                let timeline = &run.timeline;
                light.fill([0.0; 4]);
                for layer in timeline.layers() {
                    let (index, texels) = &mut images[layer.slot];
                    if *index != layer.index {
                        let look = timeline.look(layer.index);
                        let probe = Probe::draw(look, width, height, height as u32, false);
                        *index = layer.index;
                        *texels = probe
                            .pixels
                            .chunks_exact(4)
                            .zip(&probe.density)
                            .map(|(p, &d)| {
                                [
                                    f32::from(p[2]) / 255.0,
                                    f32::from(p[1]) / 255.0,
                                    f32::from(p[0]) / 255.0,
                                    d,
                                ]
                            })
                            .collect();
                    }
                    let alpha = layer.alpha();
                    for (cell, texel) in light.iter_mut().zip(texels.iter()) {
                        for k in 0..4 {
                            cell[k] += (f64::from(texel[k]) - cell[k]) * alpha;
                        }
                    }
                }
                for cell in 0..width * height {
                    for k in 0..3 {
                        sum[cell][k] += light[cell][k];
                    }
                    if light[cell][3] < 0.5 {
                        let since = *dark_since[cell].get_or_insert(time);
                        longest_dark = longest_dark.max(time - since);
                    } else {
                        dark_since[cell] = None;
                    }
                }
                samples += 1;
            }
            Wear {
                mean: sum
                    .iter()
                    .map(|s| s.map(|v| v / f64::from(samples)))
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

    // The timeline measures 128 s.
    #[test]
    fn no_cell_stays_dark_for_long() {
        let longest = wear().longest_dark;
        assert!(
            longest < 240.0,
            "a cell stayed outside the masses for {longest:.0} s"
        );
    }
}
