//! Decodes the menu-bar icon at build time: the tray takes raw RGBA and never
//! decodes images at run time.

use std::path::PathBuf;

fn main() {
	let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/app/tray/icon.png");
	println!("cargo::rerun-if-changed={}", source.display());
	let file = std::fs::File::open(&source).expect("assets/tray/icon.png exists");
	let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
	decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
	let mut reader = decoder.read_info().expect("the tray icon is a PNG");
	let mut pixels = vec![0; reader.output_buffer_size().expect("a small icon")];
	let frame = reader
		.next_frame(&mut pixels)
		.expect("the tray icon decodes");
	assert_eq!(
		frame.color_type,
		png::ColorType::Rgba,
		"the tray icon must decode to RGBA"
	);
	pixels.truncate(frame.buffer_size());
	let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
	std::fs::write(out.join("tray_icon.rgba"), &pixels).expect("write the decoded icon");
	std::fs::write(
		out.join("tray_icon.rs"),
		format!(
			"pub const WIDTH: u32 = {};\npub const HEIGHT: u32 = {};\n",
			frame.width, frame.height
		),
	)
	.expect("write the icon size");
}
