use actix_files::NamedFile;
use actix_web::{get, middleware, web, App, HttpRequest, HttpResponse, HttpServer, Result};
use askama::Template;
use clap::Parser;
use mime::Mime;
use mime_guess::from_path;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use tracing_actix_web::TracingLogger;
use tracing_subscriber;

#[derive(Debug)]
struct TranscodingConfig {
	video_codec: String,
	audio_codec: String,
	crf: u32,
	audio_bitrate: String,
	keyframe_interval: u32,
	segment_duration: f64,
}

impl Default for TranscodingConfig {
	fn default() -> Self {
		Self {
			video_codec: "libsvtav1".to_string(),
			audio_codec: "libopus".to_string(),
			crf: 32,
			audio_bitrate: "128k".to_string(),
			keyframe_interval: 60,
			segment_duration: 2.0,
		}
	}
}

#[derive(Debug)]
struct SegmentInfo {
	number: u32,
	last_accessed: Instant,
	generated: bool,
}

#[allow(dead_code)]
struct DashStream {
	source_path: PathBuf,
	temp_dir: TempDir,
	ffmpeg_process: Option<Child>,
	created_at: SystemTime,
	last_accessed: SystemTime,
	segments: HashMap<u32, SegmentInfo>,
	current_segment: u32,
	max_segments: u32,
	window_size: u32,
	initial_segments: u32,
	config: TranscodingConfig,
}

#[allow(dead_code)]
impl DashStream {
	async fn new(source_path: PathBuf) -> std::io::Result<Self> {
		let temp_dir = tempfile::Builder::new().prefix("dash_stream_").tempdir()?;
		let config = TranscodingConfig::default();

		Ok(Self {
			source_path,
			temp_dir,
			ffmpeg_process: None,
			created_at: SystemTime::now(),
			last_accessed: SystemTime::now(),
			segments: HashMap::new(),
			current_segment: 0,
			max_segments: 30,
			window_size: 5,
			initial_segments: 5,
			config,
		})
	}

	async fn init_stream(&mut self) -> std::io::Result<()> {
		if self.ffmpeg_process.is_none() {
			debug!("Initializing new FFmpeg process for stream");
			let (process, _) = init_dash_pipeline(&self.source_path, &self.temp_dir, &self.config)
				.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

			self.ffmpeg_process = Some(process);

			for i in 0..self.initial_segments {
				self.segments.insert(
					i,
					SegmentInfo {
						number: i,
						last_accessed: Instant::now(),
						generated: false,
					},
				);
			}
		}
		Ok(())
	}

	fn update_last_accessed(&mut self) {
		self.last_accessed = SystemTime::now();
	}

	async fn request_segment(&mut self, segment_number: u32) -> std::io::Result<()> {
		self.update_last_accessed();

		let is_seek = segment_number > self.current_segment + self.max_segments
			|| segment_number < self.current_segment.saturating_sub(self.max_segments);

		if is_seek {
			debug!("Seek detected to segment {}", segment_number);
			self.current_segment = segment_number;
			self.segments.clear();

			let start_segment = segment_number.saturating_sub(2);
			for i in start_segment..=segment_number + self.window_size {
				self.segments.insert(
					i,
					SegmentInfo {
						number: i,
						last_accessed: Instant::now(),
						generated: false,
					},
				);
			}

			self.restart_ffmpeg_at_segment(start_segment).await?;
		} else if !self.segments.contains_key(&segment_number) {
			for i in self.current_segment..=segment_number + self.window_size {
				if !self.segments.contains_key(&i) {
					self.segments.insert(
						i,
						SegmentInfo {
							number: i,
							last_accessed: Instant::now(),
							generated: false,
						},
					);
				}
			}

			if segment_number > self.current_segment {
				self.current_segment = segment_number;
				self.cleanup_old_segments();
				self.ensure_segment_window().await?;
			}
		}

		if let Some(info) = self.segments.get_mut(&segment_number) {
			info.last_accessed = Instant::now();
		}

		Ok(())
	}

	fn cleanup_old_segments(&mut self) {
		let segments_to_remove: Vec<u32> = self
			.segments
			.keys()
			.filter(|&&num| num < self.current_segment.saturating_sub(self.max_segments))
			.copied()
			.collect();

		for segment_num in segments_to_remove {
			self.segments.remove(&segment_num);

			let segment_path = self.temp_dir.path().join(format!("chunk-{}.m4s", segment_num));
			let _ = std::fs::remove_file(segment_path);
		}
	}

	async fn ensure_segment_window(&mut self) -> std::io::Result<()> {
		let target_segment = self.current_segment + self.window_size;

		if self
			.segments
			.iter()
			.any(|(_, info)| !info.generated && info.number <= target_segment)
		{
			if let Some(mut process) = self.ffmpeg_process.take() {
				let _ = process.kill();
				let _ = process.wait();
			}

			let (process, _) =
				init_dash_pipeline_from_segment(&self.source_path, &self.temp_dir, self.current_segment, &self.config)
					.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

			self.ffmpeg_process = Some(process);

			for i in self.current_segment..=target_segment {
				if let Some(info) = self.segments.get_mut(&i) {
					info.generated = true;
				}
			}
		}

		Ok(())
	}

	async fn restart_ffmpeg_at_segment(&mut self, start_segment: u32) -> std::io::Result<()> {
		if let Some(mut process) = self.ffmpeg_process.take() {
			let _ = process.kill();
			let _ = process.wait();
		}

		let (process, _) =
			init_dash_pipeline_from_segment(&self.source_path, &self.temp_dir, start_segment, &self.config)
				.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

		self.ffmpeg_process = Some(process);

		for (_seg_num, info) in self.segments.iter_mut() {
			info.generated = false;
		}

		Ok(())
	}
}

struct StreamManager {
	streams: HashMap<String, DashStream>,
	max_idle_time: Duration,
}

impl StreamManager {
	fn new(max_idle_time: Duration) -> Self {
		Self {
			streams: HashMap::new(),
			max_idle_time,
		}
	}

	async fn get_or_create_stream(&mut self, requested_path: &Path) -> std::io::Result<&mut DashStream> {
		let stream_id = self.get_stream_id(requested_path);

		if !self.streams.contains_key(&stream_id) {
			let source_path = self.find_source_video(requested_path)?;
			let stream = DashStream::new(source_path).await?;
			self.streams.insert(stream_id.clone(), stream);
		}

		let stream = self.streams.get_mut(&stream_id).unwrap();
		stream.update_last_accessed();
		Ok(stream)
	}

	fn get_stream_id(&self, path: &Path) -> String {
		path.with_extension("").to_string_lossy().into_owned()
	}

	fn find_source_video(&self, requested_path: &Path) -> std::io::Result<PathBuf> {
		let base_path = requested_path.with_extension("");
		for ext in &["mp4", "mkv"] {
			let video_path = base_path.with_extension(ext);
			if video_path.exists() {
				return Ok(video_path);
			}
		}
		Err(std::io::Error::new(
			std::io::ErrorKind::NotFound,
			"No matching video file found",
		))
	}

	async fn cleanup_idle_streams(&mut self) {
		let now = SystemTime::now();
		let mut to_remove = Vec::new();

		for (id, stream) in &self.streams {
			if now
				.duration_since(stream.last_accessed)
				.unwrap_or(Duration::from_secs(0))
				> self.max_idle_time
			{
				to_remove.push(id.clone());
			}
		}

		for id in to_remove {
			if let Some(mut stream) = self.streams.remove(&id) {
				if let Some(mut process) = stream.ffmpeg_process.take() {
					let _ = process.kill();
				}
				info!("Removed idle stream: {}", id);
			}
		}
	}
}

struct AppState {
	stream_manager: Arc<Mutex<StreamManager>>,
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Simple HTTP file server")]
struct Args {
	#[arg(short, long, default_value_t = 8080)]
	port: u16,

	#[arg(short = 'o', long = "open", help = "Listen on all interfaces (0.0.0.0)")]
	open: bool,
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

fn init_dash_pipeline_from_segment(
	input_path: &Path,
	temp_dir: &TempDir,
	start_segment: u32,
	config: &TranscodingConfig,
) -> Result<(Child, String), Box<dyn std::error::Error + Send + Sync>> {
	let output_dir = temp_dir.path();
	let input_path_str = input_path
		.to_str()
		.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid input path"))?;

	let segment_duration_sec = config.segment_duration as f64;
	let start_time = start_segment as f64 * segment_duration_sec;

	let segments_dir = output_dir.join("segments");
	std::fs::create_dir_all(&segments_dir)?;

	info!(
		"Starting first pass: Creating WebM segments from start time {}s",
		start_time
	);
	let segment_process = Command::new("ffmpeg")
		.args(&[
			"-ss",
			&start_time.to_string(),
			"-i",
			input_path_str,
			"-map",
			"0:v:0",
			"-map",
			"0:a:0",
			"-c:v",
			&config.video_codec,
			"-c:a",
			&config.audio_codec,
			"-crf",
			&config.crf.to_string(),
			"-b:a",
			&config.audio_bitrate,
			"-bf",
			"0",
			"-g",
			&config.keyframe_interval.to_string(),
			"-sc_threshold",
			"0",
			"-b_strategy",
			"1",
			"-ar",
			"48000",
			"-filter:a",
			"aresample=48000",
			"-f",
			"segment",
			"-segment_time",
			&segment_duration_sec.to_string(),
			"-segment_format",
			"webm",
			"-reset_timestamps",
			"1",
		])
		.arg(segments_dir.join("segment%d.webm").to_str().unwrap())
		.output()?;

	if !segment_process.status.success() {
		let error = String::from_utf8_lossy(&segment_process.stderr);
		error!("FFmpeg segmentation failed: {}", error);
		return Err(Box::new(std::io::Error::new(
			std::io::ErrorKind::Other,
			"FFmpeg segmentation failed",
		)));
	}

	let filelist_path = output_dir.join("filelist.txt");
	let mut filelist = std::fs::File::create(&filelist_path)?;

	for entry in std::fs::read_dir(&segments_dir)? {
		let entry = entry?;
		if entry.file_name().to_string_lossy().starts_with("segment") {
			writeln!(filelist, "file '{}'", entry.path().to_string_lossy())?;
		}
	}

	info!("Starting second pass: Creating DASH stream");
	let playlist_path = output_dir.join("playlist.mpd");

	let ffmpeg_process = Command::new("ffmpeg")
		.args(&[
			"-f",
			"concat",
			"-safe",
			"0",
			"-i",
			filelist_path.to_str().unwrap(),
			"-c",
			"copy",
			"-f",
			"dash",
			"-use_timeline",
			"1",
			"-use_template",
			"1",
			"-init_seg_name",
			"init-stream$RepresentationID$.webm",
			"-media_seg_name",
			"chunk-stream$RepresentationID$-$Number$.webm",
			"-adaptation_sets",
			"id=0,streams=v id=1,streams=a",
		])
		.arg(playlist_path.to_str().unwrap())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|e| {
			error!("Failed to start FFmpeg DASH process: {}", e);
			Box::new(e) as Box<dyn std::error::Error + Send + Sync>
		})?;

	info!(
		"DASH pipeline initialized successfully with PID: {}",
		ffmpeg_process.id()
	);

	Ok((ffmpeg_process, playlist_path.to_string_lossy().to_string()))
}

fn init_dash_pipeline(
	input_path: &Path,
	temp_dir: &TempDir,
	config: &TranscodingConfig,
) -> Result<(Child, String), Box<dyn std::error::Error + Send + Sync>> {
	let output_dir = temp_dir.path();
	let input_path_str = input_path
		.to_str()
		.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid input path"))?;

	let segments_dir = output_dir.join("segments");
	std::fs::create_dir_all(&segments_dir)?;

	info!("Starting first pass: Creating WebM segments");
	let segment_process = Command::new("ffmpeg")
		.args(&[
			"-i",
			input_path_str,
			"-map",
			"0:v:0",
			"-map",
			"0:a:0",
			"-c:v",
			&config.video_codec,
			"-c:a",
			&config.audio_codec,
			"-crf",
			&config.crf.to_string(),
			"-b:a",
			&config.audio_bitrate,
			"-bf",
			"0",
			"-g",
			&config.keyframe_interval.to_string(),
			"-sc_threshold",
			"0",
			"-b_strategy",
			"1",
			"-ar",
			"48000",
			"-filter:a",
			"aresample=48000",
			"-f",
			"segment",
			"-segment_time",
			&config.segment_duration.to_string(),
			"-segment_format",
			"webm",
			"-reset_timestamps",
			"1",
		])
		.arg(segments_dir.join("segment%d.webm").to_str().unwrap())
		.output()?;

	if !segment_process.status.success() {
		let error = String::from_utf8_lossy(&segment_process.stderr);
		error!("FFmpeg segmentation failed: {}", error);
		return Err(Box::new(std::io::Error::new(
			std::io::ErrorKind::Other,
			"FFmpeg segmentation failed",
		)));
	}

	let filelist_path = output_dir.join("filelist.txt");
	let mut filelist = std::fs::File::create(&filelist_path)?;

	for entry in std::fs::read_dir(&segments_dir)? {
		let entry = entry?;
		if entry.file_name().to_string_lossy().starts_with("segment") {
			writeln!(filelist, "file '{}'", entry.path().to_string_lossy())?;
		}
	}

	info!("Starting second pass: Creating DASH stream");
	let playlist_path = output_dir.join("playlist.mpd");

	let ffmpeg_process = Command::new("ffmpeg")
		.args(&[
			"-f",
			"concat",
			"-safe",
			"0",
			"-i",
			filelist_path.to_str().unwrap(),
			"-c",
			"copy",
			"-f",
			"dash",
			"-use_timeline",
			"1",
			"-use_template",
			"1",
			"-init_seg_name",
			"init-stream$RepresentationID$.webm",
			"-media_seg_name",
			"chunk-stream$RepresentationID$-$Number$.webm",
			"-adaptation_sets",
			"id=0,streams=v id=1,streams=a",
		])
		.arg(playlist_path.to_str().unwrap())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|e| {
			error!("Failed to start FFmpeg DASH process: {}", e);
			Box::new(e) as Box<dyn std::error::Error + Send + Sync>
		})?;

	info!("DASH pipeline initialized successfully");
	Ok((ffmpeg_process, playlist_path.to_string_lossy().to_string()))
}

async fn serve_init_segment(stream: &DashStream, init_file: &str, req: &HttpRequest) -> Result<HttpResponse> {
	let init_path = stream.temp_dir.path().join(init_file);
	debug!("Looking for DASH init segment: {:?}", init_path);

	let mut attempts = 0;
	while !init_path.exists() && attempts < 50 {
		tokio::time::sleep(Duration::from_millis(100)).await;
		attempts += 1;
	}

	if init_path.exists() {
		debug!("Serving DASH init segment: {:?}", init_path);
		NamedFile::open(init_path)
			.map(|file| {
				file.set_content_type("video/mp4".parse::<Mime>().unwrap())
					.into_response(req)
			})
			.map_err(|e| {
				error!("Failed to serve init segment: {}", e);
				actix_web::error::ErrorInternalServerError("Failed to serve init segment")
			})
	} else {
		error!("Init segment not generated after {} attempts", attempts);
		Ok(HttpResponse::NotFound().body("Init segment not available"))
	}
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
async fn serve_path(req: HttpRequest, data: web::Data<AppState>) -> Result<HttpResponse> {
	let path: PathBuf = req.match_info().query("path").parse().unwrap_or_default();
	debug!("Requested path: {:?}", path);

	let mut final_path = PathBuf::from(".");

	for component in path.components() {
		match component {
			std::path::Component::Normal(c) => {
				final_path.push(c);
			},
			std::path::Component::CurDir => (),
			_ => {
				warn!("Access denied for path component: {:?}", component);
				return Ok(HttpResponse::NotFound().body("Access denied"));
			},
		}
	}

	if !final_path.exists() {
		if final_path.extension().map_or(false, |ext| ext == "mpd") {
			debug!("MPD file not found, attempting to initialize stream");
			return serve_dash_stream(&final_path, &req, data).await;
		} else {
			error!("Path not found: {:?}", final_path);
			return Ok(HttpResponse::NotFound().body("Not found"));
		}
	}

	if final_path.is_dir() {
		serve_directory(&final_path, &path).await
	} else if final_path.extension().map_or(false, |ext| ext == "mpd" || ext == "m4s") {
		serve_dash_stream(&final_path, &req, data).await
	} else {
		serve_regular_file(&final_path, &req)
	}
}

async fn serve_directory(final_path: &Path, path: &Path) -> Result<HttpResponse> {
	match get_dir_entries(final_path).await {
		Ok(entries) => {
			info!("Serving directory: {:?}", final_path);
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

			let html = template.render().map_err(actix_web::error::ErrorInternalServerError)?;
			Ok(HttpResponse::Ok().content_type("text/html").body(html))
		},
		Err(e) => {
			error!("Failed to read directory: {:?}", e);
			Ok(HttpResponse::InternalServerError().body("Failed to read directory"))
		},
	}
}

fn serve_regular_file(path: &Path, req: &HttpRequest) -> Result<HttpResponse> {
	debug!("Serving regular file: {:?}", path);

	NamedFile::open(path).map_or_else(
		|e| {
			error!("Failed to open file {:?}: {}", path, e);
			Ok(HttpResponse::NotFound().body("File not found"))
		},
		|file| {
			let mime_type = from_path(path).first_or_octet_stream().to_string();
			debug!("File {:?} has mime type: {}", path, mime_type);

			match mime_type.parse() {
				Ok(parsed_mime) => Ok(file.set_content_type(parsed_mime).into_response(req)),
				Err(e) => {
					error!("Failed to parse mime type for file {:?}: {}", path, e);
					Ok(HttpResponse::InternalServerError().body("Failed to determine mime type"))
				},
			}
		},
	)
}

async fn serve_dash_stream(final_path: &Path, req: &HttpRequest, data: web::Data<AppState>) -> Result<HttpResponse> {
	let mut manager = data.stream_manager.lock().await;
	let stream = manager.get_or_create_stream(final_path).await.map_err(|e| {
		error!("Failed to get or create stream: {}", e);
		actix_web::error::ErrorInternalServerError("Failed to initialize stream")
	})?;

	let requested_file = final_path.file_name().unwrap().to_str().unwrap();

	if requested_file.ends_with(".mpd") {
		serve_manifest(stream, req).await
	} else if requested_file.ends_with(".m4s") {
		if requested_file.starts_with("init-") {
			serve_init_segment(stream, requested_file, req).await
		} else {
			serve_segment(stream, requested_file, req).await
		}
	} else {
		Ok(HttpResponse::NotFound().body("Invalid DASH request"))
	}
}

async fn serve_manifest(stream: &mut DashStream, req: &HttpRequest) -> Result<HttpResponse> {
	stream.init_stream().await.map_err(|e| {
		error!("Failed to initialize stream: {}", e);
		actix_web::error::ErrorInternalServerError("Failed to initialize stream")
	})?;

	let manifest_path = stream.temp_dir.path().join("playlist.mpd");
	debug!("Waiting for DASH manifest: {:?}", manifest_path);

	let mut attempts = 0;
	while !manifest_path.exists() && attempts < 50 {
		tokio::time::sleep(Duration::from_millis(100)).await;
		attempts += 1;
	}

	if manifest_path.exists() {
		debug!("Serving DASH manifest: {:?}", manifest_path);
		NamedFile::open(manifest_path)
			.map(|file| {
				file.set_content_type("application/dash+xml".parse::<Mime>().unwrap())
					.into_response(req)
			})
			.map_err(|e| {
				error!("Failed to serve manifest: {}", e);
				actix_web::error::ErrorInternalServerError("Failed to serve manifest")
			})
	} else {
		error!("Manifest file not generated after {} attempts", attempts);
		Ok(HttpResponse::NotFound().body("Manifest not available"))
	}
}

async fn serve_segment(stream: &mut DashStream, segment_name: &str, req: &HttpRequest) -> Result<HttpResponse> {
	let segment_number = segment_name
		.strip_prefix("chunk-")
		.and_then(|s| s.strip_suffix(".m4s"))
		.and_then(|s| s.parse::<u32>().ok())
		.ok_or_else(|| actix_web::error::ErrorBadRequest("Invalid segment name"))?;

	stream.request_segment(segment_number).await.map_err(|e| {
		error!("Failed to manage segment {}: {}", segment_number, e);
		actix_web::error::ErrorInternalServerError("Failed to manage segment")
	})?;

	let segment_path = stream.temp_dir.path().join(segment_name);
	debug!("Looking for DASH segment: {:?}", segment_path);

	let mut attempts = 0;
	while !segment_path.exists() && attempts < 100 {
		tokio::time::sleep(Duration::from_millis(100)).await;
		attempts += 1;
	}

	if segment_path.exists() {
		NamedFile::open(segment_path)
			.map(|file| {
				file.set_content_type("video/mp4".parse::<Mime>().unwrap())
					.into_response(req)
			})
			.map_err(|e| {
				error!("Failed to serve segment: {}", e);
				actix_web::error::ErrorInternalServerError("Failed to serve segment")
			})
	} else {
		error!("Segment {} not generated after {} attempts", segment_number, attempts);
		Ok(HttpResponse::NotFound().body("Segment not available"))
	}
}

async fn cleanup_task(data: web::Data<AppState>) {
	loop {
		sleep(Duration::from_secs(30)).await;
		let mut manager = data.stream_manager.lock().await;
		manager.cleanup_idle_streams().await;
	}
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
	tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

	let args = Args::parse();
	let host = if args.open { "0.0.0.0" } else { "127.0.0.1" };

	info!("Starting DASH streaming server at http://{}:{}", host, args.port);

	let app_state = web::Data::new(AppState {
		stream_manager: Arc::new(Mutex::new(StreamManager::new(Duration::from_secs(300)))),
	});

	let cleanup_state = app_state.clone();
	tokio::spawn(async move {
		cleanup_task(cleanup_state).await;
	});

	HttpServer::new(move || {
		App::new()
			.app_data(app_state.clone())
			.wrap(TracingLogger::default())
			.wrap(middleware::Compress::default())
			.service(serve_path)
	})
	.bind((host, args.port))?
	.run()
	.await
}
