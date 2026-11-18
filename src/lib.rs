//! CPU renderer for slowly flowing, glowing colour ribbons on a near-black background.
//!
//! The pattern lives on a torus of period 1 in both axes. The screen is a small window onto it
//! that drifts across the torus, so every screen region sees the same mix of light and colour.

/// Width of the shared-memory buffer in pixels. The compositor scales it to the screen.
pub const BUFFER_WIDTH: usize = 320;

/// Buffer height that keeps the aspect ratio of a surface of the given logical size.
pub fn buffer_height(logical_width: u32, logical_height: u32) -> usize {
    let height = BUFFER_WIDTH as f64 * f64::from(logical_height) / f64::from(logical_width.max(1));
    (height.round() as usize).max(1)
}

/// Width of the visible window in torus units.
const VIEW_WIDTH: f32 = 0.34;

// Every spatial frequency below is a whole number of turns per torus period. That makes the
// pattern tile the torus exactly, so the drift offset can wrap at 1.0 without a visible seam.
const WARP_STEPS: usize = 4;
const WARP_FREQ: f32 = 3.0;
const WARP_AMP: f32 = 0.035;
const RIBBON: [f32; 2] = [3.0, 11.0];
const SECOND: [f32; 2] = [-4.0, 9.0];
const MASK: [f32; 2] = [2.0, 3.0];
const HUE: [f32; 2] = [2.0, 4.0];
const SECOND_HUE_SHIFT: f32 = 0.3;

// Rates are in turns per second. Each one has the same sign as the drift's contribution to that
// term at a fixed pixel. Opposite signs nearly cancel, and a term that barely moves on screen
// leaves a lasting bright or dark patch that the drift does not wash out.
const WARP_RATE: [[f64; 2]; WARP_STEPS] = [
    [0.0061, 0.0031],
    [0.0083, 0.0069],
    [0.0107, 0.0093],
    [0.0131, 0.0121],
];
const RIBBON_RATE: f64 = 0.0197;
const SECOND_RATE: f64 = 0.0181;
const MASK_RATE: f64 = 0.0093;
const HUE_RATE: f64 = 1.0 / 190.0;
const WARP_AMP_RATE: f64 = 1.0 / 83.0;

/// Drift of the visible window in torus units per second.
const DRIFT: [f64; 2] = [0.00107, 0.00063];

/// Background colour, a deep navy close to black.
const BACKGROUND: [f32; 3] = [0.006, 0.006, 0.024];
/// Colour of the dim glow around the ribbons, an indigo.
const GLOW: [f32; 3] = [0.05, 0.02, 0.15];
const RIBBON_GAIN: f32 = 0.95;
const SECOND_GAIN: f32 = 0.95;
const WHITE_GAIN: f32 = 0.25;

/// Palette per channel as a two-harmonic Fourier series of the hue angle: base, then the cosine
/// and sine weights of the first and second harmonic. It runs aqua, blue, violet, magenta, pink
/// and coral, then returns to aqua through magenta, violet and blue. Violets and pinks hold about
/// two thirds of the cycle.
const PALETTE: [[f32; 5]; 3] = [
    [0.6250, -0.5120, -0.1339, -0.1475, -0.0476],
    [0.3833, 0.2334, -0.0433, 0.2325, 0.1573],
    [0.7525, 0.2896, 0.1644, -0.0692, -0.1256],
];

const ROUND_MAGIC: f32 = 12_582_912.0;

/// Sine of an angle in turns, accurate to about 1e-3.
///
/// It has no branches or calls so the pixel loop vectorizes on baseline x86-64.
#[inline(always)]
fn sin_turns(turns: f32) -> f32 {
    // Adding and subtracting 1.5 * 2^23 rounds to the nearest integer without SSE4.1.
    let r = turns - ((turns + ROUND_MAGIC) - ROUND_MAGIC);
    let y = 8.0 * r - 16.0 * r * r.abs();
    0.225 * (y * y.abs() - y) + y
}

#[inline(always)]
fn cos_turns(turns: f32) -> f32 {
    sin_turns(turns + 0.25)
}

/// Fractional part of `rate * time + start`.
///
/// Phases are reduced in f64 before they reach the f32 pixel loop. An f32 product of a rate and
/// a time of several days loses most of its fractional bits, which would make the motion stutter.
fn phase(time: f64, rate: f64, start: f64) -> f32 {
    (time * rate + start).rem_euclid(1.0) as f32
}

/// Cosine and sine of a hue angle and of twice that angle.
#[derive(Clone, Copy)]
struct Harmonics {
    c1: f32,
    s1: f32,
    c2: f32,
    s2: f32,
}

impl Harmonics {
    #[inline(always)]
    fn new(c1: f32, s1: f32) -> Self {
        Self {
            c1,
            s1,
            c2: c1 * c1 - s1 * s1,
            s2: 2.0 * c1 * s1,
        }
    }

    /// The same harmonics for the angle plus the one whose cosine and sine are `by`.
    #[inline(always)]
    fn rotate(self, by: [f32; 2]) -> Self {
        Self::new(
            self.c1 * by[0] - self.s1 * by[1],
            self.s1 * by[0] + self.c1 * by[1],
        )
    }

    #[inline(always)]
    fn palette(self, channel: usize) -> f32 {
        let [base, c1, s1, c2, s2] = PALETTE[channel];
        base + c1 * self.c1 + s1 * self.s1 + c2 * self.c2 + s2 * self.s2
    }
}

/// Everything that depends on time, reduced to small f32 values once per frame.
struct FrameParams {
    offset: [f32; 2],
    warp_amp: f32,
    warp_phase: [[f32; 2]; WARP_STEPS],
    ribbon_phase: f32,
    second_phase: f32,
    mask_phase: f32,
    hue_phase: f32,
}

impl FrameParams {
    fn at(time: f64) -> Self {
        let breathe = (f64::from(phase(time, WARP_AMP_RATE, 0.0)) * std::f64::consts::TAU).sin();
        Self {
            offset: std::array::from_fn(|axis| phase(time, DRIFT[axis], 0.0)),
            warp_amp: WARP_AMP * (1.0 + 0.25 * breathe as f32),
            warp_phase: std::array::from_fn(|step| {
                std::array::from_fn(|axis| {
                    phase(time, WARP_RATE[step][axis], 0.17 * (step * 2 + axis) as f64)
                })
            }),
            ribbon_phase: phase(time, RIBBON_RATE, 0.0),
            second_phase: phase(time, SECOND_RATE, 0.5),
            mask_phase: phase(time, MASK_RATE, 0.25),
            hue_phase: phase(time, HUE_RATE, 0.0),
        }
    }
}

/// Converts a channel value in 0..=1 to its 8-bit code.
#[inline(always)]
fn channel_byte(value: f32) -> u32 {
    // Adding 2^23 leaves the rounded code in the low mantissa bits. A float to integer cast
    // saturates in Rust and does not vectorize on baseline x86-64.
    (value.clamp(0.0, 1.0) * 255.0 + 8_388_608.0).to_bits() & 0xff
}

/// Renders frames of a fixed size into XRGB8888 pixels.
pub struct Renderer {
    width: usize,
    height: usize,
    columns: Vec<f32>,
    qx: Vec<f32>,
    qy: Vec<f32>,
}

impl Renderer {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            columns: vec![0.0; width],
            qx: vec![0.0; width],
            qy: vec![0.0; width],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Size in bytes of one frame.
    pub fn frame_len(&self) -> usize {
        self.width * self.height * 4
    }

    /// Renders the frame at `time` seconds into `out` as little-endian XRGB8888.
    pub fn render(&mut self, time: f64, out: &mut [u8]) {
        assert_eq!(out.len(), self.frame_len(), "frame buffer size");
        let p = FrameParams::at(time);
        let scale = VIEW_WIDTH / self.width as f32;
        for (x, column) in self.columns.iter_mut().enumerate() {
            *column = (x as f32 + 0.5) * scale + p.offset[0];
        }
        let amp = p.warp_amp;
        let second_shift = [cos_turns(SECOND_HUE_SHIFT), sin_turns(SECOND_HUE_SHIFT)];

        // Each stage runs over a whole row before the next one starts. Short loop bodies with
        // independent iterations let the CPU overlap the long chain of dependent warp steps.
        for (y, row) in out.chunks_exact_mut(self.width * 4).enumerate() {
            let py = (y as f32 + 0.5) * scale + p.offset[1];
            let first_x = amp * sin_turns(WARP_FREQ * py + p.warp_phase[0][0]);
            let first_y = p.warp_phase[0][1];
            for ((qx, qy), &px) in self.qx.iter_mut().zip(&mut self.qy).zip(&self.columns) {
                *qx = px + first_x;
                *qy = py + amp * cos_turns(WARP_FREQ * *qx + first_y);
            }

            for (step, &[phase_x, phase_y]) in p.warp_phase.iter().enumerate().skip(1) {
                let k = (step + 1) as f32;
                let (a, f) = (amp / k, WARP_FREQ * k);
                for (qx, qy) in self.qx.iter_mut().zip(&mut self.qy) {
                    *qx += a * sin_turns(f * *qy + phase_x);
                    *qy += a * cos_turns(f * *qx + phase_y);
                }
            }

            for ((pixel, &qx), &qy) in row.chunks_exact_mut(4).zip(&self.qx).zip(&self.qy) {
                let band = 0.5 + 0.5 * sin_turns(RIBBON[0] * qx + RIBBON[1] * qy + p.ribbon_phase);
                let band2 = band * band;
                let other = 0.5 + 0.5 * sin_turns(SECOND[0] * qx + SECOND[1] * qy + p.second_phase);
                let other2 = other * other;
                let mask = 0.5 + 0.5 * sin_turns(MASK[0] * qx + MASK[1] * qy + p.mask_phase);
                let (near, far) = (mask * mask, (1.0 - mask) * (1.0 - mask));
                let main = RIBBON_GAIN * band2 * band2 * near;
                let second = SECOND_GAIN * other2 * other2 * far;
                let glow = band2 * near + other2 * far;
                let core = main + second;
                let core2 = core * core;
                let white = WHITE_GAIN * core2 * core2;

                let hue = HUE[0] * qx + HUE[1] * qy + p.hue_phase;
                let first = Harmonics::new(cos_turns(hue), sin_turns(hue));
                let shifted = first.rotate(second_shift);

                let [r, g, b] = std::array::from_fn(|channel| {
                    channel_byte(
                        BACKGROUND[channel]
                            + main * first.palette(channel)
                            + second * shifted.palette(channel)
                            + glow * GLOW[channel]
                            + white,
                    )
                });
                pixel.copy_from_slice(&((r << 16) | (g << 8) | b).to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    /// Grid for the long simulations. It shows the same view as the real buffer at lower detail.
    const GRID: (usize, usize) = (64, 27);
    const FRAME_STEP: f64 = 1.0 / 30.0;

    fn render(renderer: &mut Renderer, time: f64) -> Vec<u8> {
        let mut frame = vec![0; renderer.frame_len()];
        renderer.render(time, &mut frame);
        frame
    }

    /// Red, green and blue of every pixel in 0..=1.
    fn colours(frame: &[u8]) -> impl Iterator<Item = [f64; 3]> + '_ {
        frame
            .chunks_exact(4)
            .map(|p| [p[2], p[1], p[0]].map(|v| f64::from(v) / 255.0))
    }

    struct Wear {
        mean: Vec<[f64; 3]>,
        deviation: Vec<[f64; 3]>,
    }

    /// Per-pixel mean and standard deviation of each channel over 8 simulated hours.
    fn wear() -> &'static Wear {
        static WEAR: OnceLock<Wear> = OnceLock::new();
        WEAR.get_or_init(|| {
            let mut renderer = Renderer::new(GRID.0, GRID.1);
            let frames = 7200;
            let mut sum = vec![[0.0; 3]; GRID.0 * GRID.1];
            let mut squares = vec![[0.0; 3]; GRID.0 * GRID.1];
            for i in 0..frames {
                let frame = render(&mut renderer, f64::from(i) * 4.0);
                for ((s, q), rgb) in sum.iter_mut().zip(&mut squares).zip(colours(&frame)) {
                    for c in 0..3 {
                        s[c] += rgb[c];
                        q[c] += rgb[c] * rgb[c];
                    }
                }
            }
            let n = f64::from(frames);
            let mean: Vec<[f64; 3]> = sum.iter().map(|s| s.map(|v| v / n)).collect();
            let deviation = mean
                .iter()
                .zip(&squares)
                .map(|(m, q)| std::array::from_fn(|c| (q[c] / n - m[c] * m[c]).max(0.0).sqrt()))
                .collect();
            Wear { mean, deviation }
        })
    }

    fn max_over_min(values: impl Iterator<Item = f64>) -> f64 {
        let (min, max) = values.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
        max / min
    }

    #[test]
    fn buffer_keeps_the_screen_aspect_ratio() {
        assert_eq!(buffer_height(3413, 1440), 135);
        assert_eq!(buffer_height(1280, 720), 180);
    }

    // The tuned renderer measures 1.10, 1.09 and 1.07 for red, green and blue. Darkening the
    // corners by 10% raises that to 1.19, 1.17 and 1.15.
    #[test]
    fn every_region_wears_evenly() {
        let wear = wear();
        for channel in 0..3 {
            let ratio = max_over_min(wear.mean.iter().map(|m| m[channel]));
            assert!(
                ratio < 1.13,
                "channel {channel}: time-averaged max/min {ratio:.3}"
            );
        }
    }

    // The tuned renderer measures a minimum of 0.14 on green.
    #[test]
    fn no_pixel_holds_a_colour() {
        let wear = wear();
        for channel in 0..3 {
            let (pixel, deviation) = wear
                .deviation
                .iter()
                .map(|d| d[channel])
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .expect("grid is not empty");
            assert!(
                deviation > 0.08,
                "channel {channel}: pixel {pixel} has deviation {deviation:.4}"
            );
        }
    }

    // Over these frames the tuned renderer has a near-black share of 0.46 to 0.58 and a bright
    // share of 0.10 to 0.18.
    #[test]
    fn frames_are_mostly_dark_with_bright_ribbons() {
        let mut renderer = Renderer::new(BUFFER_WIDTH, buffer_height(3413, 1440));
        let pixels = (renderer.width() * renderer.height()) as f64;
        let peak = |rgb: [f64; 3]| rgb[0].max(rgb[1]).max(rgb[2]);
        for i in 0..20 {
            let time = 13.0 + f64::from(i) * 90.0;
            let frame = render(&mut renderer, time);
            let dark = colours(&frame).filter(|&rgb| peak(rgb) < 0.12).count() as f64 / pixels;
            let bright = colours(&frame).filter(|&rgb| peak(rgb) > 0.6).count() as f64 / pixels;
            assert!(
                dark > 0.4,
                "at {time} s only {dark:.3} of the frame is near black"
            );
            assert!(
                (0.05..0.25).contains(&bright),
                "at {time} s the bright share is {bright:.3}"
            );
        }
    }

    /// Mean absolute byte difference between consecutive frames, for `count` frames from `start`.
    fn consecutive_differences(start: f64, count: u32) -> Vec<f64> {
        let mut renderer = Renderer::new(BUFFER_WIDTH, buffer_height(3413, 1440));
        let mut previous = render(&mut renderer, start);
        (1..=count)
            .map(|i| {
                let frame = render(&mut renderer, start + f64::from(i) * FRAME_STEP);
                let total: u64 = frame
                    .iter()
                    .zip(&previous)
                    .map(|(&a, &b)| u64::from(a.abs_diff(b)))
                    .sum();
                previous = frame;
                total as f64 / previous.len() as f64
            })
            .collect()
    }

    /// Mean frame-to-frame difference near `start`, and the worst max/min ratio of consecutive
    /// differences inside one window. A clock that has lost precision moves in uneven steps.
    fn motion_near(start: f64) -> (f64, f64) {
        let windows: Vec<Vec<f64>> = (0..6)
            .map(|k| consecutive_differences(start + f64::from(k) * 37.0, 10))
            .collect();
        let count = windows.iter().map(Vec::len).sum::<usize>() as f64;
        let mean = windows.iter().flatten().sum::<f64>() / count;
        let unevenness = windows
            .iter()
            .map(|w| max_over_min(w.iter().copied()))
            .fold(f64::MIN, f64::max);
        (mean, unevenness)
    }

    // The tuned renderer measures means of 0.213 at both times and an unevenness of 1.015.
    #[test]
    fn motion_after_three_days_matches_motion_after_ten_seconds() {
        let (early_mean, early_unevenness) = motion_near(10.0);
        let (late_mean, late_unevenness) = motion_near(259_200.0);
        let ratio = late_mean / early_mean;
        assert!(
            (0.85..1.18).contains(&ratio),
            "mean difference {late_mean:.4} after three days, {early_mean:.4} after ten seconds"
        );
        assert!(
            early_unevenness < 1.2,
            "uneven steps after ten seconds: {early_unevenness:.3}"
        );
        assert!(
            late_unevenness < 1.2,
            "uneven steps after three days: {late_unevenness:.3}"
        );
    }
}
