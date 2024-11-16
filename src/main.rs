use actix_files::NamedFile;
use actix_web::error::ErrorInternalServerError;
use actix_web::{get, middleware, App, HttpRequest, HttpResponse, HttpServer, Result};
use askama::Template;
use clap::Parser;
use mime_guess::from_path;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(author, version, about = "Simple HTTP file server")]
struct Args {
	#[arg(short, long, default_value_t = 8080)]
	port: u16,
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
		NamedFile::open(&final_path).map_or_else(
			|_| Ok(HttpResponse::NotFound().body("File not found")),
			|file| {
				let mime_type = from_path(&final_path).first_or_octet_stream().to_string();
				Ok(file
					.set_content_type(mime_type.parse().expect("Failed to parse mime type"))
					.into_response(&req))
			},
		)
	}
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
	let args = Args::parse();
	println!("Starting server at http://localhost:{}", args.port);

	HttpServer::new(|| App::new().wrap(middleware::Compress::default()).service(serve_path))
		.bind(("127.0.0.1", args.port))?
		.run()
		.await
}
