use crossbeam_channel::Receiver;
use std::{
	fs,
	io::{Error, Result},
	path::{Path, PathBuf},
	time::Instant,
};

use super::{debouncer::VfsDebouncer, VfsBackend, VfsEvent};
use crate::config::Config;

pub struct StdBackend {
	watching: bool,
	debouncer: VfsDebouncer,
	watched_paths: Vec<PathBuf>,
}

impl StdBackend {
	pub fn new(watch: bool) -> Self {
		Self {
			watching: watch,
			debouncer: VfsDebouncer::new(),
			watched_paths: Vec::new(),
		}
	}
}

impl VfsBackend for StdBackend {
	fn read(&self, path: &Path) -> Result<Vec<u8>> {
		fs::read(path)
	}

	fn read_to_string(&self, path: &Path) -> Result<String> {
		let contents = fs::read_to_string(path)?;

		if Config::new().ignore_line_endings && contents.contains('\r') {
			return Ok(contents.replace("\r\n", "\n").replace("\r", "\n"));
		}

		Ok(contents)
	}

	fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
		let mut paths = Vec::new();

		for entry in fs::read_dir(path)? {
			paths.push(entry?.path());
		}

		Ok(paths)
	}

	// Every write through the VFS is the processor's own: noted so the event
	// it causes is dropped as an echo instead of being synced back

	fn write(&mut self, path: &Path, contents: &[u8]) -> Result<()> {
		self.debouncer.note_self_write(path);
		fs::write(path, contents)
	}

	fn create_dir(&mut self, path: &Path) -> Result<()> {
		self.debouncer.note_self_write(path);
		fs::create_dir_all(path)
	}

	fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
		self.debouncer.note_self_write(from);
		self.debouncer.note_self_write(to);
		fs::rename(from, to)
	}

	fn remove(&mut self, path: &Path) -> Result<()> {
		self.debouncer.note_self_write(path);
		self.unwatch(path)?;

		if Config::new().move_to_bin {
			trash::delete(path).map_err(Error::other)
		} else if path.is_dir() {
			fs::remove_dir_all(path)
		} else {
			fs::remove_file(path)
		}
	}

	fn exists(&self, path: &Path) -> bool {
		path.exists()
	}

	fn is_dir(&self, path: &Path) -> bool {
		path.is_dir()
	}

	fn is_file(&self, path: &Path) -> bool {
		path.is_file()
	}

	fn watch(&mut self, path: &Path, recursive: bool) -> Result<()> {
		let path = path.to_owned();

		if !self.watching || self.watched_paths.iter().any(|p| path.starts_with(p)) {
			return Ok(());
		}

		self.debouncer.watch(&path, recursive)?;
		self.watched_paths.push(path);

		Ok(())
	}

	fn unwatch(&mut self, path: &Path) -> Result<()> {
		if !self.watching {
			return Ok(());
		}

		let path = path.to_owned();

		self.watched_paths.retain(|p| {
			let unwatch = p.starts_with(&path);

			if unwatch {
				self.debouncer.unwatch(p).ok();
			}

			!unwatch
		});

		Ok(())
	}

	fn pause(&mut self) {
		self.debouncer.pause()
	}

	fn resume(&mut self) {
		self.debouncer.resume()
	}

	fn receiver(&self) -> Receiver<VfsEvent> {
		self.debouncer.receiver()
	}

	fn last_flush(&self) -> Option<Instant> {
		self.debouncer.last_flush()
	}

	fn suppressed_events(&self) -> u64 {
		self.debouncer.suppressed()
	}

	fn held_events(&self) -> usize {
		self.debouncer.held()
	}
}
