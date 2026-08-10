use anyhow::{Context, Result};
use log::{debug, trace, warn};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, process, thread};

use crate::util;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Session {
	pub pid: u32,
	pub host: Option<String>,
	pub port: Option<u16>,
}

impl Session {
	pub fn get_address(&self) -> Option<String> {
		if let Some(host) = &self.host {
			if let Some(port) = self.port {
				return Some(format!("http://{host}:{port}"));
			}
		}

		None
	}
}

#[derive(Serialize, Deserialize, Debug)]
struct Sessions {
	last_session: String,
	active_sessions: HashMap<String, Session>,
}

fn get_sessions() -> Result<Sessions> {
	let path = util::get_wsync_dir()?.join("sessions.toml");

	if path.exists() {
		match toml::from_str(&fs::read_to_string(&path)?) {
			Ok(sessions) => return Ok(sessions),
			// Leave the file untouched: rewriting it here would wipe entries
			// of sessions that are still running
			Err(_) => warn!("Session data file is corrupted!"),
		}

		return Ok(Sessions {
			last_session: String::new(),
			active_sessions: HashMap::new(),
		});
	}

	let sessions = Sessions {
		last_session: String::new(),
		active_sessions: HashMap::new(),
	};

	fs::write(path, toml::to_string(&sessions)?)?;

	Ok(sessions)
}

fn set_sessions(sessions: &Sessions) -> Result<()> {
	let dir = util::get_wsync_dir()?;

	// Write-then-rename keeps the swap atomic: a concurrent reader (another
	// CLI process, or a session cleaning up after itself on shutdown) must
	// never see a half-written file, as it would parse as corrupted
	let temp = dir.join(format!("sessions.toml.{}.tmp", process::id()));

	fs::write(&temp, toml::to_string(sessions)?)?;
	fs::rename(temp, dir.join("sessions.toml"))?;

	Ok(())
}

pub fn add(id: Option<String>, host: Option<String>, port: Option<u16>, pid: u32, run_async: bool) -> Result<()> {
	let mut sessions = get_sessions()?;

	let session = Session { host, port, pid };
	let id = id.unwrap_or(generate_id(&sessions));

	sessions.last_session.clone_from(&id);
	sessions.active_sessions.insert(id, session.clone());

	set_sessions(&sessions)?;

	if !run_async {
		ctrlc::set_handler(move || {
			match remove(&session) {
				Ok(()) => trace!("Session entry removed"),
				Err(err) => warn!("Failed to remove session entry: {err}"),
			}

			process::exit(0);
		})?;
	}

	// Schedule manual cleanup of old sessions
	// as ctrlc handler does not work on Windows,
	// on UNIX cleanup will remove crashed sessions
	thread::spawn(move || match cleanup(sessions) {
		Ok(()) => debug!("Session cleanup completed"),
		Err(err) => warn!("Failed to cleanup sessions: {err}"),
	});

	Ok(())
}

pub fn get(id: Option<String>, host: Option<String>, port: Option<u16>) -> Result<Option<Session>> {
	let sessions = get_sessions()?;

	if id.is_none() && host.is_none() && port.is_none() {
		return Ok(sessions.active_sessions.get(&sessions.last_session).cloned());
	} else if let Some(id) = id {
		return Ok(sessions.active_sessions.get(&id).cloned());
	}

	for (_, session) in sessions.active_sessions {
		let host_matches = host.is_none() || session.host == host;
		let port_matches = port.is_none() || session.port == port;

		if host_matches && port_matches {
			return Ok(Some(session));
		}
	}

	Ok(None)
}

/// Returns the most recently registered session together with its id
pub fn get_last() -> Result<Option<(String, Session)>> {
	let sessions = get_sessions()?;
	let session = sessions.active_sessions.get(&sessions.last_session).cloned();

	Ok(session.map(|session| (sessions.last_session, session)))
}

pub fn get_all() -> Result<HashMap<String, Session>> {
	Ok(get_sessions()?.active_sessions)
}

pub fn remove(session: &Session) -> Result<()> {
	let mut sessions = get_sessions()?;

	let id = sessions
		.active_sessions
		.iter()
		.find_map(|(i, s)| if s == session { Some(i.clone()) } else { None })
		.context("Session not found")?;

	sessions.active_sessions.remove(&id);

	if sessions.last_session == id {
		if let Some((session_id, _)) = sessions.active_sessions.iter().next() {
			sessions.last_session.clone_from(session_id);
		} else {
			sessions.last_session = String::new();
		}
	}

	set_sessions(&sessions)?;

	Ok(())
}

pub fn remove_ids(ids: &[String]) -> Result<()> {
	let mut sessions = get_sessions()?;

	remove_ids_in(&mut sessions, ids);

	set_sessions(&sessions)?;

	Ok(())
}

/// Removes only the given entries, preserving every other registered session.
/// `last_session` is reassigned only when it was one of the removed ids
fn remove_ids_in(sessions: &mut Sessions, ids: &[String]) {
	for id in ids {
		sessions.active_sessions.remove(id);
	}

	if !sessions.active_sessions.contains_key(&sessions.last_session) {
		sessions.last_session = sessions.active_sessions.keys().next().cloned().unwrap_or_default();
	}
}

fn cleanup(mut sessions: Sessions) -> Result<()> {
	let mut did_remove = false;

	for (id, session) in sessions.active_sessions.clone() {
		if !util::process_exists(session.pid) {
			sessions.active_sessions.remove(&id);
			did_remove = true;
		}
	}

	if did_remove {
		set_sessions(&sessions)?;
	}

	Ok(())
}

fn generate_id(sessions: &Sessions) -> String {
	let mut index = 0;

	loop {
		let id = index.to_string();

		if !sessions.active_sessions.contains_key(&id) {
			return id;
		}

		index += 1;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn session(port: u16) -> Session {
		Session {
			pid: 1000 + u32::from(port),
			host: Some("localhost".into()),
			port: Some(port),
		}
	}

	fn registry() -> Sessions {
		Sessions {
			last_session: "2".into(),
			active_sessions: HashMap::from([("0".into(), session(7978)), ("2".into(), session(7986))]),
		}
	}

	#[test]
	fn removing_one_session_preserves_the_others() {
		let mut sessions = registry();

		remove_ids_in(&mut sessions, &["2".into()]);

		assert_eq!(sessions.active_sessions.len(), 1);
		assert_eq!(sessions.active_sessions.get("0"), Some(&session(7978)));
	}

	#[test]
	fn last_session_is_kept_when_untouched() {
		let mut sessions = registry();

		remove_ids_in(&mut sessions, &["0".into()]);

		assert_eq!(sessions.last_session, "2");
	}

	#[test]
	fn last_session_is_reassigned_when_removed() {
		let mut sessions = registry();

		remove_ids_in(&mut sessions, &["2".into()]);

		assert_eq!(sessions.last_session, "0");
	}

	#[test]
	fn removing_every_session_clears_last_session() {
		let mut sessions = registry();

		remove_ids_in(&mut sessions, &["0".into(), "2".into()]);

		assert!(sessions.active_sessions.is_empty());
		assert_eq!(sessions.last_session, "");
	}

	#[test]
	fn unknown_ids_are_ignored() {
		let mut sessions = registry();

		remove_ids_in(&mut sessions, &["7".into()]);

		assert_eq!(sessions.active_sessions.len(), 2);
		assert_eq!(sessions.last_session, "2");
	}
}
