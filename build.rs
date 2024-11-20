use std::fs;
use std::io::Write;
use std::path::Path;

fn main() {
	let out_dir = std::env::var("OUT_DIR").unwrap();
	let css_url = "https://unpkg.com/video.js/dist/video-js.min.css";
	let js_url = "https://unpkg.com/video.js/dist/video.min.js";

	let css_path = Path::new(&out_dir).join("video-js.min.css");
	let js_path = Path::new(&out_dir).join("video.min.js");

	download_file(css_url, &css_path);
	download_file(js_url, &js_path);

	println!("cargo:rerun-if-changed=build.rs");
}

fn download_file(url: &str, dest_path: &Path) {
	let response = reqwest::blocking::get(url).expect("Failed to download file");
	assert!(
		response.status().is_success(),
		"Failed to download: {} (status: {})",
		url,
		response.status()
	);

	let mut file = fs::File::create(dest_path).expect("Failed to create file");
	file.write_all(&response.bytes().expect("Failed to read file bytes"))
		.expect("Failed to write file");
}
