//! Times drawing one keyframe at the buffer size of a 5120x2160 screen, and its slowest band.

use std::hint::black_box;
use std::time::Instant;

use screensaver::{Look, Raster, buffer_size};

const SCREEN: [u32; 2] = [5120, 2160];

fn main() {
    let [width, height] = SCREEN.map(|s| buffer_size(s) as usize);
    let look = Look {
        seed: 1,
        hue_shift: 0.7,
    };
    let mut band = Vec::new();
    let start = Instant::now();
    let mut raster = Raster::new(look, width, height, SCREEN[1], true);
    let mut slowest = start.elapsed();
    loop {
        let band_start = Instant::now();
        let row = raster.next_band(&mut band);
        slowest = slowest.max(band_start.elapsed());
        if row.is_none() {
            break;
        }
        black_box(&band);
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "keyframe at {width}x{height}: {elapsed:.3} s, {:.1} ns per pixel, slowest band {:.3} ms",
        elapsed * 1e9 / (width * height) as f64,
        slowest.as_secs_f64() * 1000.0,
    );
}
