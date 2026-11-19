//! Inigo Quilez's nested domain warping, drawn on the CPU row by row.
//!
//! `f(p) = fbm(p + K*r)`, `r = fbm2(p + K*q)` and `q = fbm2(p)`, as in
//! <https://iquilezles.org/articles/warp/>. The warp vectors colour the field, the gradient of
//! `f` shades it, and a threshold on `f` cuts it into crisp masses with a rim and a glow.

use std::sync::OnceLock;

use crate::Rng;

const LANES: usize = 8;
type Lanes = [f32; LANES];

/// Field units spanned by the screen height.
const FIELD_HEIGHT: f64 = 0.8;
/// `K` in `f(p + K*r)`.
const WARP: f32 = 3.5;
const OCTAVES_Q: usize = 2;
const OCTAVES_R: usize = 3;
const OCTAVES_F: usize = 8;
/// The first octaves of `f`, which a supersampled keyframe draws at every other buffer pixel.
/// Their lattice cells are at least 211 samples wide before the warp, so the cubic between the
/// pixels misses them by far less than the width of a mass edge. The other octaves are drawn at
/// every buffer pixel, which is every other sample.
const LOW_OCTAVES: usize = 4;
/// Widest spacing of the warp nodes in screen heights. It is the spacing of the reference at
/// 480x203, and twice that fails the parity check against it.
const WARP_SPACING: f64 = 2.0 / 203.0;
/// Warp node rows kept, which covers the rows of a sample row and of the pixel rows around it.
const WARP_ROWS: usize = 8;
const LACUNARITY: f32 = 2.02;
const Q_OFFSET: [f32; 2] = [5.2, 1.3];
const RX_OFFSET: [f32; 2] = [1.7, 9.2];
const RY_OFFSET: [f32; 2] = [8.3, 2.8];
/// `f` above this is inside a coloured mass.
const THRESHOLD: f32 = -0.1;
/// Half-width of the edge ramp in `f` units.
const EDGE: f32 = 0.012;
const HUE_LO: f32 = -0.25;
const HUE_HI: f32 = 0.28;
/// Largest normalized hue, which keeps the palette position below the last stop.
const HUE_TOP: f32 = 0.9999;
const STOPS: [[f32; 3]; 6] = [
    rgb(0x12f4e6),
    rgb(0x1e7bff),
    rgb(0x7a2cff),
    rgb(0xff2bb4),
    rgb(0xff6a3c),
    rgb(0xff2bb4),
];
const BACKGROUND: [f32; 3] = rgb(0x0a0a26);
const SHADE_WEIGHT: f32 = 0.3;
const RIM: f32 = 0.35;
const GLOW: f32 = 0.3;
const LIGHT: [f32; 3] = [0.5, 0.6, 0.62];
const SHADE_SLOPE: f32 = 0.02;
/// Stars per pixel of a screen 810 pixels high. The count scales with the screen area.
const STARS_PER_PIXEL_AT_810: f64 = 0.0004;
const STAR_COLOUR: [f32; 3] = [0.85, 0.9, 1.0];
const STAR_SALT: u64 = 0x5354_4152_5345_4544;
/// Field samples per buffer pixel along each axis when the buffer is smaller than the area it
/// covers on screen.
pub const SUPERSAMPLE: usize = 2;

const fn rgb(hex: u32) -> [f32; 3] {
    [
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    ]
}

/// The parameters that make one keyframe's image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub seed: u32,
    /// Shift in `0.0..2.0` of the normalized hue, applied as a triangle wave.
    pub hue_shift: f32,
}

const GRADIENT_COUNT: usize = 1 << 16;
type Gradients = [[f32; 2]; GRADIENT_COUNT];

/// Cosine and sine of the gradient angle for every 16-bit hash.
fn gradients() -> &'static Gradients {
    static TABLE: OnceLock<Box<Gradients>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let step = (2.0 * std::f64::consts::PI / GRADIENT_COUNT as f64) as f32;
        let table: Vec<[f32; 2]> = (0..GRADIENT_COUNT)
            .map(|k| {
                let angle = k as f32 * step;
                [angle.cos(), angle.sin()]
            })
            .collect();
        table.into_boxed_slice().try_into().expect("table length")
    })
}

/// Coefficients in `r * r` of `sin(r * PI / 2) / r` and `cos(r * PI / 2)` for `r` in `0.0..=1.0`.
const SINE: [f32; 5] = [
    std::f32::consts::FRAC_PI_2,
    -0.645_963_4,
    0.079_688_48,
    -0.004_672_228,
    0.000_150_820_46,
];
const COSINE: [f32; 5] = [
    0.999_999_94,
    -1.233_698_2,
    0.253_650_75,
    -0.020_810_626,
    0.000_858_190_06,
];

/// Cosine and sine of the gradient angle of a hash, within 1e-6 of the table's. It has no table
/// lookup and no branch, so a loop over a row of hashes compiles to vector code.
#[inline(always)]
fn direction(hash: u32) -> (f32, f32) {
    let r = (hash & 0x3FFF) as i32 as f32 * (1.0 / 16384.0);
    let t = r * r;
    // Fused multiply-adds halve the operations of both polynomials. The build targets a CPU
    // that has them.
    let horner = |c: [f32; 5]| {
        t.mul_add(
            t.mul_add(t.mul_add(t.mul_add(c[4], c[3]), c[2]), c[1]),
            c[0],
        )
    };
    let sine = r * horner(SINE);
    let cosine = horner(COSINE);
    let quadrant = (hash >> 14) & 3;
    // All ones in the odd quadrants, where sine and cosine trade places.
    let odd = 0u32.wrapping_sub(quadrant & 1);
    let (s, c) = (sine.to_bits(), cosine.to_bits());
    (
        f32::from_bits(((c & !odd) | (s & odd)) ^ ((quadrant.wrapping_add(1) & 2) << 30)),
        f32::from_bits(((s & !odd) | (c & odd)) ^ ((quadrant & 2) << 30)),
    )
}

const ROUND_MAGIC: f32 = 12_582_912.0;

/// Lattice cell and fractional position of a coordinate, for `|x| < 2^22`.
///
/// It has no calls so the noise vectorizes on baseline x86-64, which lacks a floor instruction.
#[inline(always)]
fn split(x: f32) -> (u32, f32) {
    let shifted = x + ROUND_MAGIC;
    let rounded = shifted - ROUND_MAGIC;
    let above = rounded > x;
    let cell = shifted
        .to_bits()
        .wrapping_sub(ROUND_MAGIC.to_bits())
        .wrapping_sub(u32::from(above));
    let floor = if above { rounded - 1.0 } else { rounded };
    (cell, x - floor)
}

const HASH_X: u32 = 0x8DA6_B343;
const HASH_Y: u32 = 0xD816_3841;

#[inline(always)]
fn finish(mut h: u32) -> u32 {
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^ (h >> 15)
}

/// Hashes of the corners of cell `(ix, iy)`, in the order `(0, 0)`, `(1, 0)`, `(0, 1)`, `(1, 1)`.
#[inline(always)]
fn corner_hashes(ix: u32, iy: u32, seed: u32) -> [u32; 4] {
    let ax = ix.wrapping_mul(HASH_X);
    let ax1 = ax.wrapping_add(HASH_X);
    let by = iy.wrapping_mul(HASH_Y);
    let by1 = by.wrapping_add(HASH_Y) ^ seed;
    let by = by ^ seed;
    [
        finish(ax ^ by),
        finish(ax1 ^ by),
        finish(ax ^ by1),
        finish(ax1 ^ by1),
    ]
}

/// Gradient noise at fractional position `(tx, ty)` in a cell with the given corner gradients.
#[inline(always)]
fn blend(tx: f32, ty: f32, g: [[f32; 2]; 4]) -> f32 {
    let ux = tx * tx * tx * (tx * (tx * 6.0 - 15.0) + 10.0);
    let uy = ty * ty * ty * (ty * (ty * 6.0 - 15.0) + 10.0);
    let a = g[0][0] * tx + g[0][1] * ty;
    let b = g[1][0] * (tx - 1.0) + g[1][1] * ty;
    let c = g[2][0] * tx + g[2][1] * (ty - 1.0);
    let d = g[3][0] * (tx - 1.0) + g[3][1] * (ty - 1.0);
    (a + (b - a) * ux + (c - a) * uy + (a - b - c + d) * ux * uy) * 1.4
}

/// Gradient noise with a quintic fade.
///
/// When every lane falls in the same lattice cell, which is the common case for the coarse
/// octaves, the corner gradients are looked up once. The result is the same either way.
#[inline(always)]
fn noise(x: &Lanes, y: &Lanes, seed: u32, table: &Gradients) -> Lanes {
    let mut ix = [0u32; LANES];
    let mut iy = [0u32; LANES];
    let mut fx = [0.0; LANES];
    let mut fy = [0.0; LANES];
    for l in 0..LANES {
        (ix[l], fx[l]) = split(x[l]);
        (iy[l], fy[l]) = split(y[l]);
    }
    let shared = (0..LANES).fold(true, |same, l| same & (ix[l] == ix[0]) & (iy[l] == iy[0]));
    let mut out = [0.0; LANES];
    if shared {
        // Masking at the lookup lets the compiler drop the bounds check.
        let g = corner_hashes(ix[0], iy[0], seed).map(|hash| table[(hash & 0xFFFF) as usize]);
        for l in 0..LANES {
            out[l] = blend(fx[l], fy[l], g);
        }
        return out;
    }
    let mut corner = [[0u32; LANES]; 4];
    for l in 0..LANES {
        let ax = ix[l].wrapping_mul(HASH_X);
        let ax1 = ax.wrapping_add(HASH_X);
        let by = iy[l].wrapping_mul(HASH_Y);
        let by1 = by.wrapping_add(HASH_Y) ^ seed;
        let by = by ^ seed;
        corner[0][l] = finish(ax ^ by);
        corner[1][l] = finish(ax1 ^ by);
        corner[2][l] = finish(ax ^ by1);
        corner[3][l] = finish(ax1 ^ by1);
    }
    let mut g = [[[0.0f32; 2]; LANES]; 4];
    for (g, corner) in g.iter_mut().zip(&corner) {
        for (g, &hash) in g.iter_mut().zip(corner) {
            *g = table[(hash & 0xFFFF) as usize];
        }
    }
    for l in 0..LANES {
        out[l] = blend(fx[l], fy[l], [g[0][l], g[1][l], g[2][l], g[3][l]]);
    }
    out
}

/// The positions of a row of points and the scratch rows [`fbm_row`] needs for them.
struct NoiseRow {
    x: Vec<f32>,
    y: Vec<f32>,
    fx: Vec<f32>,
    fy: Vec<f32>,
    ax: Vec<u32>,
    by: Vec<u32>,
    hashes: Vec<u32>,
    dots: [Vec<f32>; 4],
}

impl NoiseRow {
    fn new(length: usize) -> Self {
        Self {
            x: vec![0.0; length],
            y: vec![0.0; length],
            fx: vec![0.0; length],
            fy: vec![0.0; length],
            ax: vec![0; length],
            by: vec![0; length],
            hashes: vec![0; length],
            dots: std::array::from_fn(|_| vec![0.0; length]),
        }
    }
}

/// Adds `octaves` noise layers at the first `total.len()` positions of `row` to `total`, the
/// first one with the given amplitude, and leaves the positions turned and scaled for the next
/// octave. It matches [`fbm`] to within the error of [`direction`].
///
/// Every step is one loop over the whole row with no table lookup, so each loop compiles to
/// vector code at the full register width.
fn fbm_row(octaves: usize, seed: u32, mut amplitude: f32, total: &mut [f32], row: &mut NoiseRow) {
    let n = total.len();
    let (xs, ys) = (&mut row.x[..n], &mut row.y[..n]);
    let (fxs, fys) = (&mut row.fx[..n], &mut row.fy[..n]);
    let (axs, bys) = (&mut row.ax[..n], &mut row.by[..n]);
    for _ in 0..octaves {
        for (((((&x, &y), fx), fy), ax), by) in xs
            .iter()
            .zip(ys.iter())
            .zip(fxs.iter_mut())
            .zip(fys.iter_mut())
            .zip(axs.iter_mut())
            .zip(bys.iter_mut())
        {
            let (ix, tx) = split(x);
            let (iy, ty) = split(y);
            (*fx, *fy) = (tx, ty);
            (*ax, *by) = (ix.wrapping_mul(HASH_X), iy.wrapping_mul(HASH_Y));
        }
        for (corner, dots) in row.dots.iter_mut().enumerate() {
            let (right, down) = (corner & 1 != 0, corner & 2 != 0);
            let (step_x, step_y) = (
                if right { HASH_X } else { 0 },
                if down { HASH_Y } else { 0 },
            );
            let (dx, dy) = (f32::from(u8::from(right)), f32::from(u8::from(down)));
            // The hashes get a loop of their own. One loop for the whole corner is a chain of
            // about 60 cycles per step, and the processor can only work on three such steps at
            // once.
            let hashes = &mut row.hashes[..n];
            for ((hash, &ax), &by) in hashes.iter_mut().zip(axs.iter()).zip(bys.iter()) {
                *hash = finish(ax.wrapping_add(step_x) ^ by.wrapping_add(step_y) ^ seed);
            }
            for (((dot, &hash), &fx), &fy) in dots[..n]
                .iter_mut()
                .zip(hashes.iter())
                .zip(fxs.iter())
                .zip(fys.iter())
            {
                let (gx, gy) = direction(hash);
                *dot = gx.mul_add(fx - dx, gy * (fy - dy));
            }
        }
        let [a, b, c, d] = &row.dots;
        for ((((((total, &a), &b), &c), &d), &tx), &ty) in total
            .iter_mut()
            .zip(&a[..n])
            .zip(&b[..n])
            .zip(&c[..n])
            .zip(&d[..n])
            .zip(fxs.iter())
            .zip(fys.iter())
        {
            let fade = |t: f32| t * t * t * t.mul_add(t.mul_add(6.0, -15.0), 10.0);
            let (ux, uy) = (fade(tx), fade(ty));
            let top = (b - a).mul_add(ux, a);
            let bottom = (d - c).mul_add(ux, c);
            let noise = (bottom - top).mul_add(uy, top);
            *total = (amplitude * 1.4).mul_add(noise, *total);
        }
        for (x, y) in xs.iter_mut().zip(ys.iter_mut()) {
            (*x, *y) = (
                (0.8 * *x - 0.6 * *y) * LACUNARITY,
                (0.6 * *x + 0.8 * *y) * LACUNARITY,
            );
        }
        amplitude *= 0.5;
    }
}

/// Fractal sum of `OCTAVES` noise layers, each rotated and scaled from the one before.
#[inline(always)]
fn fbm<const OCTAVES: usize>(mut x: Lanes, mut y: Lanes, seed: u32, table: &Gradients) -> Lanes {
    let mut total = [0.0; LANES];
    let mut amplitude = 0.5;
    for _ in 0..OCTAVES {
        let n = noise(&x, &y, seed, table);
        for l in 0..LANES {
            total[l] += amplitude * n[l];
            let (rx, ry) = (0.8 * x[l] - 0.6 * y[l], 0.6 * x[l] + 0.8 * y[l]);
            x[l] = rx * LACUNARITY;
            y[l] = ry * LACUNARITY;
        }
        amplitude *= 0.5;
    }
    total
}

#[inline(always)]
fn offset(base: &Lanes, warp: &Lanes, shift: f32) -> Lanes {
    std::array::from_fn(|l| base[l] + WARP * warp[l] + shift)
}

#[inline(always)]
fn smoothstep(low: f32, high: f32, x: f32) -> f32 {
    // The compiler folds the reciprocal of constant bounds, and a multiplication is cheaper
    // than a division.
    let t = ((x - low) * (1.0 / (high - low))).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Midpoint of `b` and `c` on the cubic through `a`, `b`, `c` and `d` at equal spacing.
#[inline(always)]
fn midpoint(a: f32, b: f32, c: f32, d: f32) -> f32 {
    (9.0 * (b + c) - (a + d)) * (1.0 / 16.0)
}

/// Converts a channel value to its 8-bit code, rounding down like a cast of `value * 255`.
#[inline(always)]
fn channel_byte(value: f32) -> u32 {
    // Adding 2^23 leaves the rounded code in the low mantissa bits. A float to integer cast
    // saturates in Rust and does not vectorize on baseline x86-64.
    let scaled = value.clamp(0.0, 1.0) * 255.0;
    let shifted = scaled + 8_388_608.0;
    let floor = if shifted - 8_388_608.0 > scaled {
        shifted - 1.0
    } else {
        shifted
    };
    floor.to_bits() & 0xff
}

/// The value between `b` and `c` on the cubic through `a`, `b`, `c` and `d` at equal spacing,
/// with the given weights. Halfway it is the [`midpoint`], which the reference uses.
#[inline(always)]
fn between(halfway: bool, weights: [f32; 4], a: f32, b: f32, c: f32, d: f32) -> f32 {
    if halfway {
        midpoint(a, b, c, d)
    } else {
        weights[0] * a + weights[1] * b + weights[2] * c + weights[3] * d
    }
}

/// Position along the palette of a hue, from 0 at the first stop to the last stop's index.
#[inline(always)]
fn palette_position(hue: f32, hue_shift: f32) -> f32 {
    let normalized = ((hue - HUE_LO) * (1.0 / (HUE_HI - HUE_LO))).clamp(0.0, HUE_TOP);
    let wrapped = normalized + hue_shift;
    let wrapped = if wrapped >= 2.0 {
        wrapped - 2.0
    } else {
        wrapped
    };
    let shifted = 1.0 - (wrapped - 1.0).abs();
    shifted.min(HUE_TOP) * (STOPS.len() - 1) as f32
}

fn lanes(values: &[f32]) -> &Lanes {
    values.try_into().expect("a chunk is LANES long")
}

fn lanes_mut(values: &mut [f32]) -> &mut Lanes {
    values.try_into().expect("a chunk is LANES long")
}

/// The warp vectors `qx`, `qy`, `rx` and `ry` at every `grid` columns of one row. Node `i` sits
/// at column `grid * (i - 1)`, so the nodes reach past both edges.
struct WarpRow {
    row: isize,
    values: [Vec<f32>; 4],
}

/// Samples the prefilter weighs for one buffer pixel along each axis.
const TAPS: usize = 6;
/// Weights of those samples: a tent three buffer pixels wide, centred between the pixel's own two
/// samples. The compositor's bilinear filter turns any detail near one buffer pixel wide into
/// beads along diagonal streaks, so the weights reach into the neighbouring pixels to remove it.
const PREFILTER: [f32; TAPS] = [
    1.0 / 18.0,
    3.0 / 18.0,
    5.0 / 18.0,
    5.0 / 18.0,
    3.0 / 18.0,
    1.0 / 18.0,
];

/// Turns rows of samples, [`SUPERSAMPLE`] per buffer pixel along each axis, into buffer rows.
struct Prefilter {
    /// Samples per row.
    width: usize,
    /// Rows of samples.
    height: usize,
    /// The last [`TAPS`] sample rows, weighed along the row, by row index modulo [`TAPS`].
    rows: [[Vec<f32>; 3]; TAPS],
    /// One channel of a sample row with its edge samples repeated on both sides.
    padded: Vec<f32>,
    /// Sample rows received.
    pushed: usize,
}

impl Prefilter {
    fn new(width: usize, out_stride: usize, height: usize) -> Self {
        Self {
            width,
            height,
            rows: std::array::from_fn(|_| std::array::from_fn(|_| vec![0.0; out_stride])),
            padded: vec![0.0; out_stride * SUPERSAMPLE + TAPS],
            pushed: 0,
        }
    }

    /// The last sample row that buffer row `y` needs.
    fn last_row(&self, y: usize) -> usize {
        (SUPERSAMPLE * y + TAPS / 2).min(self.height - 1)
    }

    /// Takes the next sample row. Each sample is clamped first, like the screen would clamp it.
    fn push(&mut self, light: &[Vec<f32>; 3]) {
        let reach = TAPS / 2 - 1;
        let last = self.width - 1;
        for (row, light) in self.rows[self.pushed % TAPS].iter_mut().zip(light) {
            // The inside is a plain copy so it compiles to vector code, and the edge samples
            // repeat on both sides.
            let inside = &mut self.padded[reach..=reach + last];
            for (padded, &light) in inside.iter_mut().zip(&light[..=last]) {
                *padded = light.clamp(0.0, 1.0);
            }
            let (first, end) = (self.padded[reach], self.padded[reach + last]);
            self.padded[..reach].fill(first);
            self.padded[reach + last + 1..].fill(end);
            // The weights are the same from both ends, so each one multiplies a sum of two
            // samples. Walking the row in pairs lets the loop vectorize.
            let (pairs, _) = self.padded.as_chunks::<SUPERSAMPLE>();
            let [outer, middle, inner, ..] = PREFILTER;
            for (out, ((a, b), c)) in row
                .iter_mut()
                .zip(pairs.iter().zip(&pairs[1..]).zip(&pairs[2..]))
            {
                *out = outer * (a[0] + c[1]) + middle * (a[1] + c[0]) + inner * (b[0] + b[1]);
            }
        }
        self.pushed += 1;
    }

    /// Writes buffer row `y` once [`Prefilter::last_row`] of it has been pushed.
    fn row(&self, y: usize, out: &mut [Vec<f32>; 3]) {
        let first = (SUPERSAMPLE * y + 1) as isize - (TAPS / 2) as isize;
        for (channel, out) in out.iter_mut().enumerate() {
            out.fill(0.0);
            for (tap, weight) in PREFILTER.into_iter().enumerate() {
                let row = (first + tap as isize).clamp(0, self.height as isize - 1) as usize;
                for (out, value) in out.iter_mut().zip(&self.rows[row % TAPS][channel]) {
                    *out += value * weight;
                }
            }
        }
    }
}

/// Rows of values at every other row and column of a finer grid, and the cubic midpoints that
/// fill the finer grid from them.
struct Midpoints {
    /// The last rows by row index modulo 4, each with its row index. Node `i` of a row sits at
    /// column `2 * i - 2` of the finer grid.
    rows: [(isize, Vec<f32>); 4],
    /// Rows blended for an odd row of the finer grid.
    blend: Vec<f32>,
}

impl Midpoints {
    /// Rows of `nodes` values for a finer grid twice as wide.
    fn new(nodes: usize) -> Self {
        Self {
            rows: std::array::from_fn(|_| (isize::MIN, vec![0.0; nodes + 3])),
            blend: vec![0.0; nodes + 3],
        }
    }

    fn slot(row: isize) -> usize {
        row.rem_euclid(4) as usize
    }

    fn has(&self, row: isize) -> bool {
        self.rows[Self::slot(row)].0 == row
    }

    /// Stores row `row`. The nodes past its edges continue the line through the last two inside.
    fn put(&mut self, row: isize, inside: &[f32]) {
        let (index, values) = &mut self.rows[Self::slot(row)];
        *index = row;
        let n = inside.len();
        values[1..=n].copy_from_slice(inside);
        values[0] = 2.0 * values[1] - values[2];
        values[n + 1] = 2.0 * values[n] - values[n - 1];
        values[n + 2] = 3.0 * values[n] - 2.0 * values[n - 1];
    }

    /// The rows that row `y` of the finer grid is made from.
    fn needs(y: isize) -> std::ops::RangeInclusive<isize> {
        let row = y.div_euclid(2);
        if y.rem_euclid(2) == 0 {
            row..=row
        } else {
            row - 1..=row + 2
        }
    }

    /// Writes row `y` of the finer grid once the rows it [`needs`](Self::needs) are stored.
    fn fill(&mut self, y: isize, out: &mut [f32]) {
        let row = y.div_euclid(2);
        let values = |k: isize| {
            let (index, values) = &self.rows[Self::slot(k)];
            debug_assert_eq!(*index, k);
            values
        };
        let nodes = if y.rem_euclid(2) == 0 {
            values(row)
        } else {
            let [a, b, c, d] = [row - 1, row, row + 1, row + 2].map(values);
            for (i, out) in self.blend.iter_mut().enumerate() {
                *out = midpoint(a[i], b[i], c[i], d[i]);
            }
            &self.blend
        };
        for (pair, near) in out.chunks_exact_mut(2).zip(nodes.windows(4)) {
            pair[0] = near[1];
            pair[1] = midpoint(near[0], near[1], near[2], near[3]);
        }
    }
}

/// What a supersampled keyframe draws at the buffer pixels instead of at every sample.
struct Reduced {
    /// The low octaves of `f` at every other buffer pixel.
    low: Midpoints,
    /// `f` at every buffer pixel, which is every other sample.
    f: Midpoints,
    /// The hue at every buffer pixel of the last pixel rows, by row index modulo 8.
    hues: [Vec<f32>; 8],
    noise: NoiseRow,
    /// The warp vectors `rx` and `ry`, `f` and the hue of the pixel row being drawn.
    rx: Vec<f32>,
    ry: Vec<f32>,
    row_f: Vec<f32>,
    row_hue: Vec<f32>,
    /// The hue at the warp nodes of the pixel row being drawn.
    node_hue: Vec<f32>,
    /// The turn and scale that the low octaves apply to a position, as a complex factor.
    turn: [f32; 2],
    /// Amplitude of the first octave after the low ones.
    amplitude: f32,
}

/// Writes every `step`th sample of a row from its nodes, which sit `cubic.len()` samples apart.
fn spread(nodes: &[f32], out: &mut [f32], cubic: &[[f32; 4]], step: usize) {
    let grid = cubic.len();
    match grid / step {
        8 => spread_groups::<8>(nodes, out, cubic, step),
        4 => spread_groups::<4>(nodes, out, cubic, step),
        count => {
            // Two samples per node keep the reference's midpoint to the last bit.
            for (group, near) in out.chunks_exact_mut(count).zip(nodes.windows(4)) {
                group[0] = near[1];
                for (index, out) in group.iter_mut().enumerate().skip(1) {
                    let phase = index * step;
                    let weights = cubic[phase];
                    *out = between(
                        2 * phase == grid,
                        weights,
                        near[0],
                        near[1],
                        near[2],
                        near[3],
                    );
                }
            }
        }
    }
}

/// Writes `N` samples per node, each a weighted sum of the four nearest nodes. The weights of a
/// node are laid out by sample so the sum of a group is four vector operations.
fn spread_groups<const N: usize>(nodes: &[f32], out: &mut [f32], cubic: &[[f32; 4]], step: usize) {
    let weights: [[f32; N]; 4] =
        std::array::from_fn(|node| std::array::from_fn(|index| cubic[index * step][node]));
    let (groups, _) = out.as_chunks_mut::<N>();
    for (group, near) in groups.iter_mut().zip(nodes.windows(4)) {
        for (index, out) in group.iter_mut().enumerate() {
            *out = weights[0][index] * near[0]
                + weights[1][index] * near[1]
                + weights[2][index] * near[2]
                + weights[3][index] * near[3];
        }
    }
}

/// Field values of one row.
#[derive(Clone)]
struct FieldRow {
    /// `f` with one ghost column on each side, extrapolated so that a central difference there
    /// equals the one-sided difference at the edge.
    f: Vec<f32>,
    /// Position along the palette, from 0 at the first stop to the last stop's index.
    position: Vec<f32>,
}

#[derive(Clone, Copy)]
struct Star {
    x: i32,
    y: i32,
    brightness: f32,
}

/// Draws a keyframe into a buffer row by row, so the work can be spread across frames.
///
/// The warp vectors vary slowly, so they are evaluated at nodes several samples apart and the
/// samples between lie on the cubic through the four nearest nodes. A keyframe drawn one sample
/// per screen pixel evaluates everything else at every sample. A supersampled keyframe draws
/// `f` and the hue at the buffer pixels, which are every other sample, takes the cubic midpoints
/// between them and evaluates only the shading at every sample. A [`Prefilter`] turns the samples into buffer pixels when there
/// are more samples than pixels.
pub struct Raster {
    /// Samples per row.
    width: usize,
    /// Rows of samples.
    height: usize,
    stride: usize,
    out_width: usize,
    prefilter: Option<Prefilter>,
    /// The buffer row being drawn, per channel, when a prefilter makes it.
    sums: [Vec<f32>; 3],
    /// The palette colour of every sample of the buffer row `coloured`, per channel.
    pixel: [Vec<f32>; 3],
    /// The palette position of each pixel of that row.
    position: Vec<f32>,
    coloured: Option<usize>,
    next_out: usize,
    seed: u32,
    hue_shift: f32,
    /// The gradient table, which only a keyframe drawn one sample per screen pixel uses.
    table: Option<&'static Gradients>,
    /// Scratch rows for the warp nodes of a supersampled keyframe.
    node_noise: Option<NoiseRow>,
    units_per_pixel: f32,
    columns: Vec<f32>,
    node_columns: Vec<f32>,
    /// Factor from a difference of `f` across one pixel to the shading slope.
    slope: f32,
    /// Samples between two warp nodes along each axis.
    grid: usize,
    /// Weights of the four nearest nodes for each sample between two nodes.
    cubic: Vec<[f32; 4]>,
    reduced: Option<Reduced>,
    /// Warp node rows by row index modulo [`WARP_ROWS`].
    warp_rows: [WarpRow; WARP_ROWS],
    /// Warp node values blended between node rows for an odd pixel row.
    blend: [Vec<f32>; 4],
    /// Warp vectors at every pixel of the row being computed.
    warps: [Vec<f32>; 4],
    /// Field rows `y - 1`, `y` and `y + 1` of the next row `y` to shade, by row modulo 3.
    rows: [FieldRow; 3],
    computed: usize,
    next_row: usize,
    stars: Vec<Star>,
    next_star: usize,
    kernel: Vec<f32>,
    reach: i32,
    /// Weights along each axis of the buffer pixels a star is stamped on after the prefilter.
    stamp: [f32; 3],
    /// Sum of a stamped star's buffer pixels for a star of brightness 1.
    stamp_sum: f32,
    next_stamp: usize,
    light: [Vec<f32>; 3],
    density: Vec<f32>,
    /// Luminance, rim and glow of the row being shaded.
    factors: [Vec<f32>; 3],
    /// How far the row being shaded has moved from each palette stop to the next.
    blends: [Vec<f32>; STOPS.len() - 1],
}

impl Raster {
    /// A raster of `width` by `height` buffer pixels for a screen `screen_height` pixels high,
    /// with or without its stars. One buffer pixel spans `scale` screen pixels.
    pub fn new(
        look: Look,
        width: usize,
        height: usize,
        screen_height: u32,
        scale: f64,
        stars: bool,
    ) -> Self {
        assert!(
            width >= 2 && height >= 2,
            "a keyframe needs two rows and columns"
        );
        let samples = if scale > 1.0 { SUPERSAMPLE } else { 1 };
        let out_width = width;
        let out_stride = width.div_ceil(LANES) * LANES;
        let (width, height, stride) = (width * samples, height * samples, out_stride * samples);
        // Screen pixels per sample.
        let step = scale / samples as f64;
        // A keyframe drawn one sample per screen pixel keeps the reference's node spacing.
        let grid = if samples > 1 {
            let widest = WARP_SPACING * f64::from(screen_height) / step;
            if widest >= 8.0 { 8 } else { 4 }
        } else {
            2
        };
        let cubic = (0..grid)
            .map(|phase| {
                let t = phase as f64 / grid as f64;
                [
                    -t * (t - 1.0) * (t - 2.0) / 6.0,
                    (t + 1.0) * (t - 1.0) * (t - 2.0) / 2.0,
                    -(t + 1.0) * t * (t - 2.0) / 2.0,
                    (t + 1.0) * t * (t - 1.0) / 6.0,
                ]
                .map(|w| w as f32)
            })
            .collect();
        let nodes = (stride / grid + 4).div_ceil(LANES) * LANES;
        let reduced = (samples > 1).then(|| {
            let mut turn = [1.0f64, 0.0];
            for _ in 0..LOW_OCTAVES {
                let lacunarity = f64::from(LACUNARITY);
                turn = [
                    (0.8 * turn[0] - 0.6 * turn[1]) * lacunarity,
                    (0.6 * turn[0] + 0.8 * turn[1]) * lacunarity,
                ];
            }
            let half = stride / 2;
            Reduced {
                low: Midpoints::new(half / 2),
                f: Midpoints::new(half),
                hues: std::array::from_fn(|_| vec![0.0; half]),
                noise: NoiseRow::new(half),
                rx: vec![0.0; half],
                ry: vec![0.0; half],
                row_f: vec![0.0; half],
                row_hue: vec![0.0; half],
                node_hue: vec![0.0; nodes],
                turn: turn.map(|t| t as f32),
                amplitude: 0.5f32.powi(LOW_OCTAVES as i32 + 1),
            }
        });
        let nodes = (stride / grid + 4).div_ceil(LANES) * LANES;
        let units_per_pixel = (FIELD_HEIGHT * step / f64::from(screen_height)) as f32;
        let columns = (0..stride).map(|x| x as f32 * units_per_pixel).collect();
        let node_columns = (0..nodes)
            .map(|i| ((i as i32 - 1) * grid as i32) as f32 * units_per_pixel)
            .collect();
        let warp_row = || WarpRow {
            row: isize::MIN,
            values: std::array::from_fn(|_| vec![0.0; nodes]),
        };
        let row = FieldRow {
            f: vec![0.0; stride + 2],
            position: vec![0.0; stride],
        };

        let scale = f64::from(screen_height) / (810.0 * step);
        let star_list = if stars {
            let mut rng = Rng::new(u64::from(look.seed) ^ STAR_SALT);
            let count =
                (width as f64 * height as f64 * STARS_PER_PIXEL_AT_810 / (scale * scale)) as usize;
            let mut list: Vec<Star> = (0..count)
                .map(|_| Star {
                    x: rng.below(width as u32) as i32,
                    y: rng.below(height as u32) as i32,
                    brightness: (rng.unit().powi(4) * 1.8 + 0.3) as f32,
                })
                .collect();
            list.sort_by_key(|star| star.y);
            list
        } else {
            Vec::new()
        };
        // The compositor's bilinear filter spreads a buffer pixel into a tent with a variance of a
        // sixth of a buffer pixel squared, so a stamped star only carries the variance its
        // Gaussian has beyond that. A screen pixel centre lies a quarter of a screen pixel from
        // the star's centre on average along each axis, which lowers the peak of the tent there.
        // The sum splits that loss evenly between the star's peak and its total light on screen.
        let pixel_scale = scale / samples as f64;
        let variance = 0.225 * pixel_scale * pixel_scale - 1.0 / 6.0;
        let side = if variance > 0.0 {
            (-0.5 / variance).exp()
        } else {
            0.0
        };
        let stamp = [side, 1.0, side].map(|w| (w / (1.0 + 2.0 * side)) as f32);
        let off_centre = 1.0 - 0.25 / (step * samples as f64);
        let stamp_sum =
            (0.45 * std::f64::consts::PI * pixel_scale * pixel_scale / off_centre) as f32;
        let reach = (2.0 * scale).ceil() as i32;
        let side = (2 * reach + 1) as usize;
        let kernel = (0..side * side)
            .map(|i| {
                let (dx, dy) = ((i % side) as i32 - reach, (i / side) as i32 - reach);
                (-f64::from(dx * dx + dy * dy) / (0.45 * scale * scale)).exp() as f32
            })
            .collect();

        Self {
            width,
            height,
            stride,
            out_width,
            prefilter: (samples > 1).then(|| Prefilter::new(width, out_stride, height)),
            sums: std::array::from_fn(|_| vec![0.0; out_stride]),
            pixel: std::array::from_fn(|_| vec![0.0; stride]),
            position: vec![0.0; out_stride],
            coloured: None,
            next_out: 0,
            seed: look.seed.wrapping_mul(0x9E37_79B9),
            hue_shift: look.hue_shift,
            table: (samples == 1).then(gradients),
            node_noise: (samples > 1).then(|| NoiseRow::new(nodes)),
            units_per_pixel,
            columns,
            node_columns,
            slope: SHADE_SLOPE / units_per_pixel,
            grid,
            cubic,
            reduced,
            warp_rows: std::array::from_fn(|_| warp_row()),
            blend: std::array::from_fn(|_| vec![0.0; nodes]),
            warps: std::array::from_fn(|_| vec![0.0; stride]),
            rows: [row.clone(), row.clone(), row],
            computed: 0,
            next_row: 0,
            stars: star_list,
            next_star: 0,
            kernel,
            reach,
            stamp,
            stamp_sum,
            next_stamp: 0,
            light: std::array::from_fn(|_| vec![0.0; stride]),
            density: vec![0.0; stride],
            factors: std::array::from_fn(|_| vec![0.0; stride]),
            blends: std::array::from_fn(|_| vec![0.0; stride]),
        }
    }

    /// Renders the next buffer row into `out` as little-endian XRGB8888 and returns its index,
    /// or `None` once the whole keyframe is drawn.
    pub fn next_band(&mut self, out: &mut Vec<u8>) -> Option<usize> {
        let y = match &self.prefilter {
            None => self.shade_next()?,
            Some(prefilter) => {
                let y = self.next_out;
                if y >= self.height / SUPERSAMPLE {
                    return None;
                }
                for _ in prefilter.pushed..=prefilter.last_row(y) {
                    self.shade_next().expect("the row is inside the keyframe");
                    let prefilter = self.prefilter.as_mut().expect("checked above");
                    prefilter.push(&self.light);
                }
                let prefilter = self.prefilter.as_ref().expect("checked above");
                prefilter.row(y, &mut self.sums);
                self.stamp_stars(y);
                self.next_out += 1;
                y
            }
        };
        out.clear();
        out.resize(self.sums[0].len() * 4, 0);
        let [red, green, blue] = if self.prefilter.is_some() {
            &self.sums
        } else {
            &self.light
        };
        for (((pixels, red), green), blue) in out
            .chunks_exact_mut(4 * LANES)
            .zip(red.chunks_exact(LANES))
            .zip(green.chunks_exact(LANES))
            .zip(blue.chunks_exact(LANES))
        {
            let codes: [u32; LANES] = std::array::from_fn(|l| {
                (channel_byte(red[l]) << 16) | (channel_byte(green[l]) << 8) | channel_byte(blue[l])
            });
            pixels.copy_from_slice(codes.map(u32::to_le_bytes).as_flattened());
        }
        out.truncate(self.out_width * 4);
        Some(y)
    }

    /// Draws the whole keyframe at once.
    pub fn render(
        look: Look,
        width: usize,
        height: usize,
        screen_height: u32,
        scale: f64,
        stars: bool,
    ) -> Vec<u8> {
        let mut raster = Self::new(look, width, height, screen_height, scale, stars);
        let mut pixels = Vec::with_capacity(width * height * 4);
        let mut band = Vec::new();
        while raster.next_band(&mut band).is_some() {
            pixels.extend_from_slice(&band);
        }
        pixels
    }

    /// Makes sure node row `row` is in its slot of the ring.
    fn warp_row(&mut self, row: isize) {
        let slot = &mut self.warp_rows[row.rem_euclid(WARP_ROWS as isize) as usize];
        if slot.row == row {
            return;
        }
        slot.row = row;
        let seed = self.seed;
        let [qx_row, qy_row, rx_row, ry_row] = &mut slot.values;
        if let Some(noise) = &mut self.node_noise {
            let py = (self.grid as isize * row) as f32 * self.units_per_pixel;
            let columns = &self.node_columns;
            // One row of positions for each of the four values, as in `fbm2`.
            let place = |noise: &mut NoiseRow, offset: [f32; 2], warp: Option<[&[f32]; 2]>| {
                for (i, (x, y)) in noise.x.iter_mut().zip(&mut noise.y).enumerate() {
                    let (wx, wy) = warp.map_or((0.0, 0.0), |[qx, qy]| (WARP * qx[i], WARP * qy[i]));
                    (*x, *y) = (columns[i] + wx + offset[0], py + wy + offset[1]);
                }
            };
            for (values, offset) in [(&mut *qx_row, [0.0, 0.0]), (&mut *qy_row, Q_OFFSET)] {
                place(noise, offset, None);
                values.fill(0.0);
                fbm_row(OCTAVES_Q, seed, 0.5, values, noise);
            }
            for (values, offset) in [(&mut *rx_row, RX_OFFSET), (&mut *ry_row, RY_OFFSET)] {
                place(noise, offset, Some([qx_row, qy_row]));
                values.fill(0.0);
                fbm_row(OCTAVES_R, seed, 0.5, values, noise);
            }
            return;
        }
        let table = self
            .table
            .expect("a keyframe without node rows has the table");
        let py = [(self.grid as isize * row) as f32 * self.units_per_pixel; LANES];
        for (chunk, px) in self.node_columns.chunks_exact(LANES).enumerate() {
            let px = lanes(px);
            let qx = fbm::<OCTAVES_Q>(*px, py, seed, table);
            let qy = fbm::<OCTAVES_Q>(
                px.map(|x| x + Q_OFFSET[0]),
                py.map(|y| y + Q_OFFSET[1]),
                seed,
                table,
            );
            let rx = fbm::<OCTAVES_R>(
                offset(px, &qx, RX_OFFSET[0]),
                offset(&py, &qy, RX_OFFSET[1]),
                seed,
                table,
            );
            let ry = fbm::<OCTAVES_R>(
                offset(px, &qx, RY_OFFSET[0]),
                offset(&py, &qy, RY_OFFSET[1]),
                seed,
                table,
            );
            let span = chunk * LANES..(chunk + 1) * LANES;
            qx_row[span.clone()].copy_from_slice(&qx);
            qy_row[span.clone()].copy_from_slice(&qy);
            rx_row[span.clone()].copy_from_slice(&rx);
            ry_row[span].copy_from_slice(&ry);
        }
    }

    /// Fills `blend` with the warp nodes of sample row `y`, which may lie outside the keyframe.
    fn blend_nodes(&mut self, y: isize) {
        let grid = self.grid as isize;
        let (row, phase) = (y.div_euclid(grid), y.rem_euclid(grid) as usize);
        let slot = |k: isize| k.rem_euclid(WARP_ROWS as isize) as usize;
        if phase == 0 {
            self.warp_row(row);
            for (blend, values) in self.blend.iter_mut().zip(&self.warp_rows[slot(row)].values) {
                blend.copy_from_slice(values);
            }
            return;
        }
        for k in row - 1..=row + 2 {
            self.warp_row(k);
        }
        let [a, b, c, d] = [row - 1, row, row + 1, row + 2].map(|k| &self.warp_rows[slot(k)]);
        let (halfway, weights) = (2 * phase == self.grid, self.cubic[phase]);
        for (q, blend) in self.blend.iter_mut().enumerate() {
            let (a, b, c, d) = (&a.values[q], &b.values[q], &c.values[q], &d.values[q]);
            for (i, out) in blend.iter_mut().enumerate() {
                *out = between(halfway, weights, a[i], b[i], c[i], d[i]);
            }
        }
    }

    /// Fills the warp vectors of every sample of row `y` from the node rows.
    fn interpolate_warps(&mut self, y: usize) {
        self.blend_nodes(y as isize);
        for (nodes, warps) in self.blend.iter().zip(&mut self.warps) {
            spread(nodes, warps, &self.cubic, 1);
        }
    }

    /// Makes sure the low octaves of row `row` of every other buffer pixel are stored.
    fn low_row(&mut self, reduced: &mut Reduced, row: isize) {
        if reduced.low.has(row) {
            return;
        }
        self.blend_nodes(4 * row);
        let n = self.stride / 4;
        spread(&self.blend[2], &mut reduced.rx[..n], &self.cubic, 4);
        spread(&self.blend[3], &mut reduced.ry[..n], &self.cubic, 4);
        let py = (4 * row) as f32 * self.units_per_pixel;
        let noise = &mut reduced.noise;
        for ((((x, y), px), rx), ry) in noise
            .x
            .iter_mut()
            .zip(&mut noise.y)
            .zip(self.columns.iter().step_by(4))
            .zip(&reduced.rx)
            .zip(&reduced.ry)
        {
            (*x, *y) = (px + WARP * rx, py + WARP * ry);
        }
        let total = &mut reduced.row_f[..n];
        total.fill(0.0);
        fbm_row(LOW_OCTAVES, self.seed, 0.5, total, noise);
        reduced.low.put(row, total);
    }

    /// Makes sure `f` and the hue of buffer pixel row `row` are stored.
    fn pixel_row(&mut self, reduced: &mut Reduced, row: isize) {
        if reduced.f.has(row) {
            return;
        }
        for low in Midpoints::needs(row) {
            self.low_row(reduced, low);
        }
        reduced.low.fill(row, &mut reduced.row_f);
        self.blend_nodes(2 * row);
        let [qx, qy, rx, ry] = &self.blend;
        for (((hue, qx), qy), rx) in reduced.node_hue.iter_mut().zip(qx).zip(qy).zip(rx) {
            *hue = 0.7 * qx + 0.7 * qy + 0.1 * rx;
        }
        spread(&reduced.node_hue, &mut reduced.row_hue, &self.cubic, 2);
        spread(rx, &mut reduced.rx, &self.cubic, 2);
        spread(ry, &mut reduced.ry, &self.cubic, 2);
        let py = (2 * row) as f32 * self.units_per_pixel;
        let [re, im] = reduced.turn;
        let noise = &mut reduced.noise;
        for ((((x, y), px), rx), ry) in noise
            .x
            .iter_mut()
            .zip(&mut noise.y)
            .zip(self.columns.iter().step_by(2))
            .zip(&reduced.rx)
            .zip(&reduced.ry)
        {
            let (wx, wy) = (px + WARP * rx, py + WARP * ry);
            (*x, *y) = (re * wx - im * wy, im * wx + re * wy);
        }
        fbm_row(
            OCTAVES_F - LOW_OCTAVES,
            self.seed,
            reduced.amplitude,
            &mut reduced.row_f,
            noise,
        );
        reduced.f.put(row, &reduced.row_f);
        reduced.hues[row.rem_euclid(8) as usize].copy_from_slice(&reduced.row_hue);
    }

    /// Computes the field of row `y` into its slot of the ring.
    fn compute(&mut self, y: usize) {
        let hue_shift = self.hue_shift;
        let n = self.stride;
        if let Some(mut reduced) = self.reduced.take() {
            for row in Midpoints::needs(y as isize) {
                self.pixel_row(&mut reduced, row);
            }
            reduced.f.fill(y as isize, &mut self.rows[y % 3].f[1..=n]);
            self.reduced = Some(reduced);
        } else {
            self.interpolate_warps(y);
            let table = self
                .table
                .expect("a keyframe that is not reduced has the table");
            let seed = self.seed;
            let py = [y as f32 * self.units_per_pixel; LANES];
            let [qx_row, qy_row, rx_row, ry_row] = &self.warps;
            let row = &mut self.rows[y % 3];
            for (chunk, px) in self.columns.chunks_exact(LANES).enumerate() {
                let span = chunk * LANES..(chunk + 1) * LANES;
                let px = lanes(px);
                let (qx, qy) = (lanes(&qx_row[span.clone()]), lanes(&qy_row[span.clone()]));
                let (rx, ry) = (lanes(&rx_row[span.clone()]), lanes(&ry_row[span.clone()]));
                let f = fbm::<OCTAVES_F>(
                    std::array::from_fn(|l| px[l] + WARP * rx[l]),
                    std::array::from_fn(|l| py[l] + WARP * ry[l]),
                    seed,
                    table,
                );
                row.f[span.start + 1..span.end + 1].copy_from_slice(&f);
                let position = lanes_mut(&mut row.position[span]);
                for l in 0..LANES {
                    let hue = 0.7 * qx[l] + 0.7 * qy[l] + 0.1 * rx[l];
                    position[l] = palette_position(hue, hue_shift);
                }
            }
        }
        let w = self.width;
        let row = &mut self.rows[y % 3];
        row.f[0] = 2.0 * row.f[1] - row.f[2];
        row.f[w + 1] = 2.0 * row.f[w] - row.f[w - 1];
    }

    /// Shades the next row into the light and density rows, stars included, and returns its
    /// index.
    fn shade_next(&mut self) -> Option<usize> {
        let y = self.next_row;
        if y >= self.height {
            return None;
        }
        let ahead = (y + 1).min(self.height - 1);
        while self.computed <= ahead {
            self.compute(self.computed);
            self.computed += 1;
        }
        let above = y.saturating_sub(1);
        let vertical = self.slope / (ahead - above) as f32;
        let horizontal = self.slope * 0.5;
        let (above, centre, below) = (
            &self.rows[above % 3],
            &self.rows[y % 3],
            &self.rows[ahead % 3],
        );
        let n = self.stride;
        let [lum, rim, glow] = &mut self.factors;
        for ((((((((left, right), f), up), down), mass), lum), rim), glow) in centre.f[..n]
            .iter()
            .zip(&centre.f[2..])
            .zip(&centre.f[1..=n])
            .zip(&above.f[1..=n])
            .zip(&below.f[1..=n])
            .zip(&mut self.density)
            .zip(lum.iter_mut())
            .zip(rim.iter_mut())
            .zip(glow.iter_mut())
        {
            let gx = (right - left) * horizontal;
            let gy = (down - up) * vertical;
            let shade = ((LIGHT[0] * gx + LIGHT[1] * gy + LIGHT[2])
                / (gx * gx + gy * gy + 1.0).sqrt())
            .clamp(0.0, 1.0);
            let inner = smoothstep(THRESHOLD, THRESHOLD + 0.35, *f);
            *lum = (0.5 + 0.7 * inner) * ((1.0 - SHADE_WEIGHT) + SHADE_WEIGHT * 1.6 * shade);
            *rim = (smoothstep(THRESHOLD, THRESHOLD + 0.025, *f)
                - smoothstep(THRESHOLD + 0.03, THRESHOLD + 0.10, *f))
                * RIM;
            let outer = smoothstep(THRESHOLD - 0.3, THRESHOLD, *f);
            *glow = outer * outer * GLOW;
            *mass = smoothstep(THRESHOLD - EDGE, THRESHOLD + EDGE, *f);
        }
        if self.prefilter.is_some() {
            if self.coloured != Some(y / SUPERSAMPLE) {
                self.colour_row(y / SUPERSAMPLE);
            }
            // The colour changes far more slowly than a buffer pixel, so the samples of a
            // buffer pixel share the colour of the pixel.
            let [lum, rim, glow] = &self.factors;
            for ((channel, light), colour) in self.light.iter_mut().enumerate().zip(&self.pixel) {
                for (((((light, &mass), &lum), &rim), &glow), &colour) in light
                    .iter_mut()
                    .zip(&self.density)
                    .zip(lum)
                    .zip(rim)
                    .zip(glow)
                    .zip(colour)
                {
                    let lit = colour * lum;
                    let lit = lit + rim * (lit * 0.6 + 0.4);
                    *light =
                        BACKGROUND[channel] * (1.0 - mass) + lit * mass + lit * glow * (1.0 - mass);
                }
            }
            self.dim_stars(y);
            self.next_row += 1;
            return Some(y);
        }
        for (stop, blend) in self.blends.iter_mut().enumerate() {
            for (blend, &position) in blend.iter_mut().zip(&centre.position) {
                let t = (position - stop as f32).clamp(0.0, 1.0);
                *blend = t * t * (3.0 - 2.0 * t);
            }
        }
        let [lum, rim, glow] = &self.factors;
        let [b0, b1, b2, b3, b4] = &self.blends;
        for (channel, light) in self.light.iter_mut().enumerate() {
            let stops = STOPS.map(|stop| stop[channel]);
            let steps: [f32; STOPS.len() - 1] =
                std::array::from_fn(|stop| stops[stop + 1] - stops[stop]);
            for (((((((((light, &mass), &lum), &rim), &glow), &b0), &b1), &b2), &b3), &b4) in light
                .iter_mut()
                .zip(&self.density)
                .zip(lum)
                .zip(rim)
                .zip(glow)
                .zip(b0)
                .zip(b1)
                .zip(b2)
                .zip(b3)
                .zip(b4)
            {
                let mut colour = stops[0];
                for (step, blend) in steps.into_iter().zip([b0, b1, b2, b3, b4]) {
                    colour += step * blend;
                }
                let lit = colour * lum;
                let lit = lit + rim * (lit * 0.6 + 0.4);
                *light =
                    BACKGROUND[channel] * (1.0 - mass) + lit * mass + lit * glow * (1.0 - mass);
            }
        }
        self.add_stars(y);
        self.next_row += 1;
        Some(y)
    }

    /// Fills `pixel` with the palette colour of every buffer pixel of row `y`, once per sample.
    fn colour_row(&mut self, y: usize) {
        self.coloured = Some(y);
        let reduced = self
            .reduced
            .as_ref()
            .expect("a prefiltered keyframe is reduced");
        // A buffer pixel is centred a quarter of a pixel past the sample that holds its hue,
        // along both axes.
        let below = (y + 1).min(self.height / SUPERSAMPLE - 1);
        let (hues, next) = (&reduced.hues[y % 8], &reduced.hues[below % 8]);
        for (((hue, pair), &down), &across) in self
            .position
            .iter_mut()
            .zip(hues.windows(2))
            .zip(next)
            .zip(&next[1..])
        {
            let (top, bottom) = (0.75 * pair[0] + 0.25 * pair[1], 0.75 * down + 0.25 * across);
            *hue = palette_position(0.75 * top + 0.25 * bottom, self.hue_shift);
        }
        for (stop, blend) in self.blends.iter_mut().enumerate() {
            for (blend, &position) in blend.iter_mut().zip(&self.position) {
                let t = (position - stop as f32).clamp(0.0, 1.0);
                *blend = t * t * (3.0 - 2.0 * t);
            }
        }
        let [b0, b1, b2, b3, b4] = &self.blends;
        for (channel, pixel) in self.pixel.iter_mut().enumerate() {
            let stops = STOPS.map(|stop| stop[channel]);
            let steps: [f32; STOPS.len() - 1] =
                std::array::from_fn(|stop| stops[stop + 1] - stops[stop]);
            // Both samples of a buffer pixel get its colour, so the shading reads the colour
            // like any other row of samples.
            for (((((pair, &b0), &b1), &b2), &b3), &b4) in pixel
                .chunks_exact_mut(SUPERSAMPLE)
                .zip(b0)
                .zip(b1)
                .zip(b2)
                .zip(b3)
                .zip(b4)
            {
                let mut colour = stops[0];
                for (step, blend) in steps.into_iter().zip([b0, b1, b2, b3, b4]) {
                    colour += step * blend;
                }
                pair.fill(colour);
            }
        }
    }

    /// Dims the stars of sample row `y` by the mass they sit on, before they are stamped.
    fn dim_stars(&mut self, y: usize) {
        while let Some(star) = self.stars.get_mut(self.next_star)
            && star.y <= y as i32
        {
            star.brightness *= 1.0 - 0.7 * self.density[star.x as usize];
            self.next_star += 1;
        }
    }

    /// Adds the stars to buffer row `y` after the prefilter, which would spread each one over
    /// three pixels and leave a quarter of its peak. A star goes on the buffer pixel that holds
    /// its sample. Sharing it between pixels to keep its place within the pixel halves its peak.
    fn stamp_stars(&mut self, y: usize) {
        let row = |star: &Star| star.y as usize / SUPERSAMPLE;
        while self
            .stars
            .get(self.next_stamp)
            .is_some_and(|star| row(star) + 1 < y)
        {
            self.next_stamp += 1;
        }
        for star in &self.stars[self.next_stamp..] {
            if row(star) > y + 1 {
                break;
            }
            let vertical = self.stamp[row(star) + 1 - y];
            let column = star.x as usize / SUPERSAMPLE;
            for (dx, horizontal) in self.stamp.into_iter().enumerate() {
                let Some(x) = (column + dx).checked_sub(1).filter(|&x| x < self.out_width) else {
                    continue;
                };
                let weight = star.brightness * self.stamp_sum * vertical * horizontal;
                for (sums, colour) in self.sums.iter_mut().zip(STAR_COLOUR) {
                    sums[x] += weight * colour;
                }
            }
        }
    }

    fn add_stars(&mut self, y: usize) {
        let y = y as i32;
        while self.next_star < self.stars.len() && self.stars[self.next_star].y < y - self.reach {
            self.next_star += 1;
        }
        let side = (2 * self.reach + 1) as usize;
        let width = self.width as i32;
        for star in &self.stars[self.next_star..] {
            if star.y > y + self.reach {
                break;
            }
            let row = &self.kernel[(y - star.y + self.reach) as usize * side..][..side];
            for (dx, &k) in (-self.reach..=self.reach).zip(row) {
                let x = star.x + dx;
                if !(0..width).contains(&x) {
                    continue;
                }
                let x = x as usize;
                let weight = star.brightness * k * (1.0 - 0.7 * self.density[x]);
                for (light, colour) in self.light.iter_mut().zip(STAR_COLOUR) {
                    light[x] += weight * colour;
                }
            }
        }
    }
}

/// A keyframe drawn through the real raster, with the per-pixel values the tests look at.
#[cfg(test)]
pub(crate) struct Probe {
    pub pixels: Vec<u8>,
    /// Mass density per pixel, 0 outside the masses and 1 inside.
    pub density: Vec<f32>,
    /// Palette position per pixel.
    pub position: Vec<f32>,
}

#[cfg(test)]
impl Probe {
    pub fn draw(look: Look, width: usize, height: usize, screen_height: u32, stars: bool) -> Self {
        let mut raster = Raster::new(look, width, height, screen_height, 1.0, stars);
        let mut probe = Self {
            pixels: Vec::with_capacity(width * height * 4),
            density: Vec::with_capacity(width * height),
            position: Vec::with_capacity(width * height),
        };
        let mut band = Vec::new();
        while let Some(y) = raster.next_band(&mut band) {
            probe.pixels.extend_from_slice(&band);
            probe.density.extend_from_slice(&raster.density[..width]);
            probe
                .position
                .extend_from_slice(&raster.rows[y % 3].position[..width]);
        }
        probe
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::keyframe_look;

    const SEED: u64 = 0x5eed;
    const SIZE: [usize; 2] = [192, 81];
    const KEYFRAMES: u64 = 20;

    /// The first keyframes of a run, drawn small with their hue shifts and without stars.
    fn keyframes() -> &'static [Probe] {
        static PROBES: OnceLock<Vec<Probe>> = OnceLock::new();
        PROBES.get_or_init(|| {
            (0..KEYFRAMES)
                .map(|index| {
                    let look = keyframe_look(SEED, index);
                    Probe::draw(look, SIZE[0], SIZE[1], SIZE[1] as u32, false)
                })
                .collect()
        })
    }

    fn inside(density: f32) -> bool {
        density >= 0.5
    }

    // These keyframes measure shares from 0.566 to 0.824, with a mean of 0.709.
    #[test]
    fn coloured_share_stays_in_band() {
        let shares: Vec<f64> = keyframes()
            .iter()
            .map(|probe| {
                probe.density.iter().filter(|&&d| inside(d)).count() as f64
                    / probe.density.len() as f64
            })
            .collect();
        for (index, &share) in shares.iter().enumerate() {
            assert!(
                (0.53..0.86).contains(&share),
                "keyframe {index}: coloured share {share:.3}"
            );
        }
        let mean = shares.iter().sum::<f64>() / shares.len() as f64;
        assert!(
            (0.68..0.74).contains(&mean),
            "mean coloured share {mean:.3}"
        );
    }

    /// Brightness along the centre of a thin line at `angle`, after the prefilter and the
    /// compositor's bilinear filter. The line has a Gaussian profile with a deviation of half a
    /// buffer pixel.
    fn along_a_thin_line(angle: f64) -> Vec<f64> {
        const SIZE: usize = 96;
        const DEVIATION: f64 = 0.5;
        let samples = SIZE * SUPERSAMPLE;
        let centre = SIZE as f64 / 2.0;
        let (sin, cos) = angle.sin_cos();
        // Buffer pixel `j` is centred between samples `2 * j` and `2 * j + 1`.
        let position = |sample: usize| (sample as f64 - 0.5) / SUPERSAMPLE as f64;
        let mut prefilter = Prefilter::new(samples, SIZE, samples);
        let mut out: [Vec<f32>; 3] = std::array::from_fn(|_| vec![0.0; SIZE]);
        let mut buffer = Vec::with_capacity(SIZE);
        for y in 0..SIZE {
            for row in prefilter.pushed..=prefilter.last_row(y) {
                let line: Vec<f32> = (0..samples)
                    .map(|column| {
                        let distance =
                            (position(row) - centre) * cos - (position(column) - centre) * sin;
                        (-distance * distance / (2.0 * DEVIATION * DEVIATION)).exp() as f32
                    })
                    .collect();
                prefilter.push(&[line.clone(), line.clone(), line]);
            }
            prefilter.row(y, &mut out);
            buffer.push(out[0].clone());
        }
        (-240..240)
            .map(|step| {
                let along = f64::from(step) * 0.1;
                let (x, y) = (centre + along * cos, centre + along * sin);
                let (x0, y0) = (x.floor() as usize, y.floor() as usize);
                let (fx, fy) = (x.fract(), y.fract());
                let texel = |x: usize, y: usize| f64::from(buffer[y][x]);
                let top = texel(x0, y0) * (1.0 - fx) + texel(x0 + 1, y0) * fx;
                let bottom = texel(x0, y0 + 1) * (1.0 - fx) + texel(x0 + 1, y0 + 1) * fx;
                top * (1.0 - fy) + bottom * fy
            })
            .collect()
    }

    // The worst angle measures 0.17. A mean of the pixel's own 2x2 samples measures 0.38 and
    // shows as beads along the streaks, and the tent `(1, 3, 3, 1) / 8` measures 0.27.
    #[test]
    fn a_thin_diagonal_streak_has_no_beads() {
        for degrees in [8.0, 17.0, 26.565, 33.0, 41.0, 45.0f64] {
            let line = along_a_thin_line(degrees.to_radians());
            let (min, max) = line
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
            let mean = line.iter().sum::<f64>() / line.len() as f64;
            let modulation = (max - min) / mean;
            assert!(
                modulation < 0.2,
                "a streak at {degrees} degrees varies by {modulation:.3} along its length"
            );
        }
    }

    /// The light the stars add to a patch of a 5120x2160 screen, per screen pixel, in sums of
    /// the three channel codes. The patch is `size` buffer pixels at `scale` screen pixels per
    /// buffer pixel, shown through the compositor's bilinear filter.
    fn star_light(look: Look, size: [usize; 2], scale: f64) -> (Vec<Vec<f64>>, Raster) {
        const SCREEN_HEIGHT: u32 = 2160;
        let [width, height] = size;
        let draw = |stars| Raster::render(look, width, height, SCREEN_HEIGHT, scale, stars);
        let (with, without) = (draw(true), draw(false));
        let texel = |x: usize, y: usize| -> f64 {
            let at = (y * width + x) * 4;
            (0..3)
                .map(|c| f64::from(with[at + c]) - f64::from(without[at + c]))
                .sum()
        };
        let taps = |screen: usize, texels: usize| {
            let t = ((screen as f64 + 0.5) / scale - 0.5).clamp(0.0, (texels - 1) as f64);
            let first = (t.floor() as usize).min(texels - 2);
            (first, t - first as f64)
        };
        let [columns, rows] = size.map(|s| (s as f64 * scale) as usize);
        let light = (0..rows)
            .map(|y| {
                let (y0, fy) = taps(y, height);
                (0..columns)
                    .map(|x| {
                        let (x0, fx) = taps(x, width);
                        let top = texel(x0, y0) * (1.0 - fx) + texel(x0 + 1, y0) * fx;
                        let bottom = texel(x0, y0 + 1) * (1.0 - fx) + texel(x0 + 1, y0 + 1) * fx;
                        top * (1.0 - fy) + bottom * fy
                    })
                    .collect()
            })
            .collect();
        let raster = Raster::new(look, width, height, SCREEN_HEIGHT, scale, true);
        (light, raster)
    }

    /// Peak and total of the light within `reach` screen pixels of a point.
    fn peak_and_total(light: &[Vec<f64>], centre: [f64; 2], reach: f64) -> (f64, f64) {
        let mut found = (0.0f64, 0.0);
        for (y, row) in light.iter().enumerate() {
            for (x, &value) in row.iter().enumerate() {
                let (dx, dy) = (x as f64 + 0.5 - centre[0], y as f64 + 0.5 - centre[1]);
                if dx.abs() <= reach && dy.abs() <= reach {
                    found = (found.0.max(value), found.1 + value);
                }
            }
        }
        found
    }

    // At 3.10 screen pixels per buffer pixel the stars of this patch measure peaks from 0.77 to
    // 1.07 times their full-resolution peaks and totals from 0.81 to 1.25 times. A star that
    // goes through the prefilter measures a peak of 0.22.
    #[test]
    fn stars_are_as_bright_as_at_full_resolution() {
        const SCALE: f64 = 3.098;
        const SIZE: [usize; 2] = [256, 128];
        let look = Look {
            seed: 4,
            hue_shift: 0.0,
        };
        let (reduced, raster) = star_light(look, SIZE, SCALE);
        let full_size = SIZE.map(|s| (s as f64 * SCALE) as usize);
        let (full, _) = star_light(look, full_size, 1.0);
        let mut measured = 0;
        for (index, star) in raster.stars.iter().enumerate() {
            let centre = [star.x, star.y]
                .map(|sample| (f64::from(sample) + 0.5) * SCALE / SUPERSAMPLE as f64);
            let alone = raster.stars.iter().enumerate().all(|(other, s)| {
                other == index || (s.x - star.x).abs().max((s.y - star.y).abs()) > 16
            });
            let inside = (0..2).all(|a| centre[a] > 16.0 && centre[a] < full_size[a] as f64 - 16.0);
            if !alone || !inside {
                continue;
            }
            let (full_peak, full_total) = peak_and_total(&full, centre, 8.0);
            let (peak, total) = peak_and_total(&reduced, centre, 8.0);
            // Faint stars are a few codes high, where rounding decides the ratio.
            if full_peak < 60.0 {
                continue;
            }
            measured += 1;
            let (peak, total) = (peak / full_peak, total / full_total);

            assert!(
                (0.6..1.4).contains(&peak) && (0.6..1.4).contains(&total),
                "star {index}: peak {peak:.2} and total {total:.2} of full resolution"
            );
        }
        assert!(measured >= 5, "only {measured} stars were measured");
    }

    // The largest difference measures 4.8e-7.
    #[test]
    fn computed_directions_match_the_table() {
        for (hash, entry) in gradients().iter().enumerate() {
            let (x, y) = direction(hash as u32 | 0xABCD_0000);
            for (computed, entry) in [x, y].into_iter().zip(entry) {
                assert!(
                    (computed - entry).abs() < 1e-6,
                    "hash {hash}: {computed} instead of {entry}"
                );
            }
        }
    }

    // The rarest stop, aqua, is the nearest one for 0.070 of the coloured pixels.
    #[test]
    fn every_palette_stop_shows() {
        let mut counts = [0usize; STOPS.len()];
        for probe in keyframes() {
            for (&density, &position) in probe.density.iter().zip(&probe.position) {
                if inside(density) {
                    counts[(position + 0.5) as usize] += 1;
                }
            }
        }
        let total: usize = counts.iter().sum();
        for (stop, &count) in counts.iter().enumerate() {
            let share = count as f64 / total as f64;
            assert!(
                share > 0.05,
                "stop {stop} is the nearest one for {share:.4} of the colour"
            );
        }
    }
}
