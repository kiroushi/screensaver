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
/// covers on screen. One sample per pixel leaves stairs on the mass edges once the compositor
/// scales the buffer up.
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

fn lanes(values: &[f32]) -> &Lanes {
    values.try_into().expect("a chunk is LANES long")
}

fn lanes_mut(values: &mut [f32]) -> &mut Lanes {
    values.try_into().expect("a chunk is LANES long")
}

/// The warp vectors `qx`, `qy`, `rx` and `ry` at every other column of one even row. Node `i`
/// sits at column `2 * i - 2`, so the nodes reach two columns past both edges.
struct WarpRow {
    row: isize,
    values: [Vec<f32>; 4],
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
/// The warp vectors vary slowly, so they are evaluated at every other row and column and the
/// samples between take a cubic midpoint. Everything else is evaluated at every sample. A buffer
/// pixel is the mean of `samples` by `samples` samples.
pub struct Raster {
    /// Samples per row.
    width: usize,
    /// Rows of samples.
    height: usize,
    stride: usize,
    samples: usize,
    out_width: usize,
    /// Sums of the samples of the buffer row being drawn, per channel.
    sums: [Vec<f32>; 3],
    seed: u32,
    hue_shift: f32,
    table: &'static Gradients,
    units_per_pixel: f32,
    columns: Vec<f32>,
    node_columns: Vec<f32>,
    /// Factor from a difference of `f` across one pixel to the shading slope.
    slope: f32,
    /// Warp node rows by row index modulo 4.
    warp_rows: [WarpRow; 4],
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
    light: [Vec<f32>; 3],
    density: Vec<f32>,
    /// Luminance, rim and glow of the row being shaded.
    factors: [Vec<f32>; 3],
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
        let nodes = (stride / 2 + 4).div_ceil(LANES) * LANES;
        let units_per_pixel = (FIELD_HEIGHT * step / f64::from(screen_height)) as f32;
        let columns = (0..stride).map(|x| x as f32 * units_per_pixel).collect();
        let node_columns = (0..nodes)
            .map(|i| (2 * i as i32 - 2) as f32 * units_per_pixel)
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
            samples,
            out_width,
            sums: std::array::from_fn(|_| vec![0.0; out_stride]),
            seed: look.seed.wrapping_mul(0x9E37_79B9),
            hue_shift: look.hue_shift,
            table: gradients(),
            units_per_pixel,
            columns,
            node_columns,
            slope: SHADE_SLOPE / units_per_pixel,
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
            light: std::array::from_fn(|_| vec![0.0; stride]),
            density: vec![0.0; stride],
            factors: std::array::from_fn(|_| vec![0.0; stride]),
        }
    }

    /// Renders the next buffer row into `out` as little-endian XRGB8888 and returns its index,
    /// or `None` once the whole keyframe is drawn.
    pub fn next_band(&mut self, out: &mut Vec<u8>) -> Option<usize> {
        let y = self.shade_next()? / self.samples;
        if self.samples > 1 {
            self.average();
        }
        out.clear();
        out.resize(self.stride / self.samples * 4, 0);
        let [red, green, blue] = if self.samples > 1 {
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

    /// Averages the sample row just shaded and the rest of its buffer row into `sums`. Each
    /// sample is clamped first, like the screen would clamp it.
    fn average(&mut self) {
        let samples = self.samples;
        let weight = 1.0 / (samples * samples) as f32;
        for sums in &mut self.sums {
            sums.fill(0.0);
        }
        for row in 0..samples {
            if row > 0 {
                self.shade_next()
                    .expect("the height is a whole number of buffer rows");
            }
            for (sums, light) in self.sums.iter_mut().zip(&self.light) {
                for (sum, group) in sums.iter_mut().zip(light.chunks_exact(samples)) {
                    for &value in group {
                        *sum += value.clamp(0.0, 1.0) * weight;
                    }
                }
            }
        }
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
        let slot = &mut self.warp_rows[row.rem_euclid(4) as usize];
        if slot.row == row {
            return;
        }
        slot.row = row;
        let (table, seed) = (self.table, self.seed);
        let py = [(2 * row) as f32 * self.units_per_pixel; LANES];
        let [qx_row, qy_row, rx_row, ry_row] = &mut slot.values;
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

    /// Fills the warp vectors of every pixel of row `y` from the node rows.
    fn interpolate_warps(&mut self, y: usize) {
        let row = (y / 2) as isize;
        let nodes = if y.is_multiple_of(2) {
            self.warp_row(row);
            &self.warp_rows[row.rem_euclid(4) as usize].values
        } else {
            for k in row - 1..=row + 2 {
                self.warp_row(k);
            }
            let [a, b, c, d] =
                [row - 1, row, row + 1, row + 2].map(|k| &self.warp_rows[k.rem_euclid(4) as usize]);
            for (q, blend) in self.blend.iter_mut().enumerate() {
                let (a, b, c, d) = (&a.values[q], &b.values[q], &c.values[q], &d.values[q]);
                for (i, out) in blend.iter_mut().enumerate() {
                    *out = midpoint(a[i], b[i], c[i], d[i]);
                }
            }
            &self.blend
        };
        for (nodes, warps) in nodes.iter().zip(&mut self.warps) {
            for (m, pair) in warps.chunks_exact_mut(2).enumerate() {
                pair[0] = nodes[m + 1];
                pair[1] = midpoint(nodes[m], nodes[m + 1], nodes[m + 2], nodes[m + 3]);
            }
        }
    }

    /// Computes the field of row `y` into its slot of the ring.
    fn compute(&mut self, y: usize) {
        self.interpolate_warps(y);
        let (table, seed, hue_shift) = (self.table, self.seed, self.hue_shift);
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
                let normalized = ((hue - HUE_LO) * (1.0 / (HUE_HI - HUE_LO))).clamp(0.0, HUE_TOP);
                let wrapped = normalized + hue_shift;
                let wrapped = if wrapped >= 2.0 {
                    wrapped - 2.0
                } else {
                    wrapped
                };
                let shifted = 1.0 - (wrapped - 1.0).abs();
                position[l] = shifted.min(HUE_TOP) * (STOPS.len() - 1) as f32;
            }
        }
        let w = self.width;
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
        let [lum, rim, glow] = &self.factors;
        for (channel, light) in self.light.iter_mut().enumerate() {
            let stops = STOPS.map(|stop| stop[channel]);
            for (((((light, &position), &mass), &lum), &rim), &glow) in light
                .iter_mut()
                .zip(&centre.position)
                .zip(&self.density)
                .zip(lum)
                .zip(rim)
                .zip(glow)
            {
                let mut colour = stops[0];
                for stop in 1..stops.len() {
                    let t = (position - (stop - 1) as f32).clamp(0.0, 1.0);
                    colour += (stops[stop] - stops[stop - 1]) * (t * t * (3.0 - 2.0 * t));
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
