use actix_web::Result;
use askama::Template;
use bytes::Bytes;
use clap::Parser;
use futures::stream::Stream;
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::pin::Pin;

use crate::CHUNK_SIZE;

#[derive(Parser, Debug)]
#[command(author, version, about = "Simple HTTP file server")]
pub struct Args {
	#[arg(short, long, default_value_t = 8080)]
	pub port: u16,

	#[arg(short = 'o', long = "open", help = "Listen on all interfaces (0.0.0.0)")]
	pub open: bool,
}

#[allow(dead_code)]
pub struct VideoStream {
	file: File,
	start: u64,
	end: u64,
	current_pos: u64,
}

impl VideoStream {
	pub fn new(mut file: File, start: u64, end: u64) -> std::io::Result<Self> {
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

		let remaining = usize::try_from(this.end - this.current_pos + 1).expect("Value got truncated");
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
pub struct DirectoryTemplate {
	pub current_path: String,
	pub parent_path: String,
	pub has_parent: bool,
	pub entries: Vec<DirEntry>,
}

#[derive(Serialize)]
pub struct DirEntry {
	pub name: String,
	pub path: String,
	pub is_dir: bool,
	pub size: String,
	pub modified: String,
}
