# Quant on a phone

The phone app is a collector. It records where the phone is, which apps were
in front, and its battery and thermal state, and delivers that to your vault.
It has no vault of its own and can read nothing back: it pairs with write
scopes only.

It is the `android` and `ios` targets of the same Leptoskit application as
the desktop window. Nothing has been published; this page covers building it
yourself and running it on an Android phone attached over USB, an iPhone, or
the iOS simulator.

> **Status.** The Android app builds in CI and its Rust side is tested against
> a live daemon. The iPhone app's Xcode project builds in CI for a device,
> unsigned, and has not yet run in a simulator or on an iPhone. Neither has
> been through the device, battery and thermal measurements the
> [deployment plan](deployment-plan.md#10-gates-for-future-product-web-and-mobile-releases)
> requires before a release. Treat both as development builds.

## What it records

| Source | Vault streams | Scope | Needs |
| --- | --- | --- | --- |
| Location | `location.fix` | `location:write` | Location permission |
| App usage | `apps.focus`, `device.state` | `apps:write`, `device:write` | Usage Access (Android) |
| Battery and temperature | `device.battery`, `device.power` | `device:write` | Nothing |

An iPhone records less. iOS gives apps no way to see which other apps were
in front, so it sends no `apps.focus`, only the lock and unlock events in
`device.state`, and only while Quant is running. Its thermal state has no
raw value beside the four levels.

What each stream holds is described in
[the architecture notes](architecture.md#phone-location-app-usage-and-power).
Audio, health and motion are not collected by this app yet.

## Run it on an Android phone attached over USB

You need:

- [Rust](https://rustup.rs) and the Leptoskit CLI at the revision `Cargo.toml`
  pins. `scripts/android-device.sh doctor` prints the exact install command.
- [Android Studio](https://developer.android.com/studio) or its command-line
  tools, with an SDK platform and an NDK (SDK Manager, SDK Tools, NDK). Set
  `ANDROID_HOME` if the SDK is not in Android Studio's default place.
- Java 17 or later, `perl` and `make`. The last two build OpenSSL for the
  phone, which the encrypted spool uses.
- A phone with [USB debugging](https://developer.android.com/studio/debug/dev-options#Enable-debugging)
  switched on, which has allowed this computer.

Then, from the repository root:

```sh
scripts/android-device.sh doctor    # checks the tools and the phone
scripts/android-device.sh           # builds, installs and opens the app
```

The first build compiles the Rust host for `aarch64-linux-android` and takes
several minutes. The script wraps steps you can also run yourself, with the
phone's serial from `leptoskit device list`:

```sh
rustup target add aarch64-linux-android
leptoskit --app-target android build
leptoskit device install --kind android --device SERIAL \
  --path platforms/android/app/build/outputs/apk/debug/app-debug.apk
leptoskit device open --kind android --device SERIAL --app dev.leptoskit.quant
```

The script drives the phone through
[`leptoskit device`](https://leptoskit.dev/docs/guide/19-controlling-devices.html),
the controller Leptoskit's tests and agents use, so an agent can do the same
through `leptoskit mcp serve`. It finds `adb` in the Android SDK even when
`adb` is not on your `PATH`.

To skip the build, download the `quant-android-debug` artifact from the
`application-targets` workflow run of a commit and install it with
`scripts/android-device.sh install path/to/app-debug.apk`.

Other commands:

| Command | What it does |
| --- | --- |
| `screenshot [FILE]` | Saves what the phone shows. |
| `recent [LINES]` | Prints the app's most recent log lines. `logs` follows the log instead. |
| `sanity` | Installs and opens the app, injects a tap, a long press, a pan and a pinch, and takes a screenshot. The gestures land on whatever is on screen, so run it before you pair. |
| `device ARGS...` | Runs any `leptoskit device` command against the phone, such as `device tap --app dev.leptoskit.quant --locator-text Connect`. Steps that find a control need an [Appium](https://appium.io) server with the UI Automator 2 driver. |
| `devices` | Lists what `leptoskit device` can see. |
| `status` | Shows what is installed, running and granted. |
| `grant` | Allows every permission from the computer, for testing. |
| `stop`, `uninstall` | Stops the app, or removes it and everything it stored. |

`scripts/android-device.sh help` lists them all. Screenshots and logs are also
kept under `target/leptoskit-control`. The port forward, typing a link,
`grant`, `status` and `logs` use `adb` directly, because the controller has no
step for them that works without Appium.

## Run it on an iPhone or in the simulator

You need a Mac with:

- [Xcode](https://developer.apple.com/xcode/) and an iOS simulator runtime
  (Xcode, Settings, Components).
- [Rust](https://rustup.rs) and the Leptoskit CLI at the revision `Cargo.toml`
  pins. `scripts/ios-device.sh doctor` prints the exact install command.
- `perl` and `make`, which come with Xcode's command line tools.
- For an iPhone: [Developer Mode](https://developer.apple.com/documentation/xcode/enabling-developer-mode-on-a-device)
  switched on, the iPhone trusting this Mac, and an Apple developer team to
  sign for. A free Apple account has one. Its ID is in Xcode, Settings,
  Accounts.

Then, from the repository root:

```sh
scripts/ios-device.sh doctor                       # checks the tools, simulators and iPhones
scripts/ios-device.sh                              # builds, installs and opens the app
QUANT_IOS_TEAM=ABCDE12345 scripts/ios-device.sh    # the same, on an attached iPhone
```

Without `-s UDID` the script uses the one booted simulator or the one
attached iPhone, and boots the newest iPhone simulator when there is
neither. The first build compiles the Rust host for iOS and takes several
minutes. Simulator builds are signed ad hoc and need no team. The script wraps
steps you can also run yourself, with an id from `leptoskit device list`:

```sh
rustup target add aarch64-apple-ios-sim    # aarch64-apple-ios for an iPhone
leptoskit --app-target ios native build ios-simulator
leptoskit device install --kind ios-simulator --device UDID \
  --path .leptoskit/ios/build/Build/Products/Debug-iphonesimulator/App.app
leptoskit device open --kind ios-simulator --device UDID --app dev.leptoskit.quant
```

For an iPhone, `leptoskit --app-target ios build` writes the Xcode project at
`.leptoskit/ios/Host.xcodeproj` and builds it unsigned, which a phone refuses.
The script then builds it again signed for `QUANT_IOS_TEAM`. You can instead
open the project in Xcode, choose a team under Signing & Capabilities and
press Run. The project is rewritten by every build; change `platforms/ios`
instead.

Other commands, as for Android:

| Command | What it does |
| --- | --- |
| `link [LINK]` | Puts a pairing link on the simulator's clipboard, or on this Mac's for an iPhone to paste through [Universal Clipboard](https://support.apple.com/en-us/102430). |
| `location LAT,LON` | Moves the simulator to a place, so location readings arrive. `location clear` stops it. |
| `grant` | Allows location all the time in the simulator, for testing. |
| `screenshot [FILE]` | Saves what the simulator shows. |
| `recent [LINES]`, `logs` | Prints or follows the app's log in the simulator. For an iPhone, open Console.app and search for `App`. |
| `sanity` | Installs and opens the app and runs a tap, long press, pan and pinch. On iOS these need an [Appium](https://appium.io) server with the XCUITest driver. |
| `device ARGS...` | Runs any `leptoskit device` command against the simulator or iPhone. |
| `boot [UDID]`, `devices` | Boots a simulator, or lists what `leptoskit device` can see. |
| `status`, `stop`, `uninstall` | What is installed and running; stops the app; removes it and everything it stored. |

`scripts/ios-device.sh help` lists them all.

## Pair it with your vault

The daemon has to listen on the network, and the phone has to be able to
reach it.

```sh
quant service install --tcp 0.0.0.0:7717
```

**Over Wi-Fi.** With the phone on the same network, run `quant link`. It
prints a link that starts with `quant://link`. Get that text onto the phone
and paste it into the app, then tap Connect.

**Over the USB cable.** No shared network is needed while the cable is in.
Tap the link field in the app, then:

```sh
scripts/android-device.sh usb
quant link --host 127.0.0.1 | scripts/android-device.sh link
```

The first command forwards the phone's `127.0.0.1:7717` to this computer.
The second types the link into the phone. Tap Connect. A link made this way
stops working when the cable comes out, so pair over Wi-Fi or through a
[relay](relay.md) for a phone you carry around. To change links, tap Forget
this vault and pair again; readings that are waiting stay on the phone.

The app shows a pairing code and the command that approves it. Check the code
matches and run the command on your computer:

```sh
quant approve <code> --scope location:write --scope apps:write --scope device:write
```

Tap "I approved it", or wait a minute for the app to notice.

**In the iOS simulator.** The simulator shares this Mac's network, so the
daemon can listen on loopback. Tap the link field in the app, then:

```sh
quant service install --tcp 127.0.0.1:7717
quant link --host 127.0.0.1 | scripts/ios-device.sh link
```

Paste into the field and tap Connect. An iPhone pairs over Wi-Fi or a relay
like an Android phone; `scripts/ios-device.sh link` puts the link on this
Mac's clipboard for it.

The link carries the vault's certificate fingerprint. The phone talks to
nothing whose certificate does not match it, directly or through a relay, and
refuses any address that does not come as a pairing link.

## Permissions

The app asks for each of these on its own, when you tap its button, and says
what it is for.

| Permission | What happens without it |
| --- | --- |
| Location | No location, and nothing is collected in the background. Android 14 and later only let an app keep a location service running if it holds this permission. |
| Location all the time | Location still arrives while the app's notification is showing. It stops if Android restarts the app in the background. |
| Notifications | The collecting notification is hidden. Collection continues. |
| Usage Access | No app usage or screen and lock events. Android grants this only on its own settings screen; the button opens it. |
| Battery exemption | Android may pause the app to save battery. |

On an iPhone there are two steps, both the system's own prompts:

| Permission | What happens without it |
| --- | --- |
| Location | No location. The app usage and power readings still arrive while Quant is open. |
| Location all the time ("Change to Always Allow") | Location arrives only while Quant is open. With it, updates keep Quant running in the background, and the blue location indicator shows while they do. |

iOS has no notification, Usage Access or battery exemption to ask for, so the
app does not show those steps. It cannot read the "Always" grant without
asking, so it learns it by starting updates for the background and falling
back to the foreground when iOS refuses.

Switch "Collect on this phone" off to stop everything: location updates and
the background service stop, and readings that arrive are discarded.

## What runs where

A Leptoskit native service cannot call a Kotlin plugin; only the screen's
guest can. So the app is three pieces.

- **The screen** (`crates/quant_app`, the `phone` module) is a Wasm guest. It
  asks for permissions, starts the background service and location updates,
  and hands every plugin reply to the service unread. It decodes a reply only
  to learn whether to ask again.
- **The service** (`crates/quant_phone`, registered as `quant-phone`) holds
  the encrypted spool and the phone's signing identity. It decodes the
  replies with the plugin crates' own decoders, queues them through
  `quant_collector`, and uploads them within the battery and thermal budget.
- **The host** (`platforms/android`) is the Gradle project, the manifest, and
  a Rust crate whose dependencies tell the Leptoskit CLI which plugins to
  register. Generated files go to `.leptoskit/android`. On iOS the host is
  `platforms/ios`: Leptoskit's generated `App.swift`, the `Info.plist` with
  the two location usage strings, a UI test, and the same kind of Rust
  crate. `leptoskit build` writes the Xcode project to `.leptoskit/ios` and
  adds what the plugins declare to `Info.plist`: the `location` background
  mode, and the background plugin's `fetch` and `processing` modes and task
  identifiers.

The iPhone app leaves out the `notifications` plugin. Android needs a
foreground service, announced by a notification, before it allows background
location; iOS has no such service, and keeps an app running for as long as it
receives location updates it started for the background. Delivery on an
iPhone is driven by location replies and the screen's one-minute timer, which
run while Quant does.

The `background` plugin does something smaller on an iPhone: it schedules iOS
app refresh, which wakes the service at most every fifteen minutes, and only
when iOS chooses, to deliver what is waiting while Quant is suspended. iOS
ends an app that registers a background task after it has finished
launching, so `platforms/ios/App.swift` registers the plugin's handlers in
`application(_:didFinishLaunchingWithOptions:)`, and the screen's start only
schedules the next refresh. If that registration is missing, the start
answers `Unavailable` and collection carries on without the wakes. The
simulator never runs app refresh; to see one on an iPhone, pause the app in
Xcode's debugger and run
`e -l objc -- (void)[[BGTaskScheduler sharedScheduler] _simulateLaunchForTaskWithIdentifier:@"dev.leptoskit.background.refresh"]`.

`quant_model::phone` is the contract between the screen and the service.

The service does nothing until it is given something: a reading, a command,
a tick from the screen's one-minute timer, or the background service's wake.
On each tick it checks the vault if that is due, then delivers what is
waiting. A failed delivery waits a minute and doubles up to fifteen.

App usage is read in stretches from a watermark the service keeps, so a gap
while the app was closed is filled the next time it runs. The first read
covers the last day. The screen has no clock of its own, so the service
chooses each stretch and stamps every reading.

## What is stored on the phone

Everything is in the app's private data directory.

- `spool/` is the encrypted queue of readings that have not been delivered.
  Its key is made on first launch and kept in the Android Keystore through the
  `secure-store` plugin. If the keystore fails to answer, the app does not
  make a new key, because a new key means discarding readings the old one
  sealed. If the key is truly gone, the screen offers to start storage over.
- `phone.identity.json` is the phone's signing key and client id. It lets the
  phone add to the vault and read nothing.
- `phone.json` is the device id, the pairing link, whether collecting is on,
  and the app usage watermark.

The app opts out of Android backup and device-to-device transfer. A restored
copy would be a second phone claiming to be the first, with a spool it could
not open.

On an iPhone the same files are in the app's Application Support folder, the
spool key is in the keychain for this device only, and the app excludes the
folder from iCloud and computer backups when it starts.

Uninstalling the app deletes all of it. Readings already delivered stay in
your vault. To stop the vault accepting the phone, revoke its grant on the
desktop.

## Check that readings arrive

On the phone, the Delivery card shows how many readings are waiting and when
the vault last took some. On your computer:

```sh
quant series location.fix --from -1h
quant time apps.focus --by app --from today
quant series device.battery --from -6h
```

## Limits

- **The app has to stay in the recent apps list.** The plugins run for as
  long as the app's screen exists, in the foreground or not. Swiping the app
  away ends collection until it is opened again; the notification going away
  is the sign. Collection does not restart after a reboot until you open the
  app.
- **Pairing is by pasted link.** The app cannot scan the QR code `quant link`
  prints or open `quant://` links yet.
- **Background timers are unproven.** Location and delivery are driven by
  plugin replies and the background service's wake, so they do not depend on
  the screen's timer. The periodic app usage read does. If Android holds that
  timer back, app usage catches up from its watermark when the app is next
  on screen.
- **Nothing has been measured on a device**: not battery use, heat, or how
  Android versions and manufacturers treat the background service.
- **The debug build is not signed for distribution.** A release needs the
  upload keystore described in the Leptoskit
  [Android guide](https://leptoskit.dev/docs/platforms/android.html).

## iOS limits

- **Collection stops when Quant is swiped away or the iPhone restarts**,
  until Quant is opened again. iOS relaunches an app in the background for
  significant location changes and visits, which the location plugin
  supports, but the screen that hands readings to the service only exists
  while a window does. Collecting after a relaunch needs the host to start
  the guest without a window first.
- **Continuous updates cost battery.** The app asks for 100-metre accuracy,
  25 metres between fixes, and no automatic pausing, because iOS does not
  resume paused updates while the app is in the background. Visits and
  significant changes would cost less and record less. Nothing has been
  measured on an iPhone.
- **App usage is only lock and unlock**, buffered while Quant runs. Screen
  Time data reaches only report extensions, which cannot send it back to the
  app.
- **Nothing has run in the simulator or on an iPhone.** CI builds the app for
  a device, unsigned. None of the Swift has been exercised, and the UI test in
  `platforms/ios/AppUITests.swift` has not run.
- **Distribution is not set up.** A TestFlight or App Store build needs the
  signing described in the Leptoskit
  [iOS guide](https://leptoskit.dev/docs/platforms/ios.html), privacy
  answers, and an export compliance answer for the encryption the app uses.
