A fullscreen Wayland screensaver for COSMIC that renders flowing colour ribbons on the CPU and keeps an OLED display awake through idle-inhibit, while costing almost nothing next to background benchmarks.

## Build and test

```sh
cargo build --release                                  # the stripped binary must stay under 1 MB
cargo test --lib                                       # renderer tests: wear, motion, look
cargo clippy --all-targets -- -W clippy::all
cargo run --release --example bench                    # milliseconds per frame at 320x135
cargo run --release --example snapshot -- 45 out.ppm   # 1920x810 bilinear preview of the frame at 45 s
make install                                           # binary, desktop entry and icon into ~/.local
```

Zero warnings policy. The release build and clippy must be clean.

## Architecture

| File | Purpose |
|------|---------|
| `src/lib.rs` | Renderer that turns a time into one XRGB8888 frame, plus the burn-in and motion tests. |
| `src/main.rs` | Wayland client: fullscreen toplevel, shared-memory double buffer, viewport scaling, idle inhibitor, frame pacing and Esc. |
| `examples/` | The bench and the snapshot tools used to tune speed and look. |
| `screensaver.desktop`, `screensaver.svg`, `Makefile` | Launcher entry, icon and install. |

## Key technical details

- The buffer is 320 px wide and the compositor stretches it to the screen through `wp_viewport`. cosmic-comp filters it linearly, which produces the soft look. Never upscale on the CPU.
- The program has no GPU context on purpose. It must stay light while GPU benchmarks run, so keep rendering on `wl_shm`.
- Frames are paced at 30 fps by frame callbacks plus a deadline. When the compositor stops sending callbacks (window hidden, screen off), the loop sleeps in `poll`.
- Time stays in `f64`, and every phase is reduced with `rem_euclid` before it reaches the `f32` pixel loop. Without that, motion stutters after a few days.
- Every spatial frequency is a whole number of turns per torus period, so the drift wraps without a seam. New terms must keep integral frequencies.
- Nothing may be anchored to screen space, because the user runs it on an OLED panel. Phase rates share the sign of the drift, since opposite signs leave lasting bright or dark patches.
- `every_region_wears_evenly` guards burn-in. A 10% corner vignette fails it. Fix the renderer instead of loosening its bound.
- The pixel loop must auto-vectorize on baseline x86-64. That is why `sin_turns` has no branches, `channel_byte` uses a bit trick instead of a float-to-integer cast, and no `target-cpu` is set.
- cosmic-comp honours the idle inhibitor only while the surface is visible on an output.
- Esc is evdev keycode 1. There is no xkbcommon.

## Conventions

- Dependencies stay limited to `wayland-client`, `wayland-protocols` and `rustix`. There is no `unsafe`.
- Comments explain only constraints the code cannot express.
