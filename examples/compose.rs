//! Writes what the screen shows at a given time as a PPM: the keyframe of that time, scaled up
//! from its buffer like the compositor scales it. A keyframe shows every 10 s from 0 s on.
//!
//! Usage: `cargo run --release --example compose -- <seconds> <out.ppm> [width height]`
//!
//! The size is the physical screen size. It defaults to 5120x2160.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

use screensaver::{Raster, Timeline, buffer_layout};

const SEED: u64 = 0x5eed;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: compose <seconds> <out.ppm> [width height]";
    let time: f64 = args.next().ok_or(usage)?.parse()?;
    let path = args.next().ok_or(usage)?;
    let width: usize = args.next().map_or(Ok(5120), |a| a.parse())?;
    let height: usize = args.next().map_or(Ok(2160), |a| a.parse())?;

    let screen = [width as u32, height as u32];
    let (buffer, scale) = buffer_layout(screen);
    let mut timeline = Timeline::new(SEED);
    while timeline.due() <= time {
        timeline.show(timeline.due());
    }
    let index = timeline.shown().ok_or("no keyframe shows before 0 s")?;

    let [bw, bh] = buffer.map(|s| s as usize);
    let pixels = Raster::render(timeline.look(index), bw, bh, screen[1], scale, true);
    let texel = |x: usize, y: usize, c: usize| f32::from(pixels[(y * bw + x) * 4 + 2 - c]);
    let mut frame = vec![0.0f32; width * height * 3];
    for oy in 0..height {
        let y = ((oy as f64 + 0.5) * bh as f64 / height as f64 - 0.5).clamp(0.0, (bh - 1) as f64);
        let (y0, fy) = (y.floor() as usize, y.fract() as f32);
        let y1 = (y0 + 1).min(bh - 1);
        for ox in 0..width {
            let x =
                ((ox as f64 + 0.5) * bw as f64 / width as f64 - 0.5).clamp(0.0, (bw - 1) as f64);
            let (x0, fx) = (x.floor() as usize, x.fract() as f32);
            let x1 = (x0 + 1).min(bw - 1);
            let out = &mut frame[(oy * width + ox) * 3..][..3];
            for (c, value) in out.iter_mut().enumerate() {
                let top = texel(x0, y0, c) * (1.0 - fx) + texel(x1, y0, c) * fx;
                let bottom = texel(x0, y1, c) * (1.0 - fx) + texel(x1, y1, c) * fx;
                *value = (top * (1.0 - fy) + bottom * fy) / 255.0;
            }
        }
    }

    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    let bytes: Vec<u8> = frame
        .iter()
        .map(|&c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect();
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(())
}
