# Brand

The Quant mark is a disc that is smooth on its left half and sampled into bars on its right, like a Riemann sum: life is continuous, and Quant measures it in discrete quanta. The bars double as an audio waveform.

| File              | Use                                                                                                                          |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `mark.svg`        | The mark in color, on light or dark backgrounds.                                                                             |
| `lockup.svg`      | The mark with the wordmark; the wordmark follows the reader's light or dark appearance.                                      |
| `mono.svg`        | One color (`currentColor`), for template images such as the menu-bar icon (`apps/quant/assets/tray/icon.png`).               |
| `icon.svg`        | The macOS app icon: the mark on the 824-pixel rounded square of Apple's 1024-pixel grid. `apps/quant` uses it as `app.icon`. |
| `icon-square.svg` | Full bleed, for platforms that mask the icon themselves (iOS, Android adaptive icons) and social previews.                   |

Colors: violet `#5B4CF5` (lighter `#8F85FF` on the dark icon), coral `#FF7A59` (`#FF8A6B` on the dark icon), and night `#221D5C` to `#110E30`.

To redraw the menu-bar icon after changing the mark:

```sh
sed 's/viewBox="0 0 512 512"/viewBox="58 58 396 396"/' assets/brand/mono.svg > /tmp/tray.svg
rsvg-convert -w 36 -h 36 /tmp/tray.svg -o apps/quant/assets/tray/icon.png
```
