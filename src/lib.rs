//! Domain-warped keyframes drawn on the CPU, and the timeline that pans, zooms and fades them.
//!
//! Each keyframe is drawn once into its own buffer of at most [`BUFFER_PIXELS`] pixels. The
//! compositor scales the visible part of each buffer up to the screen, moves it and fades between
//! buffers, so the client never redraws the screen.

mod field;
mod timeline;

pub use field::{Look, Raster, SUPERSAMPLE};
pub use timeline::{Layer, Source, Timeline, keyframe_look};

/// Buffer length in pixels for a logical length at a fractional scale given in 120ths.
pub fn physical_size(logical: u32, scale: u32) -> u32 {
    ((u64::from(logical) * u64::from(scale) + 60) / 120) as u32
}

/// Most pixels a keyframe buffer holds, which is 7.2 MB of XRGB8888. Two buffers exist while one
/// keyframe is drawn and fades in over another, and the rest of the program holds about 4 MB.
pub const BUFFER_PIXELS: u32 = 1_800_000;

/// Length of the area a keyframe covers for a physical screen length: a quarter more, rounded to
/// even.
fn covered_size(screen: u32) -> u32 {
    ((u64::from(screen) * 5 + 4) / 8) as u32 * 2
}

/// The size of a keyframe buffer for a physical screen size, and the screen pixels per buffer
/// pixel. The scale is 1 when the area a keyframe covers fits in [`BUFFER_PIXELS`].
pub fn buffer_layout(screen: [u32; 2]) -> ([u32; 2], f64) {
    let covered = screen.map(covered_size);
    let pixels = u64::from(covered[0]) * u64::from(covered[1]);
    if pixels <= u64::from(BUFFER_PIXELS) {
        return (covered, 1.0);
    }
    let scale = (pixels as f64 / f64::from(BUFFER_PIXELS)).sqrt();
    let size = covered.map(|c| ((f64::from(c) / scale) as u32 / 2 * 2).max(2));
    (size, scale)
}

/// Seeded SplitMix64 generator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform value in `0.0..1.0`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform value in `low..high`.
    pub fn range(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    /// Uniform integer in `0..n`.
    pub fn below(&mut self, n: u32) -> u32 {
        (((self.next_u64() >> 32) * u64::from(n)) >> 32) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_match_the_fractional_scale_and_margin() {
        let screen = [3413, 1440].map(|s| physical_size(s, 180));
        assert_eq!(screen, [5120, 2160]);
        let (size, scale) = buffer_layout(screen);
        assert_eq!(size, [2064, 870]);
        assert!((scale - 9.6f64.sqrt()).abs() < 1e-12, "scale {scale}");
        assert_eq!(
            buffer_layout([1280, 720].map(|s| physical_size(s, 120))),
            ([1600, 900], 1.0)
        );
        assert_eq!(physical_size(1281, 150), 1601);
        assert_eq!(buffer_layout([1601, 810]).0, [1886, 952]);
    }

    #[test]
    fn no_buffer_holds_more_than_the_pixel_budget() {
        for screen in [
            [5120, 2160],
            [3840, 2160],
            [7680, 4320],
            [1080, 1920],
            [1601, 901],
        ] {
            let (size, scale) = buffer_layout(screen);
            assert!(
                size[0] * size[1] <= BUFFER_PIXELS,
                "screen {screen:?}: {size:?}"
            );
            for a in 0..2 {
                let covered = f64::from(size[a]) * scale;
                assert!(
                    covered > f64::from(screen[a]) * 1.24,
                    "screen {screen:?}: the buffer covers {covered} on axis {a}"
                );
            }
        }
    }
}
