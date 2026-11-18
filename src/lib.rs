//! Domain-warped keyframes drawn on the CPU, and the timeline that pans, zooms and fades them.
//!
//! Each keyframe is drawn once at physical resolution into its own buffer. The compositor moves
//! the visible part of each buffer and fades between them, so the client never redraws the screen.

mod field;
mod timeline;

pub use field::{Look, Raster};
pub use timeline::{Layer, Source, Timeline, keyframe_look};

/// Buffer length in pixels for a logical length at a fractional scale given in 120ths.
pub fn physical_size(logical: u32, scale: u32) -> u32 {
    ((u64::from(logical) * u64::from(scale) + 60) / 120) as u32
}

/// Keyframe buffer length for a physical screen length: a quarter more, rounded to even.
pub fn buffer_size(screen: u32) -> u32 {
    ((u64::from(screen) * 5 + 4) / 8) as u32 * 2
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
        assert_eq!(screen.map(buffer_size), [6400, 2700]);
        assert_eq!(
            [1280, 720].map(|s| buffer_size(physical_size(s, 120))),
            [1600, 900]
        );
        assert_eq!(physical_size(1281, 150), 1601);
        assert_eq!(buffer_size(1601), 2002);
        assert_eq!(buffer_size(810), 1012);
    }
}
