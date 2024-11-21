use crate::structs::DirEntry;
use std::path::Path;

pub async fn get_dir_entries(path: &Path) -> std::io::Result<Vec<DirEntry>> {
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

pub fn parse_range(range_str: &str, file_size: u64) -> Option<(u64, u64)> {
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
