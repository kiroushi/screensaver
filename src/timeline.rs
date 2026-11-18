//! The life of each keyframe: when it shows, how its source rectangle pans and zooms across its
//! buffer, and how it fades in over the keyframe before it.

use crate::{Look, Rng};

/// Largest step of the timeline clock per frame, in seconds.
const MAX_STEP: f64 = 0.1;
/// Time from the start of one keyframe to the start of the next.
const PERIOD: f64 = 10.0;
/// How long a keyframe takes to fade in over the one before.
const FADE: f64 = 4.0;
/// How long the first keyframe takes to fade in from black.
const FIRST_FADE: f64 = 1.0;
/// How long a keyframe is on screen when the next one is drawn in time.
const LIFE: f64 = PERIOD + FADE;
/// How long before a keyframe is due its drawing starts. Its buffer exists from then on. The
/// keyframe before the last one leaves when its fade ends, so no earlier start keeps the buffers
/// at two.
const DRAW_LEAD: f64 = PERIOD - FADE;
/// Source rectangle size relative to the screen at the two ends of a path.
const ZOOM: [f64; 2] = [1.0, 1.0156];
/// `wl_fixed` steps per pixel.
const FIXED: f64 = 256.0;

/// How long keyframe `index` takes to fade in.
fn fade(index: u64) -> f64 {
    if index == 0 { FIRST_FADE } else { FADE }
}

fn keyframe_rng(seed: u64, index: u64) -> Rng {
    Rng::new(seed ^ index.wrapping_mul(0xA076_1D64_78BD_642F))
}

/// The look of keyframe `index` in a run seeded with `seed`.
pub fn keyframe_look(seed: u64, index: u64) -> Look {
    draw_look(&mut keyframe_rng(seed, index))
}

/// Draws a look from a keyframe's generator, before its path.
fn draw_look(rng: &mut Rng) -> Look {
    Look {
        seed: rng.next_u64() as u32,
        hue_shift: rng.range(0.0, 2.0) as f32,
    }
}

/// A straight pan of the source rectangle's centre and a linear zoom, in buffer pixels.
#[derive(Clone, Copy, Debug)]
struct Path {
    from: [f64; 2],
    to: [f64; 2],
    zoom: [f64; 2],
}

impl Path {
    /// The path of keyframe `index`. It runs in a random direction, zooms in or out at random,
    /// and is as long as the room the zoom leaves in the buffer allows.
    fn new(seed: u64, index: u64, screen: [f64; 2], buffer: [f64; 2]) -> Self {
        let mut rng = keyframe_rng(seed, index);
        draw_look(&mut rng);
        let zoom = if rng.below(2) == 0 {
            ZOOM
        } else {
            [ZOOM[1], ZOOM[0]]
        };
        let angle = rng.range(0.0, std::f64::consts::TAU);
        let direction = [angle.cos(), angle.sin()];
        // How far the centre may stray from the buffer's centre on each axis, at each end.
        let room = zoom.map(|z| [0, 1].map(|a| ((buffer[a] - screen[a] * z) / 2.0).max(0.0)));
        let length = (0..2)
            .filter(|&a| direction[a] != 0.0)
            .map(|a| (room[0][a] + room[1][a]) / direction[a].abs())
            .fold(f64::INFINITY, f64::min);
        let from = [0, 1].map(|a| {
            let travel = length * direction[a];
            let low = (-room[0][a]).max(-room[1][a] - travel);
            let high = room[0][a].min(room[1][a] - travel);
            buffer[a] / 2.0 + (low + high) / 2.0
        });
        let to = [0, 1].map(|a| from[a] + length * direction[a]);
        Self { from, to, zoom }
    }

    /// Centre and zoom at `progress` from 0 at the start to 1 at the end.
    fn at(&self, progress: f64) -> ([f64; 2], f64) {
        let centre = [0, 1].map(|a| self.from[a] + (self.to[a] - self.from[a]) * progress);
        (
            centre,
            self.zoom[0] + (self.zoom[1] - self.zoom[0]) * progress,
        )
    }
}

/// A source rectangle in `wl_fixed` steps of 1/256 buffer pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Source {
    /// The rectangle in buffer pixels, each value exactly representable as `wl_fixed`.
    pub fn pixels(self) -> [f64; 4] {
        [self.x, self.y, self.width, self.height].map(|v| f64::from(v) / FIXED)
    }
}

/// A keyframe on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layer {
    pub index: u64,
    /// The buffer and surface that hold the keyframe.
    pub slot: usize,
    pub alpha: f64,
    pub source: Source,
    /// Centre of the source rectangle in buffer pixels, before rounding to `wl_fixed`.
    pub centre: [f64; 2],
    pub zoom: f64,
}

#[derive(Clone, Copy, Debug)]
struct Shown {
    index: u64,
    start: f64,
    path: Path,
}

/// Which keyframes are on screen at the current time, and where.
///
/// A keyframe starts once it is drawn, and no sooner than [`PERIOD`] after the one before. It
/// fades in over [`FADE`], or [`FIRST_FADE`] from black for the first keyframe, and the
/// keyframe under it leaves when the fade ends. Keyframe `n` lives in slot `n % 2`. Its drawing
/// starts [`DRAW_LEAD`] before it is due, by which time keyframe `n - 2` has left the slot.
#[derive(Clone, Debug)]
pub struct Timeline {
    seed: u64,
    /// Screen size in buffer pixels.
    screen: [f64; 2],
    buffer: [u32; 2],
    time: f64,
    /// Keyframes `0..drawn` are drawn.
    drawn: u64,
    /// Timeline time at which the last keyframe finished drawing.
    drawn_at: f64,
    /// Keyframes `0..retired` have left the screen.
    retired: u64,
    /// Keyframes on screen from bottom to top.
    shown: Vec<Shown>,
}

impl Timeline {
    /// A timeline for a physical screen size and keyframe buffers whose pixels each span `scale`
    /// screen pixels.
    pub fn new(seed: u64, screen: [u32; 2], buffer: [u32; 2], scale: f64) -> Self {
        Self {
            seed,
            screen: screen.map(|s| f64::from(s) / scale),
            buffer,
            time: 0.0,
            drawn: 0,
            drawn_at: 0.0,
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
        self.drawn_at = self.time;
    }

    /// Moves the timeline on by the time since the last frame, but never by more than
    /// [`MAX_STEP`], so a pan never jumps after the compositor stops asking for frames.
    pub fn advance(&mut self, elapsed: f64) {
        self.time += elapsed.clamp(0.0, MAX_STEP);
        let Some(top) = self.shown.last().copied() else {
            if self.drawn > 0 {
                self.shown.push(self.start(0, self.time));
            }
            return;
        };
        let next = top.index + 1;
        if self.drawn > next && self.time >= top.start + PERIOD {
            // Keyframe `next` is the last one drawn, because the one after it shares the slot of
            // the top keyframe.
            let start = self.drawn_at.max(top.start + PERIOD);
            self.shown.push(self.start(next, start));
        }
        if let [_, top] = self.shown[..]
            && self.time >= top.start + FADE
        {
            self.shown.remove(0);
            self.retired += 1;
        }
    }

    fn start(&self, index: u64, start: f64) -> Shown {
        let buffer = self.buffer.map(f64::from);
        Shown {
            index,
            start,
            path: Path::new(self.seed, index, self.screen, buffer),
        }
    }

    /// The keyframes on screen from bottom to top.
    pub fn layers(&self) -> impl Iterator<Item = Layer> + '_ {
        self.shown.iter().map(|shown| {
            let age = self.time - shown.start;
            let (centre, zoom) = shown.path.at((age / LIFE).clamp(0.0, 1.0));
            Layer {
                index: shown.index,
                slot: (shown.index % 2) as usize,
                alpha: (age / fade(shown.index)).clamp(0.0, 1.0),
                source: self.source(centre, zoom),
                centre,
                zoom,
            }
        })
    }

    /// The source rectangle around `centre` at `zoom`, rounded to `wl_fixed` and kept inside the
    /// buffer, where the rounding could otherwise push it out by one step.
    fn source(&self, centre: [f64; 2], zoom: f64) -> Source {
        let [x, y] = [0, 1].map(|a| {
            let limit = i64::from(self.buffer[a]) * FIXED as i64;
            let size = ((self.screen[a] * zoom * FIXED).round() as i64).clamp(1, limit);
            let origin = (((centre[a] - self.screen[a] * zoom / 2.0) * FIXED).round() as i64)
                .clamp(0, limit - size);
            (origin as i32, size as i32)
        });
        Source {
            x: x.0,
            y: y.0,
            width: x.1,
            height: y.1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::buffer_layout;
    use crate::field::Probe;

    /// Time between the client's frames.
    const FRAME: f64 = 1.0 / 30.0;

    /// Drives a timeline frame by frame like the client does, finishing each keyframe `delay`
    /// seconds after the timeline asks for it.
    struct Run<F: Fn(u64) -> f64> {
        timeline: Timeline,
        delay: F,
        pending: Option<(u64, f64)>,
    }

    impl<F: Fn(u64) -> f64> Run<F> {
        fn new(seed: u64, screen: [u32; 2], delay: F) -> Self {
            let (buffer, scale) = buffer_layout(screen);
            Self {
                timeline: Timeline::new(seed, screen, buffer, scale),
                delay,
                pending: None,
            }
        }

        fn frame(&mut self) {
            loop {
                if self.pending.is_none()
                    && let Some(index) = self.timeline.wanted()
                {
                    self.pending = Some((index, self.timeline.time() + (self.delay)(index)));
                }
                match self.pending {
                    Some((index, at)) if self.timeline.time() >= at => {
                        self.timeline.mark_drawn(index);
                        self.pending = None;
                    }
                    _ => break,
                }
            }
            self.timeline.advance(FRAME);
        }

        fn top(&self) -> Option<Layer> {
            self.timeline.layers().last()
        }
    }

    fn instant(_: u64) -> f64 {
        0.0
    }

    /// Drawing times from nothing to well past the 6 s a keyframe has before it is due.
    fn late(seed: u64) -> impl Fn(u64) -> f64 {
        move |index| keyframe_rng(seed ^ 0x0de1_a7ed, index).range(0.0, 70.0)
    }

    fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
        (a[0] - b[0]).hypot(a[1] - b[1])
    }

    // The unrounded rectangle comes within 1.1e-13 px of the buffer edge, because every path uses
    // all the room its zoom leaves.
    #[test]
    fn source_stays_inside_the_buffer() {
        let screens = [
            [5120, 2160],
            [1920, 810],
            [1601, 901],
            [1080, 1920],
            [64, 27],
        ];
        for seed in 0..32 {
            for screen in screens {
                let (buffer, scale) = buffer_layout(screen);
                let limit = buffer.map(|b| i64::from(b) * FIXED as i64);
                let mut run = Run::new(seed, screen, late(seed));
                for _ in 0..(400.0 / FRAME) as u32 {
                    run.frame();
                    for layer in run.timeline.layers() {
                        let Source {
                            x,
                            y,
                            width,
                            height,
                        } = layer.source;
                        for (origin, size, limit) in [(x, width, limit[0]), (y, height, limit[1])] {
                            assert!(
                                origin >= 0 && size > 0 && i64::from(origin + size) <= limit,
                                "seed {seed}, screen {screen:?}: {:?} outside {buffer:?}",
                                layer.source
                            );
                        }
                        for a in 0..2 {
                            let half = f64::from(screen[a]) / scale * layer.zoom / 2.0;
                            assert!(
                                layer.centre[a] - half > -1e-9
                                    && layer.centre[a] + half < f64::from(buffer[a]) + 1e-9,
                                "seed {seed}, screen {screen:?}: unrounded rectangle outside"
                            );
                        }
                        assert!((ZOOM[0]..=ZOOM[1]).contains(&layer.zoom));
                    }
                }
            }
        }
    }

    #[test]
    fn a_long_gap_moves_the_pan_by_a_tenth_of_a_second_at_most() {
        let mut run = Run::new(7, [5120, 2160], instant);
        while run.timeline.time() < 25.0 {
            run.frame();
        }
        let before = run.top().expect("a keyframe shows");
        let time = run.timeline.time();
        let mut ordinary = run.timeline.clone();
        ordinary.advance(0.035);
        assert!(
            (ordinary.time() - time - 0.035).abs() < 1e-9,
            "a 35 ms frame is slowed"
        );
        let mut longest = run.timeline.clone();
        longest.advance(MAX_STEP);
        let reach = distance(
            longest.layers().last().expect("shows").centre,
            before.centre,
        );
        run.timeline.advance(3600.0);
        let after = run.top().expect("a keyframe shows");
        assert!(run.timeline.time() - time <= MAX_STEP + 1e-9);
        assert_eq!(after.index, before.index);
        // A 0.1 s step moves the centre by 0.71 buffer pixels here, which is 2.10 px on screen.
        assert!(reach > 0.5, "reach {reach}");
        assert!(distance(after.centre, before.centre) <= reach + 1e-9);
    }

    // The largest change of alpha in one frame measures 0.0083 in a 4 s fade and 0.033 in the
    // first keyframe's 1 s fade, which is one frame over the length of the fade.
    #[test]
    fn one_or_two_keyframes_show_and_the_top_fades_in_smoothly() {
        let step = |index: u64| FRAME / fade(index) + 1e-9;
        for seed in 0..8 {
            let mut run = Run::new(seed, [1920, 810], late(seed));
            let mut previous: Option<(Layer, usize)> = None;
            for _ in 0..(1200.0 / FRAME) as u32 {
                run.frame();
                let layers: Vec<Layer> = run.timeline.layers().collect();
                let Some(&top) = layers.last() else {
                    assert!(previous.is_none(), "seed {seed}: the screen went blank");
                    continue;
                };
                assert!(layers.len() <= 2);
                if let [bottom, _] = layers[..] {
                    assert_eq!(
                        bottom.alpha, 1.0,
                        "seed {seed}: the bottom keyframe is see-through"
                    );
                }
                match previous {
                    None => assert!(
                        top.alpha <= step(top.index),
                        "seed {seed}: no fade from black"
                    ),
                    Some((last, _)) if last.index == top.index => {
                        assert!(
                            (top.alpha - last.alpha).abs() <= step(top.index),
                            "seed {seed}: alpha jumped"
                        );
                    }
                    Some((last, _)) => {
                        assert_eq!(top.index, last.index + 1);
                        assert_eq!(
                            layers[0].index, last.index,
                            "seed {seed}: nothing under the fade"
                        );
                        assert!(
                            top.alpha <= step(top.index),
                            "seed {seed}: the new keyframe popped in"
                        );
                    }
                }
                if let Some((_, 2)) = previous
                    && layers.len() == 1
                {
                    assert_eq!(
                        top.alpha, 1.0,
                        "seed {seed}: the bottom left during the fade"
                    );
                }
                previous = Some((top, layers.len()));
            }
        }
    }

    /// One window of 30 frames, 5 s into the life of a keyframe.
    struct Window {
        index: u64,
        steps: Vec<f64>,
        expected_step: f64,
    }

    fn observe<F: Fn(u64) -> f64>(run: &mut Run<F>, time: f64) -> Window {
        while run.timeline.time() < time {
            run.frame();
        }
        let shown = *run.timeline.shown.last().expect("a keyframe shows");
        let mut centres = Vec::new();
        for _ in 0..31 {
            run.frame();
            centres.push(run.top().expect("a keyframe shows").centre);
        }
        Window {
            index: shown.index,
            steps: centres.windows(2).map(|w| distance(w[0], w[1])).collect(),
            expected_step: distance(shown.path.from, shown.path.to) / LIFE * FRAME,
        }
    }

    #[test]
    fn three_days_behave_like_five_seconds() {
        let mut run = Run::new(11, [5120, 2160], instant);
        let early = observe(&mut run, 5.0);
        let late = observe(&mut run, 3.0 * 86_400.0 + 5.0);
        assert_eq!(early.index, 0);
        assert_eq!(late.index, 3 * 86_400 / PERIOD as u64);
        for window in [&early, &late] {
            for &step in &window.steps {
                assert!(
                    (step - window.expected_step).abs() < 1e-6,
                    "keyframe {}: step {step} instead of {}",
                    window.index,
                    window.expected_step
                );
            }
        }
        let seeds: std::collections::HashSet<u32> = (late.index - 50..late.index + 50)
            .chain(0..100)
            .map(|index| keyframe_look(11, index).seed)
            .collect();
        assert_eq!(seeds.len(), 200, "keyframe seeds repeat");
    }

    // A keyframe has 6 s to be drawn, from the end of the last fade until it is due.
    #[test]
    fn at_most_two_keyframes_are_alive_and_each_starts_on_time() {
        let mut run = Run::new(3, [5120, 2160], |_| DRAW_LEAD - 0.5);
        let mut last: Option<(u64, f64)> = None;
        while run.timeline.time() < 300.0 {
            run.frame();
            let alive =
                run.timeline.drawn + u64::from(run.pending.is_some()) - run.timeline.retired;
            assert!(alive <= 2, "{alive} keyframes are alive");
            let Some(&top) = run.timeline.shown.last() else {
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

    const WEAR_SCREEN: [u32; 2] = [64, 27];
    const WEAR_SECONDS: f64 = 6.0 * 3600.0;
    const SAMPLE_FRAMES: u32 = 60;

    struct Wear {
        /// Time-averaged light per screen cell and channel, from 0 to 1.
        mean: Vec<[f64; 3]>,
        /// Longest time any cell spent outside the masses, in seconds.
        longest_dark: f64,
    }

    /// Bilinear taps along one axis of the screen for a source rectangle, as the compositor
    /// samples them: the two texels and the weight of the second.
    fn taps(origin: f64, size: f64, cells: usize, texels: usize) -> Vec<(usize, usize, f64)> {
        (0..cells)
            .map(|cell| {
                let t = (origin + (cell as f64 + 0.5) * size / cells as f64 - 0.5)
                    .clamp(0.0, (texels - 1) as f64);
                let first = t.floor() as usize;
                (first, (first + 1).min(texels - 1), t.fract())
            })
            .collect()
    }

    /// Six hours of keyframes on a 64x27 screen through the real pan, zoom and fade, sampled
    /// every two seconds and filtered bilinearly like the compositor does. Stars are left out,
    /// because at this size each would cover whole cells.
    fn wear() -> &'static Wear {
        static WEAR: OnceLock<Wear> = OnceLock::new();
        WEAR.get_or_init(|| {
            let [width, height] = WEAR_SCREEN.map(|s| s as usize);
            let ([bw, bh], scale) = buffer_layout(WEAR_SCREEN);
            assert_eq!(scale, 1.0, "the wear screen is drawn at full resolution");
            let [bw, bh] = [bw, bh].map(|s| s as usize);
            let mut run = Run::new(0x5eed, WEAR_SCREEN, instant);
            let mut images: [(u64, Vec<[f32; 4]>); 2] =
                [(u64::MAX, Vec::new()), (u64::MAX, Vec::new())];
            let mut sum = vec![[0.0; 3]; width * height];
            let mut dark_since: Vec<Option<f64>> = vec![None; width * height];
            let mut longest_dark: f64 = 0.0;
            let mut samples = 0;
            let mut frame = 0;
            let mut light = vec![[0.0f64; 4]; width * height];
            while run.timeline.time() < WEAR_SECONDS {
                run.frame();
                frame += 1;
                if frame % SAMPLE_FRAMES != 0 {
                    continue;
                }
                light.fill([0.0; 4]);
                for layer in run.timeline.layers() {
                    let (index, texels) = &mut images[layer.slot];
                    if *index != layer.index {
                        let look = run.timeline.look(layer.index);
                        let probe = Probe::draw(look, bw, bh, WEAR_SCREEN[1], false);
                        *index = layer.index;
                        *texels = probe
                            .pixels
                            .chunks_exact(4)
                            .zip(&probe.density)
                            .map(|(p, &d)| {
                                [p[2], p[1], p[0]]
                                    .map(|v| f32::from(v) / 255.0)
                                    .into_iter()
                                    .chain([d])
                                    .collect::<Vec<f32>>()
                                    .try_into()
                                    .expect("four values")
                            })
                            .collect();
                    }
                    let [sx, sy, sw, sh] = layer.source.pixels();
                    let columns = taps(sx, sw, width, bw);
                    let rows = taps(sy, sh, height, bh);
                    let alpha = layer.alpha;
                    for (cy, &(y0, y1, fy)) in rows.iter().enumerate() {
                        for (cx, &(x0, x1, fx)) in columns.iter().enumerate() {
                            let [a, b, c, d] = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)]
                                .map(|(x, y)| texels[y * bw + x]);
                            let cell = &mut light[cy * width + cx];
                            for k in 0..4 {
                                let top = f64::from(a[k]) * (1.0 - fx) + f64::from(b[k]) * fx;
                                let bottom = f64::from(c[k]) * (1.0 - fx) + f64::from(d[k]) * fx;
                                let value = top * (1.0 - fy) + bottom * fy;
                                cell[k] += (value - cell[k]) * alpha;
                            }
                        }
                    }
                }
                let time = run.timeline.time();
                for cell in 0..width * height {
                    for k in 0..3 {
                        sum[cell][k] += light[cell][k];
                    }
                    if time < FRAME + FIRST_FADE {
                        continue;
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

    // The timeline measures 1.098, 1.101 and 1.057 for red, green and blue. With a 50 s
    // period, giving every keyframe the same seed and pan direction measured 6.95, 6.88 and 3.26.
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

    // The timeline measures 96 s. Giving every keyframe the same seed and pan direction leaves
    // a cell outside the masses for the whole six hours.
    #[test]
    fn no_cell_stays_dark_for_long() {
        let longest = wear().longest_dark;
        assert!(
            longest < 240.0,
            "a cell stayed outside the masses for {longest:.0} s"
        );
    }
}
