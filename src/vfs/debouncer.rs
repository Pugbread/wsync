use crossbeam_channel::Receiver;
use log::{debug, trace, warn};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, FileIdMap};
use std::{
	collections::HashMap,
	io::{self, Result},
	path::{Path, PathBuf},
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex, RwLock,
	},
	thread,
	time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use notify::event::DataChange;

#[cfg(not(target_os = "windows"))]
use notify::event::ModifyKind;

#[cfg(target_os = "linux")]
use notify::event::{AccessKind, AccessMode, RenameMode};

use super::VfsEvent;
use crate::constants::SYNCBACK_DEBOUNCE_TIME;

#[cfg(target_os = "linux")]
const DEBOUNCE_TIME: Duration = Duration::from_micros(500);

/// How long the debouncer holds a raw watcher event before emitting it. A
/// flush pass that started at `t` has emitted every event that arrived at or
/// before `t - DEBOUNCE_TIMEOUT` (see [`VfsDebouncer::last_flush`])
pub const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(100);

macro_rules! event_path {
	($event:expr) => {
		$event.paths.first().unwrap().to_owned()
	};
}

#[cfg(target_os = "linux")]
struct DebounceContext {
	time: Instant,
	path: PathBuf,
}

/// How often held events are checked for release
const HELD_POLL: Duration = Duration::from_millis(50);

/// How long a path the processor wrote itself is remembered as a likely echo
const SELF_WRITE_MEMORY: Duration = Duration::from_secs(10);

/// Events that arrived while the processor was writing, waiting for the write
/// window (pause + [`SYNCBACK_DEBOUNCE_TIME`]) to close, and the paths the
/// processor wrote meanwhile
#[derive(Default)]
struct Held {
	events: Vec<VfsEvent>,
	self_writes: HashMap<PathBuf, Instant>,
}

impl Held {
	/// An event is an echo when it concerns a path the processor wrote (or
	/// something inside a directory it wrote)
	fn is_echo(&self, path: &Path) -> bool {
		path.ancestors().any(|ancestor| self.self_writes.contains_key(ancestor))
	}
}

pub struct VfsDebouncer {
	inner: Debouncer<RecommendedWatcher, FileIdMap>,
	pause_state: Arc<RwLock<(bool, Instant)>>,
	receiver: Receiver<VfsEvent>,
	suppressed: Arc<AtomicU64>,
	held: Arc<Mutex<Held>>,
}

impl VfsDebouncer {
	pub fn new() -> Self {
		let (sender, receiver) = crossbeam_channel::unbounded();

		let pause_state = Arc::new(RwLock::new((false, Instant::now())));
		let local_pause_state = pause_state.clone();
		let suppressed = Arc::new(AtomicU64::new(0));
		let held = Arc::new(Mutex::new(Held::default()));
		let local_held = held.clone();

		#[cfg(target_os = "linux")]
		let mut context = DebounceContext {
			time: Instant::now(),
			path: PathBuf::new(),
		};

		// Translation runs inside the debouncer's own flush pass (not on a
		// relay thread), so once a pass completes its events are already in
		// the processor's channel — which is what lets `wait-synced` observe
		// the debouncer as drained instead of guessing with a sleep
		let release_sender = sender.clone();

		let handler = move |result: DebounceEventResult| {
			let events = match result {
				Ok(events) => events,
				Err(errors) => {
					warn!("File watcher reported errors: {errors:?}");
					return;
				}
			};

			let (is_paused, timestamp) = *local_pause_state.read().unwrap();
			let writing = is_paused || timestamp.elapsed() < SYNCBACK_DEBOUNCE_TIME;

			// While the processor writes (a Studio change, a connect apply),
			// events are held rather than dropped: most are the echo of that
			// write, but an unrelated edit landing in the same window is real
			// work. The release thread below replays what is not an echo once
			// the window closes
			let mut held = writing.then(|| local_held.lock().unwrap());

			for event in events {
				trace!("Debouncing event, paths: {:?}, kind: {:?}", event.paths, event.kind);

				#[cfg(not(target_os = "linux"))]
				let translated = debounce(&event);

				#[cfg(target_os = "linux")]
				let translated = debounce(&event, &mut context);

				if let Some(translated) = translated {
					match held.as_mut() {
						Some(held) => held.events.push(translated),
						None => {
							sender.send(translated).ok();
						}
					}
				}
			}
		};

		// Releases held events once the write window has closed: echoes of the
		// processor's own writes are dropped (and counted), everything else is
		// delivered as if it had just arrived. The processor re-reads disk for
		// each event, so a late delivery sees the file as it is now
		{
			let pause_state = pause_state.clone();
			let held = held.clone();
			let suppressed = suppressed.clone();
			let sender = release_sender;

			thread::Builder::new()
				.name("vfs-held".into())
				.spawn(move || loop {
					thread::sleep(HELD_POLL);

					let (is_paused, timestamp) = *pause_state.read().unwrap();

					if is_paused || timestamp.elapsed() < SYNCBACK_DEBOUNCE_TIME {
						continue;
					}

					let mut held = held.lock().unwrap();

					held.self_writes
						.retain(|_, written| written.elapsed() < SELF_WRITE_MEMORY);

					if held.events.is_empty() {
						continue;
					}

					let events = std::mem::take(&mut held.events);
					let mut released = 0;
					let mut echoes = 0;

					for event in events {
						if held.is_echo(event.path()) {
							echoes += 1;
						} else {
							released += 1;
							sender.send(event).ok();
						}
					}

					suppressed.fetch_add(echoes, Ordering::Relaxed);

					if released > 0 {
						debug!(
							"Released {released} file event(s) held during a sync write ({echoes} echo(es) dropped)"
						);
					}
				})
				.expect("failed to spawn the held-event thread");
		}

		let debouncer = new_debouncer(DEBOUNCE_TIMEOUT, None, handler, false).unwrap();

		Self {
			inner: debouncer,
			pause_state,
			receiver,
			suppressed,
			held,
		}
	}

	pub fn watch(&mut self, path: &Path, recursive: bool) -> Result<()> {
		let recursive = if recursive {
			RecursiveMode::Recursive
		} else {
			RecursiveMode::NonRecursive
		};

		self.inner.watcher().watch(path, recursive).map_err(map_error)?;
		self.inner.cache().add_root(path, recursive);

		Ok(())
	}

	pub fn unwatch(&mut self, path: &Path) -> Result<()> {
		self.inner.watcher().unwatch(path).map_err(map_error)?;
		self.inner.cache().remove_root(path);

		Ok(())
	}

	pub fn pause(&mut self) {
		*self.pause_state.write().unwrap() = (true, Instant::now());
	}

	pub fn resume(&mut self) {
		*self.pause_state.write().unwrap() = (false, Instant::now());
	}

	pub fn receiver(&self) -> Receiver<VfsEvent> {
		self.receiver.clone()
	}

	/// When the debouncer's most recent completed flush pass started. Every
	/// watcher event that reached the debouncer [`DEBOUNCE_TIMEOUT`] before
	/// it is in (or already through) the processor channel
	pub fn last_flush(&self) -> Option<Instant> {
		self.inner.last_flush()
	}

	/// Watcher events dropped so far as echoes: they arrived while the
	/// processor was writing (or within [`SYNCBACK_DEBOUNCE_TIME`] after) and
	/// concerned a path it wrote. Events on other paths are held and
	/// delivered, not counted here
	pub fn suppressed(&self) -> u64 {
		self.suppressed.load(Ordering::Relaxed)
	}

	/// Events currently held until the write window closes
	pub fn held(&self) -> usize {
		self.held.lock().unwrap().events.len()
	}

	/// Records a path the processor itself is writing, so the event it causes
	/// is recognized as an echo when held events are released
	pub fn note_self_write(&self, path: &Path) {
		self.held
			.lock()
			.unwrap()
			.self_writes
			.insert(path.to_owned(), Instant::now());
	}
}

fn map_error(err: notify::Error) -> io::Error {
	match err.kind {
		notify::ErrorKind::Io(err) => err,
		notify::ErrorKind::PathNotFound => io::Error::new(io::ErrorKind::NotFound, err),
		notify::ErrorKind::WatchNotFound => io::Error::new(io::ErrorKind::NotFound, err),
		_ => io::Error::other(err),
	}
}

#[cfg(target_os = "macos")]
fn debounce(event: &DebouncedEvent) -> Option<VfsEvent> {
	match event.kind {
		EventKind::Create(_) => {
			let path = event_path!(event);

			if path.exists() {
				Some(VfsEvent::Create(path))
			} else {
				None
			}
		}
		EventKind::Remove(_) => Some(VfsEvent::Delete(event_path!(event))),
		EventKind::Modify(kind) => match kind {
			ModifyKind::Name(_) => {
				let path = event_path!(event);

				if path.exists() {
					Some(VfsEvent::Create(path))
				} else {
					Some(VfsEvent::Delete(path))
				}
			}
			ModifyKind::Data(kind) => {
				if kind == DataChange::Content {
					Some(VfsEvent::Write(event_path!(event)))
				} else {
					None
				}
			}
			_ => None,
		},
		_ => None,
	}
}

#[cfg(target_os = "linux")]
fn debounce(event: &DebouncedEvent, context: &mut DebounceContext) -> Option<VfsEvent> {
	match event.kind {
		EventKind::Create(_) => {
			let path = event_path!(event);

			context.time = event.time;
			context.path.clone_from(&path);

			Some(VfsEvent::Create(path))
		}
		EventKind::Remove(_) => Some(VfsEvent::Delete(event_path!(event))),
		EventKind::Modify(ModifyKind::Name(mode)) => match mode {
			RenameMode::From => Some(VfsEvent::Delete(event_path!(event))),
			RenameMode::To => Some(VfsEvent::Create(event_path!(event))),
			_ => None,
		},
		EventKind::Access(kind) => {
			if kind == AccessKind::Close(AccessMode::Write) {
				let duration = event.time.duration_since(context.time);
				let path = event_path!(event);

				if duration < DEBOUNCE_TIME && path == context.path {
					return None;
				}

				Some(VfsEvent::Write(path))
			} else {
				None
			}
		}
		_ => None,
	}
}

#[cfg(target_os = "windows")]
fn debounce(event: &DebouncedEvent) -> Option<VfsEvent> {
	match event.kind {
		EventKind::Create(_) => Some(VfsEvent::Create(event_path!(event))),
		EventKind::Remove(_) => Some(VfsEvent::Delete(event_path!(event))),
		EventKind::Modify(_) => Some(VfsEvent::Write(event_path!(event))),
		_ => None,
	}
}
