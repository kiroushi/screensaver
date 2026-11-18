A fullscreen Wayland screensaver for COSMIC that draws domain-warped keyframes on the CPU into buffers of at most 7.2 MB, lets the compositor scale, pan, zoom and fade them, and keeps an OLED display awake through idle-inhibit, while costing almost nothing next to background benchmarks.

## Build and test

```sh
cargo build --release                                    # the stripped binary must stay under 1 MB
cargo test --lib                                         # field and timeline tests: colour, bounds, fades, wear, long runs
cargo clippy --all-targets -- -W clippy::all
cargo run --release --example bench                      # seconds to draw one 2064x870 keyframe, and the slowest row
cargo run --release --example memory                     # peak memory of the whole program at 5120x2160, without showing anything
cargo run --release --example keyframe -- 4 out.ppm      # one 1920x810 keyframe with no hue shift; NO_STARS=1 leaves out the stars
cargo run --release --example compose -- 52 out.ppm      # the 5120x2160 screen at 52 s, halfway through the fade into the second keyframe
make install                                             # binary, desktop entry and icon into ~/.local
```

Zero warnings policy. The release build and clippy must be clean. The debug `cargo test --lib` takes about 8 s, most of it in one six-hour wear simulation that two tests share.

## Architecture

| File | Purpose |
|------|---------|
| `src/lib.rs` | Seeded PRNG, the screen size for a fractional scale, and the buffer size and scale that fit the pixel budget. |
| `src/field.rs` | Inigo Quilez's nested domain warping (iquilezles.org/articles/warp): hash, gradient noise, fBm, the warp grid, shading, stars, and the raster that draws a keyframe row by row into XRGB8888, with a prefilter over 2x2 samples per pixel when the buffer is smaller than the screen. Coloured share, palette and streak beading tests. |
| `src/timeline.rs` | Each keyframe's look and path from its index, its source rectangle over its life, the fades and the drawing order. Viewport bounds, fade, long-run, wear, blind-spot and buffer lifetime tests. |
| `src/main.rs` | Wayland client: fullscreen toplevel with a black single-pixel buffer, two keyframe subsurfaces with viewports and alpha multipliers, one shared-memory buffer per keyframe, idle inhibitor, frame pacing and Esc. |
| `examples/` | The bench, memory, keyframe and compose tools used to check speed, memory and look. |
| `screensaver.desktop`, `screensaver.svg`, `Makefile` | Launcher entry, icon and install. |

## Key technical details

- The look is Inigo Quilez's nested domain warping from his article at https://iquilezles.org/articles/warp/: `f(p) = fbm(p + K*r)`, `r = fbm2(p + K*q)` and `q = fbm2(p)`. It is a port of a Python reference that was tuned against the user's screenshots. The hash, noise, fBm, warp offsets, palette, shading, rim, glow and stars match that reference. Do not retune them or add shapes.
- One field unit is the screen height divided by 0.8, in physical pixels, so the look is the same at every resolution and buffer scale. Each keyframe adds a hue shift in `0..2`, applied as a triangle wave on the normalized hue before the palette lookup.
- The whole program must stay under 20 MB at its peak. `cargo run --release --example memory` measures 18.4 MB at 5120x2160 while two keyframes are alive and 11.2 MB otherwise. The parts are 7.2 MB per keyframe buffer, 3.1 MB of binary and shared libraries and 0.9 MB of heap. The program writes keyframes with `pwrite` and never maps them, so their pages are missing from its resident set and the example adds them.
- A keyframe covers 1.25 times the screen in each dimension, which is the room the pan needs. Its buffer holds at most `BUFFER_PIXELS`, 1.8 million. At 5120x2160 the buffer is 2064x870 and one buffer pixel spans 3.10 screen pixels. A screen whose covered area fits the budget gets one buffer pixel per screen pixel.
- The format is XRGB8888 because cosmic-comp 1.11 advertises only ARGB8888, XRGB8888, ABGR8888 and XBGR8888 on `wl_shm`. A format it does not advertise is a fatal `invalid_format` error. RGB565 would hold twice the pixels in the same bytes and look sharper, so check the advertised formats again when cosmic-comp changes.
- cosmic-comp scales surfaces with `GL_LINEAR` in both directions and has no mipmaps (smithay's `GlesRenderer` defaults, which cosmic-comp does not change). A hard edge in the buffer becomes a ramp one buffer pixel wide on screen.
- When the buffer is smaller than the area it covers, the field is sampled twice per buffer pixel along each axis. A buffer pixel is a weighted sum of 6x6 samples with the weights `(1, 3, 5, 5, 3, 1) / 18` along each axis, which is a tent three buffer pixels wide. The samples are shared between neighbouring pixels, so the cost stays at four field samples per pixel.
- The compositor's bilinear filter turns detail about one buffer pixel wide into beads along diagonal streaks. `a_thin_diagonal_streak_has_no_beads` measures the brightness variation along a synthetic streak. The tent measures 0.17, the mean of a pixel's own 2x2 samples measures 0.38 and shows beads, and `(1, 3, 3, 1) / 8` measures 0.27 and still shows them faintly. A tent four pixels wide measures 0.12 but widens an edge from 5.9 to 7.5 screen pixels. One sample per pixel leaves stairs on the mass edges, and a least-squares fit to the bilinear filter draws stairs along diagonal streaks.
- Each keyframe has its own memfd and `wl_buffer`. They are created when its drawing starts, 15 s before the keyframe is due, and destroyed once the next keyframe has faded in over it and the compositor has released the buffer. Two buffers are alive for 19 s of every 50 s. `two_keyframes_are_alive_for_under_half_of_the_time` guards that.
- Frames only change the viewport source rectangles and alpha multipliers, and cosmic-comp scales and blends on the GPU it already uses. Never redraw the screen on the CPU.
- The program has no GPU context on purpose. It must stay light while GPU benchmarks run, so keep rendering on `wl_shm`.
- A source rectangle pans in a straight line at constant speed and its size goes linearly between 1.00 and 1.06 times the screen. The timeline works in buffer pixels, so the screen size it holds is the physical size divided by the buffer scale. A rectangle outside the buffer is a protocol error that kills the program. The library rounds the rectangle to `wl_fixed` steps of 1/256 pixel and keeps it inside, and `source_stays_inside_the_buffer` guards the path.
- A new keyframe starts every 50 s and fades in on top over 4 s, so each keyframe is visible for 54 s and its path spans those 54 s. A longer dissolve between two unrelated keyframes looks like a muddy double exposure. The first keyframe fades in from black over 1 s. A fade starts only once its keyframe is completely drawn. A late keyframe leaves the current one resting at the end of its path.
- Timeline time advances by the real time between frames, capped at 0.1 s per frame. A pause in frame callbacks never jumps a pan past its end, and ordinary frames that arrive late do not slow the timeline. Time stays in `f64`.
- The warp vectors `q` and `r` are evaluated at every other row and column of samples, and the samples between take the cubic midpoint `(-1, 9, 9, -1) / 16`. A wider grid fails the parity check against the reference at 480x203, and bilinear interpolation does too. Everything else is evaluated at every sample.
- The shading needs the gradient of `f`, so the raster computes one row ahead and carries it to the next band. Bands are single buffer rows, and no frame starts a row after 1.5 ms of drawing. At 5120x2160 a keyframe takes about 0.4 s of CPU, which the budget spreads over about 8 s. The first keyframe is the exception: it is drawn whole right after the first black frame is sent, so the screen is black for about 0.4 s at startup.
- When all 8 lanes of a noise call fall in the same lattice cell, which is the common case for the coarse octaves, the corner gradients are looked up once. The result is bit-identical to the per-lane path.
- Every per-pixel loop works on chunks of 8 lanes or on whole rows so it auto-vectorizes on baseline x86-64. That is why `split` and `channel_byte` use bit tricks instead of a floor and a cast, the gradient table index is masked at the lookup, and no `target-cpu` is set. A table of the 65536 gradient directions replaces `cos` and `sin`.
- `xdg_surface.set_window_geometry` is sent on every configure. Without it the window geometry spans every subsurface.
- Source rectangles, alpha multipliers and stacking apply with the parent commit. Every frame ends with one parent commit that carries the frame callback.
- cosmic-comp keeps an attached buffer until another buffer replaces it or the surface detaches it, and only then sends `wl_buffer.release`. A buffer is written only before its first attach, and a slot gets its next buffer only after the compositor has released the last one.
- Frames are paced at 30 fps by frame callbacks plus a deadline. When the compositor stops sending callbacks (window hidden, screen off), the loop sleeps in `poll`.
- Nothing may be anchored to screen space, because the user runs it on an OLED panel. `every_cell_wears_evenly` and `no_cell_stays_dark_for_long` guard burn-in. Giving every keyframe the same seed and pan direction fails both. Fix the timeline instead of loosening their bounds.
- cosmic-comp honours the idle inhibitor only while the surface is visible on an output.
- Esc is evdev keycode 1. There is no xkbcommon.

## Conventions

- Dependencies stay limited to `wayland-client`, `wayland-protocols` and `rustix`. There is no `unsafe`.
- Comments explain only constraints the code cannot express.
