# LilyGo T-Display S3 & CYD-3248S035C — Contributor Notes

Hardware reference and implementation notes for the two board ports in `esp32-no_std/`.

## Hardware Reference

### T-Display S3 (ESP32-S3, I8080 parallel)

| Pin | Function | Notes |
|-----|----------|-------|
| GPIO15 | LCD power enable | MUST be HIGH or display is dead |
| GPIO38 | Backlight | HIGH = on |
| GPIO5 | LCD reset | Active LOW pulse |
| GPIO6 | CS | I8080 chip select |
| GPIO7 | DC | I8080 data/command |
| GPIO8 | WR | I8080 write |
| GPIO9 | RD | Hold HIGH (not used) |
| GPIO39-48 | LCD_D0-D7 | I8080 data bus |
| GPIO0 | BOOT button | Active LOW, INPUT_PULLUP — dark mode toggle |
| GPIO14 | User button (top-right) | Active LOW, INPUT_PULLUP — page cycle |
| GPIO2 | Green LED | Active LOW (LOW=ON, HIGH=OFF) — not in LilyGo docs, confirmed by blink test |
| GPIO4 | Battery voltage ADC | 11dB attenuation, ×2 voltage divider, raw>4000 = no battery |

Source: https://github.com/Xinyuan-LilyGO/T-Display-S3 `examples/tft/pin_config.h`

- **LCD**: ST7789, 170×320 (landscape), I8080 8-bit parallel
- **ROW_OFFSET**: 35 (colstart=0, rowstart=35 for rotation 1)
- **MADCTL**: 0x60 (MY|MV)
- **Color inversion**: ON (0x21 command)
- **Red LED** near USB-C: NOT controllable (TP4056 charging IC)

### CYD ESP32-3248S035C (ESP32 classic, SPI)

| Pin | Function | Notes |
|-----|----------|-------|
| GPIO14 | SPI SCK | LCD clock |
| GPIO13 | SPI MOSI | LCD data out |
| GPIO12 | SPI MISO | LCD data in |
| GPIO15 | SPI CS | LCD chip select |
| GPIO2 | LCD DC | Data/Command |
| GPIO27 | Backlight | HIGH = on (stolen via AnyPin::steal) |
| GPIO32 | I2C SCL | GT911 touch |
| GPIO33 | I2C SDA | GT911 touch |
| GPIO25 | Touch RST | GT911 reset |
| GPIO0 | BOOT button | Active LOW, INPUT_PULLUP — dark mode toggle |
| GPIO4 | Red LED | Active LOW (LOW=ON) |
| GPIO16 | Green LED | Active LOW (LOW=ON) |
| GPIO17 | Blue LED | Active LOW (LOW=ON) |
| GPIO34 | Light sensor ADC | 11dB attenuation, input-only |

- **LCD**: ST7796, 320×480 portrait, SPI, color inversion ON, BGR order
- **Orientation**: `Orientation::default().flip_horizontal()` = cable down, LED top-left
- **Touch**: GT911 capacitive I2C (addresses 0x5D or 0x14 — autodetected at init)
- **Touch note**: The GT911 can sometimes fail to enumerate on I2C reset. The code retries both addresses (0x5D then 0x14) during init. If you see no touch response, a full power cycle usually fixes it.
- **Buttons**: Only BOOT (GPIO0) and RESET (EN) — no user button. BOOT short press cycles pages, long press toggles dark mode.

## Build & Flash

> Requires the [esp-rs toolchain](https://docs.esp-rs.org/book/installation/index.html). Adjust `--port` to match your serial device.

### T-Display S3

```bash
cd esp32-no_std/lilygo-tdisplay-s3

SLINT_FONT_SIZES=10,12,14,16,18,20,22 \
  PATH=$HOME/.rustup/toolchains/esp/xtensa-esp-elf/esp-15.2.0_20250920/xtensa-esp-elf/bin:$PATH \
  cargo +esp build --release

espflash flash --chip esp32s3 --port /dev/ttyACM0 \
  target/xtensa-esp32s3-none-elf/release/slint-workshop-esp-lilygo-tdisplay-s3
```

### CYD-3248S035C

```bash
cd esp32-no_std/cyd-3248s035c

SLINT_FONT_SIZES=10,11,12,13,14,15,16,18,20,22,24,28,32 \
  PATH=$HOME/.rustup/toolchains/esp/xtensa-esp-elf/esp-15.2.0_20250920/xtensa-esp-elf/bin:$PATH \
  cargo +esp build --release

espflash flash --chip esp32 --port /dev/ttyUSB0 \
  target/xtensa-esp32-none-elf/release/slint-workshop-esp-cyd-3248s035c
```

## Dependency Versions (IMPORTANT — mismatched versions won't compile)

| Board | esp-hal | esp-hal-embassy | esp-rtos | Slint |
|-------|---------|-----------------|----------|-------|
| T-Display S3 | `=1.0.0-rc.0` (pinned) | `0.9.0` | N/A | `1.15.x` |
| CYD | `~1.0` (stable) | N/A | `0.2.0` | `1.15.x` |

The T-Display S3 uses the RC esp-hal because the I8080 LCD driver API changed between RC and stable. Do NOT mix versions.

## Font Quality — Known Limitations & What Was Tried

### The Problem
Bitmap fonts on the 170px-tall RGB565 display produce characters only 6-12 pixels tall at sizes 10-14px. This is a physical resolution limitation, not a Slint bug.

### What Was Tried
1. **SDF fonts** (`.with_sdf_fonts(true)` in build.rs): WORSE on this screen. Slint docs literally say "Rendering is slower and may result in slightly inferior visual output." Binary dropped from 447KB to 307KB but fonts were blurrier. **Reverted.**
2. **`sdf-fonts` feature on `slint` runtime crate**: Does NOT exist. Only on `slint-build`. Adding to `slint` deps causes resolver error.
3. **`EmbedForSoftwareRendererWithSdf`**: Does NOT exist in slint-build 1.15.1. Must use `.with_sdf_fonts(true)` method.
4. **`SLINT_FONT_SIZES` env var**: Current approach — pre-renders exact pixel sizes at build time. Prevents blurry runtime scaling. This is the best option available.

### What Might Work (future)
- Import a custom pixel/bitmap font optimized for small displays
- Use a higher-resolution panel (e.g., 480×320 IPS)
- Try Slint's `with-font-embedder` to embed a hand-crafted tiny font

### Dark Mode Font Contrast
Light-on-dark text appears thinner than dark-on-light because anti-aliasing pixels blend toward invisible black (halation effect). Mitigation:
- Use brighter dark-mode text colors (#dddddd+)
- Use heavier font weights (600+) for body text
- Pure white on pure black = 21:1 contrast (WCAG AAA)
- Never use colors below #aaaaaa on #000000 for text

## Slint Language Limitations (v1.12.x / 1.15.x)

These will cause confusing compiler errors if you hit them:

| Feature | Status | Workaround |
|---------|--------|------------|
| `for i in 0..N :` (integer iteration) | Broken | Unroll loops manually |
| `for c in [a, b] :` (array iteration) | Broken | Unroll manually |
| `["str1", "str2"][idx]` (integer indexing) | Broken | Use `if/else` chains |
| `as float` cast | Doesn't exist | Use integer math |
| `Math.sin()` | Requires unit suffix | `Math.sin(1rad * x)` |
| `%` operator | Doesn't exist | `Math.mod(x, y)` |
| `ProgressBar` | Not in std-widgets | Build from Rectangle |
| `vertical-alignment` on `HorizontalLayout` | Invalid | Put on child elements only |
| `border-radius` / `background` on `HorizontalLayout` | Invalid | Wrap in a `Rectangle` |

## UI Architecture

### T-Display S3

The T-Display S3 uses a single `mainwindow.slint` file with 7 pages selected by `if page == N :` conditional elements. Pages cycle via GPIO14 button press. The `tick` property (incremented in main.rs) drives all animations.

- **Page 0**: Status Dashboard — dark mode badge, LED indicator, battery bar
- **Page 1**: Typography — font size/weight samples, colored letters
- **Page 2**: Animations — progress bar, color-cycling dots, opacity wave
- **Page 3**: Colors & Shapes — palette, border radius, border styles
- **Page 4**: Gauges & Bars — CPU/MEM/NET animated bars, stacked bar
- **Page 5**: Cards & Layout — sensor cards, uptime bar
- **Page 6**: Visual States — toggle switches, button states (Normal/Hover/Press/Off)

### CYD-3248S035C

The CYD uses `dashboard.slint` with 7 pages and a `SwipeGestureHandler` carousel for iOS-style swipe navigation. Pages also advance via BOOT short press. BOOT long press toggles dark mode. Swipe left/right with 25% threshold snap-swipe (250ms ease-out). `tick` drives animations. Non-interactive page indicator dots in the bottom bar.

- **Page 0**: Hardware Dashboard — mode, RGB LED status, light ADC bar, boot button, touch position, tap count
- **Page 1**: Typography — font size/weight samples (10-24px), colored letters (larger than T-Display S3)
- **Page 2**: Animations — progress bar, color-cycling dots, opacity wave
- **Page 3**: Colors & Shapes — palette, border radius, border styles
- **Page 4**: Gauges & Bars — CPU/MEM/NET/DISK animated bars, stacked bar
- **Page 5**: Touch Demo — touch state indicator, position, tap count (CYD-exclusive, showcases GT911)
- **Page 6**: LED Control — large color picker with current LED indicator

Architecture: 5 embassy tasks (render_loop, touch_task, sensor_task, rgb_task, ui_sync_task) communicate via `SharedState` mutex. Line-by-line rendering via `LineBufferProvider`. 100KB heap (`HEAP_SIZE = 100 * 1024`) to accommodate all 7 pages in the item tree simultaneously for smooth swipe transitions.

## Related

- Upstream PR: https://github.com/WilstonOreo/slint-esp-workshop/pull/8
- esp-hal I8080 discussion: https://github.com/esp-rs/esp-hal/issues/5136
