# Brand

The Quant mark is the letter Q drawn as a measurement. Its left side is one continuous stroke; its right side breaks into five dots that shrink as they go, like a signal being sampled: life is continuous, and Quant measures it in discrete quanta. The coral tail is the one thing that leaves the circle, the reading you choose to let out.

| File              | Use                                                                                                                                                |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mark.svg`        | The mark in color; follows the reader's light or dark appearance.                                                                                   |
| `lockup.svg`      | The mark with the monoline wordmark.                                                                                                                |
| `wordmark.svg`    | The wordmark alone, in `currentColor`.                                                                                                              |
| `mono.svg`        | One color (`currentColor`), for template images such as the menu-bar icon (`assets/app/tray/icon.png`).                                             |
| `icon.svg`        | The macOS app icon: the mark on the 824-pixel rounded square of Apple's 1024-pixel grid; set as `[app.icons.macos].image` in `leptoskit.toml`.    |
| `icon-square.svg` | Full bleed, for platforms that mask the icon themselves (iOS, Android adaptive icons) and social previews; set as `[app.icons].image`.             |
| `splash.svg`      | The launch-screen image: the mark centred on a transparent 1024 canvas inside Android 12's circle; set as `[app.launch_screen].image`.             |

Colors: Night `#0E0C24` (icon gradient from `#1A1645`), violet `#9D90FF` on dark and `#5B4CF5` on light, coral `#FF7A59` on dark and `#F0603C` on light. The phone app's full palette lives in `crates/quant_app/src/theme.rs`.

To redraw the menu-bar icon after changing the mark:

```sh
sed 's/viewBox="0 0 512 512"/viewBox="48 48 416 416"/; s/currentColor/#000000/g' assets/brand/mono.svg > /tmp/tray.svg
magick -background none -density 300 /tmp/tray.svg -resize 36x36 assets/app/tray/icon.png
```
