A fullscreen Wayland screensaver for COSMIC that draws domain-warped keyframes at full resolution on the CPU, lets the compositor pan, zoom and fade them, and keeps an OLED display awake through idle-inhibit, while costing almost nothing next to background benchmarks.

## Build and test

```sh
cargo build --release                                    # the stripped binary must stay under 1 MB
cargo test --lib                                         # field and timeline tests: colour, bounds, fades, wear, long runs
cargo clippy --all-targets -- -W clippy::all
cargo run --release --example bench                      # seconds to draw one 6400x2700 keyframe, and the slowest row
cargo run --release --example keyframe -- 4 out.ppm      # one 1920x810 keyframe with no hue shift; NO_STARS=1 leaves out the stars
cargo run --release --example compose -- 52 out.ppm      # the 5120x2160 screen at 52 s, halfway through the fade into the second keyframe
make install                                             # binary, desktop entry and icon into ~/.local
```

Zero warnings policy. The release build and clippy must be clean. The debug `cargo test --lib` takes about 8 s, most of it in one six-hour wear simulation that two tests share.

## Architecture

| File | Purpose |
|------|---------|
| `src/lib.rs` | Seeded PRNG, and the screen and buffer sizes for a fractional scale. |
| `src/field.rs` | Inigo Quilez's nested domain warping (iquilezles.org/articles/warp): hash, gradient noise, fBm, the warp grid, shading, stars, and the raster that draws a keyframe row by row into XRGB8888. Coloured share and palette tests. |
| `src/timeline.rs` | Each keyframe's look and path from its index, its source rectangle over its life, the fades and the drawing order. Viewport bounds, fade, long-run, wear and blind-spot tests. |
| `src/main.rs` | Wayland client: fullscreen toplevel with a black single-pixel buffer, two keyframe subsurfaces with viewports and alpha multipliers, shared-memory pool, idle inhibitor, frame pacing and Esc. |
| `examples/` | The bench, keyframe and compose tools used to check speed and look. |
| `screensaver.desktop`, `screensaver.svg`, `Makefile` | Launcher entry, icon and install. |

## Key technical details

- The look is Inigo Quilez's nested domain warping from his article at https://iquilezles.org/articles/warp/: `f(p) = fbm(p + K*r)`, `r = fbm2(p + K*q)` and `q = fbm2(p)`. It is a port of a Python reference that was tuned against the user's screenshots. The hash, noise, fBm, warp offsets, palette, shading, rim, glow and stars match that reference. Do not retune them or add shapes.
- One field unit is the screen height divided by 0.8, in physical pixels, so the look is the same at every resolution. Each keyframe adds a hue shift in `0..2`, applied as a triangle wave on the normalized hue before the palette lookup.
- Each keyframe is drawn once at physical resolution into one of two buffers that are 1.25 times the screen in each dimension, rounded to even. At 5120x2160 the pool holds two 6400x2700 XRGB8888 buffers, 138 MB in total. Frames only change the viewport source rectangles and alpha multipliers, and cosmic-comp scales and blends on the GPU it already uses. Never redraw the screen on the CPU.
- The program has no GPU context on purpose. It must stay light while GPU benchmarks run, so keep rendering on `wl_shm`.
- A source rectangle pans in a straight line at constant speed and its size goes linearly between 1.00 and 1.06 times the screen. A rectangle outside the buffer is a protocol error that kills the program. The library rounds the rectangle to `wl_fixed` steps of 1/256 pixel and keeps it inside, and `source_stays_inside_the_buffer` guards the path.
- A new keyframe starts every 50 s and fades in on top over 4 s, so each keyframe is visible for 54 s and its path spans those 54 s. A longer dissolve between two unrelated keyframes looks like a muddy double exposure. The first keyframe fades in from black over 1 s. A fade starts only once its keyframe is completely drawn. A late keyframe leaves the current one resting at the end of its path.
- Timeline time advances by the real time between frames, capped at 0.1 s per frame. A pause in frame callbacks never jumps a pan past its end, and ordinary frames that arrive late do not slow the timeline. Time stays in `f64`.
- The warp vectors `q` and `r` are evaluated at every other row and column, and the pixels between take the cubic midpoint `(-1, 9, 9, -1) / 16`. A wider grid fails the parity check against the reference at 480x203, and bilinear interpolation does too. Everything else is evaluated at every pixel.
- The shading needs the gradient of `f`, so the raster computes one row ahead and carries it to the next band. Bands are single rows, and no frame starts a row after 1.5 ms of drawing. At 5120x2160 a keyframe takes about 0.9 s of CPU, which the budget spreads over about 20 s. The first keyframe is the exception: it is drawn whole right after the first black frame is sent, so the screen is black for about 0.9 s at startup.
- When all 8 lanes of a noise call fall in the same lattice cell, which is the common case for the coarse octaves, the corner gradients are looked up once. The result is bit-identical to the per-lane path.
- Every per-pixel loop works on chunks of 8 lanes or on whole rows so it auto-vectorizes on baseline x86-64. That is why `split` and `channel_byte` use bit tricks instead of a floor and a cast, the gradient table index is masked at the lookup, and no `target-cpu` is set. A table of the 65536 gradient directions replaces `cos` and `sin`.
- `xdg_surface.set_window_geometry` is sent on every configure. Without it the window geometry spans every subsurface.
- Source rectangles, alpha multipliers and stacking apply with the parent commit. Every frame ends with one parent commit that carries the frame callback.
- A buffer is written only after it is detached and the compositor has released it.
- Frames are paced at 30 fps by frame callbacks plus a deadline. When the compositor stops sending callbacks (window hidden, screen off), the loop sleeps in `poll`.
- Nothing may be anchored to screen space, because the user runs it on an OLED panel. `every_cell_wears_evenly` and `no_cell_stays_dark_for_long` guard burn-in. Giving every keyframe the same seed and pan direction fails both. Fix the timeline instead of loosening their bounds.
- cosmic-comp honours the idle inhibitor only while the surface is visible on an output.
- Esc is evdev keycode 1. There is no xkbcommon.

## Conventions

- Dependencies stay limited to `wayland-client`, `wayland-protocols` and `rustix`. There is no `unsafe`.
- Comments explain only constraints the code cannot express.
