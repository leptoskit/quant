//! watchOS platform host registration generated from this runner's
//! dependencies.
#[cfg(target_os = "watchos")]
include!(concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../.leptoskit/watchos/registrant.rs"
));
