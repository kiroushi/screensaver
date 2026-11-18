# screensaver

A fullscreen screensaver for the COSMIC desktop that shows swirling, marbled colour fields and keeps the display awake while it is open. Each image slowly pans and zooms, and every 10 seconds a new one with its own colour balance fades in over it, so an OLED panel wears evenly.

## Building

The build targets x86-64-v3, so the binary needs a CPU with AVX2: Intel Haswell (2013), AMD Excavator (2015) or later.

```sh
cargo build --release
make install    # binary, launcher entry and icon into ~/.local (set PREFIX to change it)
```

## Usage

```sh
screensaver     # or open "Screensaver" from the launcher; Esc closes it
```

## License

MIT
