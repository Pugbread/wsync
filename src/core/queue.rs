use anyhow::{bail, Result};
use colored::Colorize;
use crossbeam_channel::{Receiver, Sender};
use std::{
	collections::HashMap,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, RwLock,
	},
};

use crate::{
	config::Config,
	constants::QUEUE_TIMEOUT,
	server::{self, Message},
	wsync_warn,
};

macro_rules! read {
	($rwlock:expr) => {
		$rwlock.write().unwrap()
	};
}

macro_rules! write {
	($rwlock:expr) => {
		$rwlock.write().unwrap()
	};
}

#[derive(Debug, Clone)]
struct Listener {
	pub id: u32,
	pub name: String,
	pub is_internal: bool,
}

#[derive(Debug)]
struct Channel {
	sender: Sender<Message>,
	receiver: Receiver<Message>,
	flow: Arc<Flow>,
}

impl Channel {
	fn new() -> Self {
		let (sender, receiver) = crossbeam_channel::unbounded();

		Self {
			sender,
			receiver,
			flow: Arc::new(Flow::default()),
		}
	}
}

/// Delivery accounting for one listener, in messages and in the changes they
/// carry. A consumer that comes back for another message is done with the
/// previous one — the WS bridge translates each message and hands its frames
/// to the socket queue before it asks again — so `settled` trails `pushed`
/// by exactly what is still on its way to the socket. `confirmed` is the
/// highest settled mark a `wait-synced` barrier proved the plugin applied
#[derive(Debug, Default)]
struct Flow {
	pushed: AtomicU64,
	pushed_changes: AtomicU64,
	taken: AtomicU64,
	taken_changes: AtomicU64,
	settled: AtomicU64,
	settled_changes: AtomicU64,
	confirmed_changes: AtomicU64,
}

impl Flow {
	fn on_push(&self, message: &Message) {
		self.pushed_changes.fetch_add(change_count(message), Ordering::SeqCst);
		self.pushed.fetch_add(1, Ordering::SeqCst);
	}

	/// The consumer is back: everything it took before is handled
	fn on_wait(&self) {
		self.settled_changes
			.fetch_max(self.taken_changes.load(Ordering::SeqCst), Ordering::SeqCst);
		self.settled
			.fetch_max(self.taken.load(Ordering::SeqCst), Ordering::SeqCst);
	}

	fn on_take(&self, message: &Message) {
		self.taken_changes.fetch_add(change_count(message), Ordering::SeqCst);
		self.taken.fetch_add(1, Ordering::SeqCst);
	}

	fn mark(&self) -> FlowMark {
		FlowMark {
			pushed: self.pushed.load(Ordering::SeqCst),
			pushed_changes: self.pushed_changes.load(Ordering::SeqCst),
			settled: self.settled.load(Ordering::SeqCst),
			settled_changes: self.settled_changes.load(Ordering::SeqCst),
			confirmed_changes: self.confirmed_changes.load(Ordering::SeqCst),
		}
	}
}

fn change_count(message: &Message) -> u64 {
	match message {
		Message::SyncChanges(server::SyncChanges(changes)) => changes.total() as u64,
		_ => 0,
	}
}

/// A point-in-time read of one listener's [`Flow`]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlowMark {
	/// Messages pushed to the listener
	pub pushed: u64,
	/// Changes carried by those messages
	pub pushed_changes: u64,
	/// Messages the consumer has finished handling
	pub settled: u64,
	pub settled_changes: u64,
	/// Changes a completed barrier proved applied on the other side
	pub confirmed_changes: u64,
}

impl FlowMark {
	/// Changes pushed but not yet handed on by the consumer
	pub fn queued_changes(&self) -> u64 {
		self.pushed_changes.saturating_sub(self.settled_changes)
	}

	/// Changes handed on but not yet confirmed by a barrier
	pub fn unconfirmed_changes(&self) -> u64 {
		self.settled_changes.saturating_sub(self.confirmed_changes)
	}
}

#[derive(Debug)]
pub struct Queue {
	queues: RwLock<HashMap<u32, Channel>>,
	listeners: RwLock<Vec<Listener>>,
	unsynced_changes: RwLock<usize>,
}

impl Queue {
	pub fn new() -> Self {
		Self {
			queues: RwLock::new(HashMap::new()),
			listeners: RwLock::new(Vec::new()),
			unsynced_changes: RwLock::new(0),
		}
	}

	pub fn push<M>(&self, message: M, id: Option<u32>) -> Result<()>
	where
		M: Into<Message>,
	{
		if let Some(id) = id {
			if !self.is_subscribed(id) {
				bail!("Not subscribed")
			}

			let queues = read!(self.queues);
			let channel = queues.get(&id).unwrap();
			let message = message.into();

			channel.flow.on_push(&message);
			channel.sender.send(message)?;

			return Ok(());
		}

		let message: Message = message.into();
		let mut did_push = false;

		for listener in read!(self.listeners).iter() {
			let queues = read!(self.queues);
			let channel = queues.get(&listener.id).unwrap();

			channel.flow.on_push(&message);
			channel.sender.send(message.clone())?;

			did_push = true;
		}

		if !did_push {
			let max_unsynced_changes = Config::new().max_unsynced_changes;
			let mut unsynced_changes = write!(self.unsynced_changes);

			*unsynced_changes += 1;

			if max_unsynced_changes > 0 && *unsynced_changes >= max_unsynced_changes {
				wsync_warn!(
					"There are {} unsynced changes. Connect at least one client to this server or increase max_unsynced_changes setting to suppress this warning",
					unsynced_changes.to_string().bold()
				);
			}
		}

		Ok(())
	}

	pub fn get(&self, id: u32) -> Result<Option<Message>> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		let queues = read!(self.queues);
		let channel = queues.get(&id).unwrap();
		let (receiver, flow) = (channel.receiver.clone(), channel.flow.clone());

		drop(queues);

		flow.on_wait();

		let message = receiver.recv().ok();

		if let Some(message) = &message {
			flow.on_take(message);
		}

		Ok(message)
	}

	pub fn get_timeout(&self, id: u32) -> Result<Option<Message>> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		let queues = read!(self.queues);
		let channel = queues.get(&id).unwrap();
		let (receiver, flow) = (channel.receiver.clone(), channel.flow.clone());

		drop(queues);

		flow.on_wait();

		let message = receiver.recv_timeout(QUEUE_TIMEOUT).ok();

		if let Some(message) = &message {
			flow.on_take(message);
		}

		Ok(message)
	}

	/// The delivery accounting of one listener (`None` once unsubscribed)
	pub fn flow(&self, id: u32) -> Option<FlowMark> {
		read!(self.queues).get(&id).map(|channel| channel.flow.mark())
	}

	/// Records that a barrier proved everything up to `mark` applied on the
	/// listener's side. Never moves backwards
	pub fn confirm(&self, id: u32, mark: &FlowMark) {
		if let Some(channel) = read!(self.queues).get(&id) {
			channel
				.flow
				.confirmed_changes
				.fetch_max(mark.settled_changes, Ordering::SeqCst);
		}
	}

	pub fn get_change(&self, id: u32) -> Result<Message> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		let queues = read!(self.queues);
		let receiver = queues.get(&id).unwrap().receiver.clone();

		drop(queues);

		Ok(loop {
			if let Ok(message) = receiver.recv() {
				if message.is_change() {
					break message;
				}
			}
		})
	}

	pub fn subscribe(&self, id: u32, name: &str) -> Result<()> {
		if self.is_subscribed(id) {
			bail!("Already subscribed")
		}

		let channel = Channel::new();

		let listener = Listener {
			id,
			name: name.to_owned(),
			is_internal: false,
		};

		write!(self.listeners).push(listener);
		write!(self.queues).insert(id.to_owned(), channel);

		Ok(())
	}

	pub fn subscribe_internal(&self) -> Result<()> {
		let mut id = 0;

		loop {
			if !self.is_subscribed(id) {
				break;
			}

			id += 1;
		}

		let channel = Channel::new();

		let listener = Listener {
			id,
			name: format!("Internal listener #{id}"),
			is_internal: true,
		};

		write!(self.listeners).push(listener);
		write!(self.queues).insert(id.to_owned(), channel);

		Ok(())
	}

	pub fn unsubscribe(&self, id: u32) -> Result<()> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		write!(self.listeners).retain(|listener| listener.id != id);
		write!(self.queues).remove(&id);

		Ok(())
	}

	pub fn disconnect(&self, message: &str, id: u32) -> Result<()> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		self.push(
			server::Disconnect {
				message: message.to_owned(),
			},
			Some(id),
		)?;

		Ok(())
	}

	pub fn is_subscribed(&self, id: u32) -> bool {
		read!(self.listeners).iter().any(|listener| listener.id == id)
	}

	pub fn get_first_non_internal_listener_name(&self) -> Option<String> {
		read!(self.listeners)
			.iter()
			.find(|listener| !listener.is_internal)
			.map(|listener| listener.name.to_owned())
	}
}

#[cfg(test)]
mod tests {
	use rbx_dom_weak::types::Ref;

	use super::*;
	use crate::core::changes::Changes;

	fn sync(total: usize) -> server::SyncChanges {
		let mut changes = Changes::new();

		for _ in 0..total {
			changes.remove(Ref::new());
		}

		server::SyncChanges(changes)
	}

	#[test]
	fn a_message_settles_only_when_its_consumer_comes_back() {
		let queue = Queue::new();

		queue.subscribe(7, "plugin").unwrap();
		queue.push(sync(3), None).unwrap();
		queue.push(sync(2), Some(7)).unwrap();

		let mark = queue.flow(7).unwrap();
		assert_eq!((mark.pushed, mark.pushed_changes), (2, 5));
		assert_eq!(mark.queued_changes(), 5);

		// Taken, but the consumer has not come back: still on its way
		queue.get_timeout(7).unwrap().unwrap();
		assert_eq!(queue.flow(7).unwrap().settled, 0);

		// Coming back for the next one settles the first
		queue.get_timeout(7).unwrap().unwrap();
		let mark = queue.flow(7).unwrap();
		assert_eq!((mark.settled, mark.settled_changes), (1, 3));
		assert_eq!(mark.queued_changes(), 2);
		assert_eq!(mark.unconfirmed_changes(), 3);

		// A barrier confirmation never moves backwards
		queue.confirm(7, &mark);
		queue.confirm(7, &FlowMark::default());
		assert_eq!(queue.flow(7).unwrap().unconfirmed_changes(), 0);

		queue.unsubscribe(7).unwrap();
		assert!(queue.flow(7).is_none());
	}
}
