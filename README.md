# screensaver

A fullscreen screensaver for the COSMIC desktop that shows swirling, marbled colour fields and keeps the display awake while it is open. Each image slowly pans and zooms, and every 50 seconds a new one with its own colour balance fades in over it, so an OLED panel wears evenly.

## Building

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
