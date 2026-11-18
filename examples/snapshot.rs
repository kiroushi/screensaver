//! Renders one frame and writes it as a PPM, upscaled bilinearly the way the compositor does.
//!
//! Usage: `cargo run --release --example snapshot -- <seconds> <output.ppm>`

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};

use screensaver::{BUFFER_WIDTH, Renderer, buffer_height};

const OUT_WIDTH: usize = 1920;
const OUT_HEIGHT: usize = 810;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: snapshot <seconds> <output.ppm>";
    let time: f64 = args.next().ok_or(usage)?.parse()?;
    let path = args.next().ok_or(usage)?;

    let height = buffer_height(3413, 1440);
    let mut renderer = Renderer::new(BUFFER_WIDTH, height);
    let mut frame = vec![0; renderer.frame_len()];
    renderer.render(time, &mut frame);

    let sample =
        |x: usize, y: usize, channel: usize| f32::from(frame[(y * BUFFER_WIDTH + x) * 4 + channel]);
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{OUT_WIDTH} {OUT_HEIGHT}\n255\n")?;
    for oy in 0..OUT_HEIGHT {
        let sy = ((oy as f32 + 0.5) * height as f32 / OUT_HEIGHT as f32 - 0.5)
            .clamp(0.0, (height - 1) as f32);
        let (y0, fy) = (sy.floor() as usize, sy.fract());
        let y1 = (y0 + 1).min(height - 1);
        for ox in 0..OUT_WIDTH {
            let sx = ((ox as f32 + 0.5) * BUFFER_WIDTH as f32 / OUT_WIDTH as f32 - 0.5)
                .clamp(0.0, (BUFFER_WIDTH - 1) as f32);
            let (x0, fx) = (sx.floor() as usize, sx.fract());
            let x1 = (x0 + 1).min(BUFFER_WIDTH - 1);
            let mut rgb = [0u8; 3];
            for (i, channel) in [2, 1, 0].into_iter().enumerate() {
                let top = sample(x0, y0, channel) * (1.0 - fx) + sample(x1, y0, channel) * fx;
                let bottom = sample(x0, y1, channel) * (1.0 - fx) + sample(x1, y1, channel) * fx;
                rgb[i] = (top * (1.0 - fy) + bottom * fy + 0.5) as u8;
            }
            out.write_all(&rgb)?;
        }
    }
    out.flush()?;
    Ok(())
}
