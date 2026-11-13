# Reviewing the interface

The real macOS UI can be built with disconnected, illustrative data. This is
for design review and screenshots; no sample records are written to a vault.
The preview has a visible label and its action handler changes only its local
state. It does not follow the live service or invoke biometric authentication.

Use the CLI matching the root SDK pin:

```sh
leptoskit --app-target desktop-preview build
```

Run the app with isolated local paths and capture its own native views:

```sh
QUANT_HOME=/tmp/quant-ui-preview-vault \
QUANT_CONFIG=/tmp/quant-ui-preview-clients \
QUANT_SNAPSHOT="$PWD/test-results/ui-review/desktop" \
QUANT_SNAPSHOT_APPEARANCE=light \
QUANT_SNAPSHOT_DELAY=1 \
QUANT_SNAPSHOT_STEPS='capture:overview;press:Saved moments;capture:moments;press:Agents;capture:agents;press:Audit;capture:audit' \
.leptoskit/macos/App.app/Contents/MacOS/App
```

The host still has a resident service, isolated by the paths above. The guest
uses only the `ui-preview` fixture. Captures are PNGs of the actual native
interface, not HTML mockups. Use `QUANT_SNAPSHOT_APPEARANCE=dark` to capture
the dark palette. These settings affect this app process only.

The preview and desktop targets currently write to the same generated macOS
bundle. Rebuild with `leptoskit --app-target desktop build --release` before
any real packaging or distribution, then verify the preview label is absent.
Never publish an artifact built with `ui-preview` enabled. See the
[deployment plan](deployment-plan.md).

For the public website, use `leptoskit --app-target landing dev` and review
both color themes, phone and desktop widths, menu navigation, privacy routes,
FAQ and the memory demo. Screenshots and synthetic state are illustrations,
not evidence that a user's data has been recorded or analyzed.
