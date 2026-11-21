//! HarmonyOS phone and Huawei Watch host registration.
#[cfg(target_env = "ohos")]
include!(concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../.leptoskit/harmonyos/registrant.rs"
));
