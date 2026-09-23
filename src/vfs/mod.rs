use crossbeam_channel::Receiver;
use std::{
	io::Result,
	path::{Path, PathBuf},
	sync::Mutex,
	time::Instant,
};

use self::{mem_backend::MemBackend, std_backend::StdBackend};
use crate::lock;

pub mod debouncer;
pub mod mem_backend;
pub mod std_backend;

#[derive(Debug, Clone)]
pub enum VfsEvent {
	Create(PathBuf),
	Delete(PathBuf),
	Write(PathBuf),
}

pub trait VfsBackend: Send {
	fn read(&self, path: &Path) -> Result<Vec<u8>>;
	fn read_to_string(&self, path: &Path) -> Result<String>;
	fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

	fn write(&mut self, path: &Path, contents: &[u8]) -> Result<()>;
	fn create_dir(&mut self, path: &Path) -> Result<()>;
	fn rename(&mut self, from: &Path, to: &Path) -> Result<()>;
	fn remove(&mut self, path: &Path) -> Result<()>;

	fn exists(&self, path: &Path) -> bool;
	fn is_dir(&self, path: &Path) -> bool;
	fn is_file(&self, path: &Path) -> bool;

	fn watch(&mut self, path: &Path, recursive: bool) -> Result<()>;
	fn unwatch(&mut self, path: &Path) -> Result<()>;
	fn pause(&mut self);
	fn resume(&mut self);

	fn receiver(&self) -> Receiver<VfsEvent>;

	/// When the watcher's debouncer last completed a flush pass (`None` for
	/// backends without a debouncing watcher)
	fn last_flush(&self) -> Option<Instant> {
		None
	}

	/// Watcher events dropped as echoes of the processor's own writes
	fn suppressed_events(&self) -> u64 {
		0
	}

	/// Watcher events held until the processor's write window closes
	fn held_events(&self) -> usize {
		0
	}
}

impl VfsEvent {
	pub fn path(&self) -> &Path {
		match self {
			VfsEvent::Create(path) | VfsEvent::Delete(path) | VfsEvent::Write(path) => path.as_ref(),
		}
	}
}

pub struct Vfs {
	inner: Mutex<Box<dyn VfsBackend>>,
}

impl Vfs {
	pub fn new(watch: bool) -> Self {
		Self {
			inner: Mutex::new(Box::new(StdBackend::new(watch))),
		}
	}

	pub fn new_virtual() -> Self {
		Self {
			inner: Mutex::new(Box::new(MemBackend::new())),
		}
	}

	pub fn read(&self, path: &Path) -> Result<Vec<u8>> {
		lock!(self.inner).read(path)
	}

	pub fn read_to_string(&self, path: &Path) -> Result<String> {
		lock!(self.inner).read_to_string(path)
	}

	pub fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
		lock!(self.inner).read_dir(path)
	}

	pub fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
		lock!(self.inner).write(path, contents)
	}

	pub fn create_dir(&self, path: &Path) -> Result<()> {
		lock!(self.inner).create_dir(path)
	}

	pub fn rename(&self, from: &Path, to: &Path) -> Result<()> {
		lock!(self.inner).rename(from, to)
	}

	pub fn remove(&self, path: &Path) -> Result<()> {
		lock!(self.inner).remove(path)
	}

	pub fn exists(&self, path: &Path) -> bool {
		lock!(self.inner).exists(path)
	}

	pub fn is_dir(&self, path: &Path) -> bool {
		lock!(self.inner).is_dir(path)
	}

	pub fn is_file(&self, path: &Path) -> bool {
		lock!(self.inner).is_file(path)
	}

	pub fn watch(&self, path: &Path, recursive: bool) -> Result<()> {
		lock!(self.inner).watch(path, recursive)
	}

	pub fn unwatch(&self, path: &Path) -> Result<()> {
		lock!(self.inner).unwatch(path)
	}

	pub fn pause(&self) {
		lock!(self.inner).pause()
	}

	pub fn resume(&self) {
		lock!(self.inner).resume()
	}

	pub fn receiver(&self) -> Receiver<VfsEvent> {
		lock!(self.inner).receiver()
	}

	/// See [`VfsBackend::last_flush`]
	pub fn last_flush(&self) -> Option<Instant> {
		lock!(self.inner).last_flush()
	}

	/// Watcher events handed to the processor but not yet processed
	pub fn pending_events(&self) -> usize {
		self.receiver().len()
	}

	/// See [`VfsBackend::suppressed_events`]
	pub fn suppressed_events(&self) -> u64 {
		lock!(self.inner).suppressed_events()
	}

	/// See [`VfsBackend::held_events`]
	pub fn held_events(&self) -> usize {
		lock!(self.inner).held_events()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{fs, time::Duration};

	/// An edit that lands while the processor is writing is held and
	/// delivered afterwards; the processor's own write is dropped as its echo
	#[test]
	fn edits_during_a_sync_write_are_delivered_and_echoes_are_not() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();

		fs::write(root.join("mine.luau"), "old").unwrap();
		fs::write(root.join("theirs.luau"), "old").unwrap();

		let vfs = Vfs::new(true);
		vfs.watch(&root, true).unwrap();

		let receiver = vfs.receiver();

		// Let the watcher settle, then drain anything the setup produced
		std::thread::sleep(Duration::from_millis(500));
		while receiver.try_recv().is_ok() {}

		vfs.pause();
		vfs.write(&root.join("mine.luau"), b"written by the processor").unwrap();
		fs::write(root.join("theirs.luau"), "an agent's edit").unwrap();
		std::thread::sleep(Duration::from_millis(400));
		vfs.resume();

		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		let mut seen = Vec::new();

		while std::time::Instant::now() < deadline {
			if let Ok(event) = receiver.recv_timeout(Duration::from_millis(100)) {
				seen.push(event.path().to_owned());
			}

			if seen.iter().any(|path| path.ends_with("theirs.luau")) {
				// Give a straggling echo the chance to show up too
				std::thread::sleep(Duration::from_millis(300));

				while let Ok(event) = receiver.try_recv() {
					seen.push(event.path().to_owned());
				}

				break;
			}
		}

		assert!(
			seen.iter().any(|path| path.ends_with("theirs.luau")),
			"the edit made during the sync write was never delivered: {seen:?}"
		);
		assert!(
			!seen.iter().any(|path| path.ends_with("mine.luau")),
			"the processor's own write came back as an event: {seen:?}"
		);
		assert!(vfs.suppressed_events() >= 1);
		assert_eq!(vfs.held_events(), 0);
	}
}
