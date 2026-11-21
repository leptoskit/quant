# Quant on watches and wearables

Quant treats every watch, fitness band, chest strap, and smart ring as a
write-only collector for your private vault. Nothing on a watch can read back
from the vault, and every `health.*` stream carries the `health` sensitivity
class so agents approved with `--deny health` never see biometric data.

> **Status.** The Rust collector adapters, encrypted spool, pinned-TLS uploader,
> companion sync envelopes, BLE GATT decoders, and `WearableApp` watch surfaces
> are tested in CI against a live `quantd` daemon. Native watch packaging and
> on-hardware battery/thermal measurements on Apple Watch, Wear OS / Galaxy
> Watch, and Huawei Watch remain development builds until the gates in the
> [deployment plan](deployment-plan.md#10-gates-for-future-product-web-and-mobile-releases)
> pass.

## Supported wearable targets

| Device family | Quant target | Host directory | Sensors & APIs |
| --- | --- | --- | --- |
| Wear OS (Pixel Watch, Samsung Galaxy Watch / One UI Watch, TicWatch, Wear OS 3+) | `wearos` | `platforms/wearos` | Wear Health Services (`PassiveMonitoringClient` / `MeasureClient`), Health Connect, `SensorManager`, Wear Data & Message Layer, BLE GATT, Tiles, Complications |
| Apple Watch (watchOS 10+) | `watchos` | `platforms/watchos` | `HealthKit` (`HKWorkoutSession`, `HKLiveWorkoutBuilder`, `HKObserverQuery`), `CoreMotion` pedometer & motion, `WatchConnectivity`, `CoreBluetooth` BLE GATT, `WidgetKit` Smart Stack glances & complications |
| Huawei Watch (HarmonyOS Wearable) | `harmonyos-watch` | `platforms/harmonyos` | `@kit.SensorServiceKit`, `@kit.HealthServiceKit`, `@kit.WearEngine`, `@kit.ConnectivityKit` BLE GATT |
| Phone Health stores (iPhone, Android, Huawei phones) | `ios`, `android`, `harmonyos` | `platforms/{ios,android,harmonyos}` | Incremental `HealthKit` / Health Connect / HarmonyOS Health reads + companion watch sync |
| Generic BLE GATT wearables (Polar, Garmin, Wahoo, Oura/rings, oximeters, thermometers) | Any phone or watch target | `leptoskit_plugin_ble` | Standard SIG GATT decoders (`0x2A37` HR + RR/RMSSD, `0x2A5E`/`0x2A5F` SpO2, `0x2A1C` temperature, `0x2A19` peripheral battery, `0x2A53` running cadence, `0x2A63` cycling power) |

## Sensor-first watch architecture (`WearableApp`)

Watches spend almost all of their time with the screen off. Instead of shrinking
phone cards onto a circular face, Quant declares its watch app in
`crates/quant_app/src/watch.rs` using `leptoskit_watch::WearableApp`:

1. **Declare streams first** (`QUANT_WATCH_STREAMS`: heart rate, resting HR,
   HRV, SpO2, skin temperature, respiratory rate, steps, floors, calories,
   distance, sleep, workouts, wear state, stress, body composition, motion,
   location, battery, thermal, and BLE sensors).
2. **Choose sampling & delivery policies** (`SamplingPolicy::Passive` +
   `DeliveryMode::DirectWithCompanionFallback`):
   - **Standalone direct**: the watch keeps its own encrypted SQLCipher spool
     and Ed25519 identity and uploads directly to `quantd` over pinned TLS
     (LAN or relay) when on Wi-Fi or LTE.
   - **Companion phone sync**: the phone app can push its vault pairing link to
     the watch (`watch-sync` topic `/quant/pair`), and when the watch has no
     direct network path it ferries readings through the paired phone
     (`/quant/reading`, attributed to the watch's own `DeviceId`).
3. **Auto-generated glanceable surfaces**: `WearableApp::render_status_app`
   projects `quant_model::phone::Snapshot` into a budget-validated watch page,
   tile/glance, and circular face complication, while `WatchScreen` renders a
   scrollable high-contrast wrist view (live BPM, steps, queued count, battery
   percentage, Pause/Record, Sync, Start/End live workout, Allow sensors).

## Vault streams and scopes

| Stream | Kind | Scope | Source |
| --- | --- | --- | --- |
| `health.heart_rate` | series (`bpm`) | `health:write` | Watch optical sensor, Health store, BLE `0x2A37` / `0x2A5E` |
| `health.resting_heart_rate` | series (`bpm`) | `health:write` | Health store |
| `health.hrv` | series (`rmssd` ms) | `health:write` | Watch IBI/RMSSD, Health Connect RMSSD, BLE `0x2A37` RR intervals |
| `health.hrv_sdnn` | series (`sdnn` ms) | `health:write` | Apple Watch / `HealthKit` HRV SDNN |
| `health.spo2` | series (`percent`) | `health:write` | Watch SpO2, Health store, BLE `0x2A5E`/`0x2A5F` |
| `health.skin_temperature` | series (`celsius`) | `health:write` | Watch wrist temp, Health store, BLE `0x2A1C` |
| `health.respiratory_rate` | series (`breaths_per_min`) | `health:write` | Health store |
| `health.steps` | series (`steps`) | `health:write` | Watch passive steps, Health store, pedometer deltas |
| `health.floors` | series (`floors`) | `health:write` | Watch / Health store |
| `health.active_calories` | series (`kcal`) | `health:write` | Watch / Health store |
| `health.distance` | series (`meters`) | `health:write` | Watch / Health store |
| `health.sleep` | event (`stage`) | `health:write` | Health store sleep stages (`asleep`, `light`, `deep`, `rem`, `awake`, `in_bed`) |
| `health.workout` | event (`activity`) | `health:write` | Health store exercise sessions |
| `health.stress` | series (`score`) | `health:write` | Watch stress sensor |
| `health.body_composition` | event | `health:write` | BIA / smart scales |
| `health.ecg` | event (`classification`) | `health:write` | Single-lead watch ECG summaries |
| `motion.accelerometer`, `motion.gyroscope`, `motion.barometer` | series | `motion:write` | Phone & watch motion sensors |
| `device.wear_state` | event (`on_wrist`/`off_wrist`) | `device:write` | Watch off-body detector |
| `sensor.ble.running_cadence`, `sensor.ble.cycling_power`, `sensor.ble.battery` | series | `sensor:write` | BLE GATT fitness sensors and peripheral battery |

## Development builds

```sh
# Wear OS (Pixel Watch, Samsung Galaxy Watch, Wear OS emulator)
scripts/wearos-device.sh doctor
scripts/wearos-device.sh

# Apple Watch (macOS + Xcode + watchOS SDK)
scripts/watchos-device.sh doctor
scripts/watchos-device.sh

# Huawei Watch (HarmonyOS SDK + DevEco + hdc)
scripts/harmonyos-device.sh doctor
scripts/harmonyos-device.sh
```
