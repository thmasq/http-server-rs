mod helpers;
mod structs;

use actix_files::NamedFile;
use actix_web::error::ErrorInternalServerError;
use actix_web::{get, middleware, App, HttpRequest, HttpResponse, HttpServer, Result};
use askama::Template;
use clap::Parser;
use helpers::{get_dir_entries, parse_range};
use mime_guess::from_path;
use std::fs::File;
use std::path::{Path, PathBuf};
use structs::{Args, DirectoryTemplate, VideoStream};

const CHUNK_SIZE: usize = 64 * 1024;
const VIDEO_CSS: &str = include_str!(concat!(env!("OUT_DIR"), "/video-js.min.css"));
const VIDEO_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/video.min.js"));

#[allow(clippy::future_not_send)]
#[get("/{path:.*}")]
async fn serve_path(req: HttpRequest) -> Result<HttpResponse> {
	let path: PathBuf = req.match_info().query("path").parse().unwrap_or_default();
	let mut final_path = PathBuf::from(".");

	for component in path.components() {
		match component {
			std::path::Component::Normal(c) => final_path.push(c),
			std::path::Component::CurDir => (),
			_ => return Ok(HttpResponse::NotFound().body("Access denied")),
		}
	}

	if !final_path.exists() {
		return Ok(HttpResponse::NotFound().body("Not found"));
	}

	if final_path.is_dir() {
		match get_dir_entries(&final_path).await {
			Ok(entries) => {
				let current_path = path.to_string_lossy().to_string();
				let parent_path = Path::new(&current_path)
					.parent()
					.map(|p| p.to_string_lossy().to_string())
					.unwrap_or_default();
				let template = DirectoryTemplate {
					current_path,
					parent_path,
					has_parent: !path.as_os_str().is_empty(),
					entries,
				};
				let html = template.render().map_err(ErrorInternalServerError)?;
				Ok(HttpResponse::Ok().content_type("text/html").body(html))
			},
			Err(_) => Ok(HttpResponse::InternalServerError().body("Failed to read directory")),
		}
	} else {
		let Ok(file) = File::open(&final_path) else {
			return Ok(HttpResponse::NotFound().body("File not found"));
		};

		let mime_type = from_path(&final_path).first_or_octet_stream().to_string();
		let file_size = file.metadata()?.len();

		if let Some(range_header) = req.headers().get("range") {
			let range_str = range_header.to_str().map_err(ErrorInternalServerError)?;
			if let Some(range) = parse_range(range_str, file_size) {
				let (start, end) = range;
				let content_length = end - start + 1;

				let stream = VideoStream::new(file, start, end).map_err(ErrorInternalServerError)?;

				return Ok(HttpResponse::PartialContent()
					.append_header(("Content-Type", mime_type))
					.append_header(("Content-Length", content_length.to_string()))
					.append_header(("Content-Range", format!("bytes {start}-{end}/{file_size}")))
					.append_header(("Accept-Ranges", "bytes"))
					.streaming(stream));
			}
		}

		Ok(NamedFile::open(&final_path)?.into_response(&req))
	}
}

#[get("/_static/video-js.min.css")]
async fn serve_css() -> HttpResponse {
	HttpResponse::Ok().content_type("text/css").body(VIDEO_CSS)
}

#[get("/_static/video.min.js")]
async fn serve_js() -> HttpResponse {
	HttpResponse::Ok().content_type("application/javascript").body(VIDEO_JS)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
	let args = Args::parse();
	let host = if args.open { "0.0.0.0" } else { "127.0.0.1" };
	println!("Starting server at http://{}:{}", host, args.port);

	HttpServer::new(|| {
		App::new()
			.wrap(middleware::Compress::default())
			.service(serve_css)
			.service(serve_js)
			.service(serve_path)
	})
	.bind((host, args.port))?
	.run()
	.await
}
