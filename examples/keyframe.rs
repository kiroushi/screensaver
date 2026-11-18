//! Draws one keyframe with no hue shift, treating the image as the whole screen, and writes it as
//! a PPM. Set `NO_STARS=1` to leave the stars out. With a scale above 1 the image is the buffer
//! that covers the screen at that many screen pixels per buffer pixel.
//!
//! Usage: `cargo run --release --example keyframe -- <seed> <out.ppm> [width height [scale]]`

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

use screensaver::{Look, Raster};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: keyframe <seed> <out.ppm> [width height [scale]]";
    let seed: u32 = args.next().ok_or(usage)?.parse()?;
    let path = args.next().ok_or(usage)?;
    let width: usize = args.next().map_or(Ok(1920), |a| a.parse())?;
    let height: usize = args.next().map_or(Ok(810), |a| a.parse())?;
    let scale: f64 = args.next().map_or(Ok(1.0), |a| a.parse())?;
    let stars = std::env::var_os("NO_STARS").is_none();
    let screen_height = height as u32;
    let [width, height] = [width, height].map(|s| (s as f64 / scale) as usize);

    let look = Look {
        seed,
        hue_shift: 0.0,
    };
    let pixels = Raster::render(look, width, height, screen_height, scale, stars);
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    for pixel in pixels.chunks_exact(4) {
        out.write_all(&[pixel[2], pixel[1], pixel[0]])?;
    }
    out.flush()?;
    Ok(())
}
