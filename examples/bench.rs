//! Times 600 frames at the target buffer size and prints the mean milliseconds per frame.

use std::hint::black_box;
use std::time::Instant;

use screensaver::{BUFFER_WIDTH, Renderer, buffer_height};

const FRAMES: u32 = 600;

fn main() {
    let mut renderer = Renderer::new(BUFFER_WIDTH, buffer_height(3413, 1440));
    let mut frame = vec![0; renderer.frame_len()];
    renderer.render(0.0, &mut frame);

    let start = Instant::now();
    for i in 0..FRAMES {
        renderer.render(black_box(f64::from(i) / 30.0), &mut frame);
        black_box(&frame);
    }
    let mean = start.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES);
    println!(
        "{FRAMES} frames at {}x{}: {mean:.3} ms per frame",
        renderer.width(),
        renderer.height()
    );
}
