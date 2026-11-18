# screensaver

A fullscreen screensaver for the COSMIC desktop that shows slowly flowing colour ribbons and keeps the display awake while it is open. The ribbons drift across the whole screen and keep changing hue, so an OLED panel wears evenly.

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
