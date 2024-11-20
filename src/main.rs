use actix_files::NamedFile;
use actix_web::error::ErrorInternalServerError;
use actix_web::{get, middleware, App, HttpRequest, HttpResponse, HttpServer, Result};
use askama::Template;
use bytes::Bytes;
use clap::Parser;
use futures::stream::Stream;
use mime_guess::from_path;
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;

const CHUNK_SIZE: usize = 64 * 1024;
const VIDEO_CSS: &str = include_str!(concat!(env!("OUT_DIR"), "/video-js.min.css"));
const VIDEO_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/video.min.js"));

#[derive(Parser, Debug)]
#[command(author, version, about = "Simple HTTP file server")]
struct Args {
	#[arg(short, long, default_value_t = 8080)]
	port: u16,

	#[arg(short = 'o', long = "open", help = "Listen on all interfaces (0.0.0.0)")]
	open: bool,
}

#[allow(dead_code)]
struct VideoStream {
	file: File,
	start: u64,
	end: u64,
	current_pos: u64,
}

impl VideoStream {
	fn new(mut file: File, start: u64, end: u64) -> std::io::Result<Self> {
		if start > end {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"Start position must be less than or equal to end position",
			));
		}

		file.seek(SeekFrom::Start(start))?;

		Ok(Self {
			file,
			start,
			end,
			current_pos: start,
		})
	}
}

impl Stream for VideoStream {
	type Item = Result<Bytes, std::io::Error>;

	fn poll_next(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
		let this = self.get_mut();

		if this.current_pos > this.end {
			return std::task::Poll::Ready(None);
		}

		let remaining = (this.end - this.current_pos + 1) as usize;
		let to_read = remaining.min(CHUNK_SIZE);
		let mut buffer = vec![0; to_read];

		match this.file.read(&mut buffer) {
			Ok(0) => std::task::Poll::Ready(None),
			Ok(n) => {
				this.current_pos += n as u64;
				buffer.truncate(n);
				std::task::Poll::Ready(Some(Ok(Bytes::from(buffer))))
			},
			Err(e) => std::task::Poll::Ready(Some(Err(e))),
		}
	}
}

#[derive(Template)]
#[template(path = "directory.html")]
struct DirectoryTemplate {
	current_path: String,
	parent_path: String,
	has_parent: bool,
	entries: Vec<DirEntry>,
}

#[derive(Serialize)]
struct DirEntry {
	name: String,
	path: String,
	is_dir: bool,
	size: String,
	modified: String,
}

async fn get_dir_entries(path: &Path) -> std::io::Result<Vec<DirEntry>> {
	let mut entries = Vec::new();
	let mut read_dir = tokio::fs::read_dir(path).await?;

	while let Some(entry) = read_dir.next_entry().await? {
		let metadata = entry.metadata().await?;
		let modified = metadata.modified().unwrap_or_else(|_| std::time::SystemTime::now());

		let modified = chrono::DateTime::<chrono::Local>::from(modified)
			.format("%Y-%m-%d %H:%M:%S")
			.to_string();

		let size = if metadata.is_dir() {
			"-".to_string()
		} else {
			humansize::format_size(metadata.len(), humansize::BINARY).to_string()
		};

		let name = entry.file_name().to_string_lossy().into_owned();

		let path = entry
			.path()
			.strip_prefix(".")
			.unwrap_or(&entry.path())
			.to_string_lossy()
			.into_owned();

		entries.push(DirEntry {
			name,
			path,
			is_dir: metadata.is_dir(),
			size,
			modified,
		});
	}

	entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
		(true, false) => std::cmp::Ordering::Less,
		(false, true) => std::cmp::Ordering::Greater,
		_ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
	});

	Ok(entries)
}

fn parse_range(range_str: &str, file_size: u64) -> Option<(u64, u64)> {
	let range = range_str.strip_prefix("bytes=")?;
	let mut parts = range.split('-');

	let start_str = parts.next()?;
	let end_str = parts.next()?;

	let start = if start_str.is_empty() {
		let last_n = end_str.parse::<u64>().ok()?;
		file_size.checked_sub(last_n)?
	} else {
		start_str.parse::<u64>().ok()?
	};

	let end = if end_str.is_empty() {
		file_size - 1
	} else {
		end_str.parse::<u64>().ok()?
	};

	if start <= end && end < file_size {
		Some((start, end))
	} else {
		None
	}
}

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
