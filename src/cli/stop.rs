use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use reqwest::blocking::Client;
use std::collections::HashMap;

use crate::{
	logger::Table,
	sessions::{self, Session},
	util, wsync_info, wsync_warn,
};

/// Stop WSync session by address, ID or all running sessions
#[derive(Parser)]
pub struct Stop {
	/// Sessions to stop (registry ID, `host:port` address or port)
	#[arg()]
	session: Vec<String>,

	/// Server host name
	#[arg(short = 'H', long)]
	host: Option<String>,

	/// Server port
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Stop all running session
	#[arg(short, long)]
	all: bool,

	/// List all running session
	#[arg(short, long)]
	list: bool,
}

/// What actually happened to one targeted session
enum Outcome {
	/// The session accepted `/stop` or its process was killed
	Stopped,
	/// The process was already dead; only the stale registry entry was removed
	Pruned,
	/// A managed daemon declined the unauthenticated stop; entry kept
	Refused,
}

impl Stop {
	pub fn main(self) -> Result<()> {
		if self.list {
			let sessions = sessions::get_all()?;

			if sessions.is_empty() {
				wsync_warn!("There are no running sessions");
				return Ok(());
			}

			let mut table = Table::new();
			table.set_header(vec!["ID", "Host", "Port", "PID"]);

			for (id, session) in sessions {
				table.add_row(vec![
					id,
					session.host.unwrap_or("None".into()),
					session.port.map(|p| p.to_string()).unwrap_or("None".into()),
					session.pid.to_string(),
				]);
			}

			wsync_info!("All running sessions:\n\n{}", table);

			return Ok(());
		}

		let all_sessions = sessions::get_all()?;

		let (targets, unmatched) = if self.all {
			let mut targets: Vec<(String, Session)> = all_sessions.into_iter().collect();
			targets.sort_by(|(a, _), (b, _)| a.cmp(b));

			(targets, Vec::new())
		} else if self.session.is_empty() {
			let target = if self.host.is_none() && self.port.is_none() {
				sessions::get_last()?
			} else {
				find_by_address(&all_sessions, self.host.as_ref(), self.port)
			};

			(target.into_iter().collect(), Vec::new())
		} else {
			resolve_targets(&all_sessions, &self.session)
		};

		for arg in &unmatched {
			wsync_warn!("There is no running session matching {}", arg.bold());
		}

		if targets.is_empty() {
			if unmatched.is_empty() {
				if self.all {
					wsync_warn!("There are no running sessions");
				} else {
					wsync_warn!("There is no matching session to stop");
				}
			}

			bail!("No sessions were stopped");
		}

		let mut removed_ids = Vec::new();
		let (mut stopped, mut pruned, mut refused) = (0usize, 0usize, 0usize);

		for (id, session) in &targets {
			match Self::stop_session(id, session) {
				Outcome::Stopped => {
					stopped += 1;
					removed_ids.push(id.clone());
				}
				Outcome::Pruned => {
					pruned += 1;
					removed_ids.push(id.clone());
				}
				Outcome::Refused => refused += 1,
			}
		}

		// Only entries that were actually stopped or verified dead leave the
		// registry - sessions that refused (or were never targeted) remain
		if !removed_ids.is_empty() {
			sessions::remove_ids(&removed_ids)?;
		}

		Self::finish(stopped, pruned, refused)
	}

	/// Success only when at least one session was stopped or reconciled;
	/// a pure no-op must exit nonzero so scripts can tell the difference
	fn finish(stopped: usize, pruned: usize, refused: usize) -> Result<()> {
		if stopped > 0 || pruned > 0 {
			return Ok(());
		}

		if refused > 0 {
			let daemons = if refused == 1 { "daemon" } else { "daemons" };

			bail!(
				"No sessions were stopped: {} managed {daemons} refused the stop request. Use {} instead",
				refused.to_string().bold(),
				"wsync daemon stop --project <path>".bold()
			);
		}

		bail!("No sessions were stopped");
	}

	fn stop_session(id: &str, session: &Session) -> Outcome {
		if let Some(address) = session.get_address() {
			let url = format!("{address}/stop");

			match Client::new().post(url).send() {
				Ok(response) if response.status().is_success() => {
					wsync_info!("Stopped WSync session with address: {}", address.bold());
					Outcome::Stopped
				}
				// A managed daemon refuses unauthenticated stops; never fall back
				// to killing a process that explicitly declined
				Ok(_) => {
					wsync_warn!(
						"Session at {} is a managed daemon and refused the stop request. Use {} instead",
						address.bold(),
						"wsync daemon stop --project <path>".bold()
					);
					Outcome::Refused
				}
				Err(_) => Self::stop_process(id, session),
			}
		} else {
			Self::stop_process(id, session)
		}
	}

	fn stop_process(id: &str, session: &Session) -> Outcome {
		if util::process_exists(session.pid) {
			util::kill_process(session.pid);
			wsync_info!("Stopped WSync process with PID: {}", session.pid.to_string().bold());

			Outcome::Stopped
		} else {
			wsync_info!(
				"Session {} (PID {}) is not running, removing its stale registry entry",
				id.bold(),
				session.pid.to_string().bold()
			);

			Outcome::Pruned
		}
	}
}

/// Resolves each argument to registry entries: exact session id first, then
/// `host:port` address, then bare port. Arguments that match nothing are
/// returned separately so they can be reported
fn resolve_targets(sessions: &HashMap<String, Session>, args: &[String]) -> (Vec<(String, Session)>, Vec<String>) {
	let mut targets: Vec<(String, Session)> = Vec::new();
	let mut unmatched = Vec::new();

	for arg in args {
		let ids = resolve_arg(sessions, arg);

		if ids.is_empty() {
			unmatched.push(arg.clone());
			continue;
		}

		for id in ids {
			if !targets.iter().any(|(existing, _)| existing == &id) {
				let session = sessions[&id].clone();
				targets.push((id, session));
			}
		}
	}

	(targets, unmatched)
}

fn resolve_arg(sessions: &HashMap<String, Session>, arg: &str) -> Vec<String> {
	if sessions.contains_key(arg) {
		return vec![arg.to_owned()];
	}

	if let Some((host, port)) = arg.rsplit_once(':') {
		if let Ok(port) = port.parse::<u16>() {
			let mut ids: Vec<String> = sessions
				.iter()
				.filter(|(_, session)| session.host.as_deref() == Some(host) && session.port == Some(port))
				.map(|(id, _)| id.clone())
				.collect();

			if !ids.is_empty() {
				ids.sort();
				return ids;
			}
		}
	}

	if let Ok(port) = arg.parse::<u16>() {
		let mut ids: Vec<String> = sessions
			.iter()
			.filter(|(_, session)| session.port == Some(port))
			.map(|(id, _)| id.clone())
			.collect();

		ids.sort();
		return ids;
	}

	Vec::new()
}

fn find_by_address(
	sessions: &HashMap<String, Session>,
	host: Option<&String>,
	port: Option<u16>,
) -> Option<(String, Session)> {
	let mut matched: Vec<(String, Session)> = sessions
		.iter()
		.filter(|(_, session)| {
			let host_matches = host.is_none() || session.host.as_ref() == host;
			let port_matches = port.is_none() || session.port == port;

			host_matches && port_matches
		})
		.map(|(id, session)| (id.clone(), session.clone()))
		.collect();

	matched.sort_by(|(a, _), (b, _)| a.cmp(b));
	matched.into_iter().next()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn session(host: Option<&str>, port: Option<u16>) -> Session {
		Session {
			pid: 12345,
			host: host.map(str::to_owned),
			port,
		}
	}

	fn registry() -> HashMap<String, Session> {
		HashMap::from([
			("0".into(), session(Some("localhost"), Some(7978))),
			("2".into(), session(Some("localhost"), Some(7986))),
			("build".into(), session(None, None)),
		])
	}

	#[test]
	fn resolves_registry_id() {
		let (targets, unmatched) = resolve_targets(&registry(), &["2".into()]);

		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].0, "2");
		assert!(unmatched.is_empty());
	}

	#[test]
	fn resolves_host_port_address() {
		let (targets, unmatched) = resolve_targets(&registry(), &["localhost:7986".into()]);

		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].0, "2");
		assert!(unmatched.is_empty());
	}

	#[test]
	fn resolves_bare_port() {
		let (targets, unmatched) = resolve_targets(&registry(), &["7986".into()]);

		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].0, "2");
		assert!(unmatched.is_empty());
	}

	#[test]
	fn registry_id_takes_precedence_over_port() {
		let mut sessions = registry();
		sessions.insert("7978".into(), session(Some("localhost"), Some(9000)));

		let (targets, _) = resolve_targets(&sessions, &["7978".into()]);

		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].0, "7978");
		assert_eq!(targets[0].1.port, Some(9000));
	}

	#[test]
	fn duplicate_matches_are_deduplicated() {
		let (targets, unmatched) = resolve_targets(&registry(), &["2".into(), "localhost:7986".into(), "7986".into()]);

		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].0, "2");
		assert!(unmatched.is_empty());
	}

	#[test]
	fn unmatched_arguments_are_reported() {
		let (targets, unmatched) = resolve_targets(&registry(), &["9999".into(), "otherhost:7986".into()]);

		assert!(targets.is_empty());
		assert_eq!(unmatched, vec!["9999".to_owned(), "otherhost:7986".to_owned()]);
	}

	#[test]
	fn mixed_arguments_resolve_and_report_independently() {
		let (targets, unmatched) = resolve_targets(&registry(), &["7978".into(), "9999".into()]);

		assert_eq!(targets.len(), 1);
		assert_eq!(targets[0].0, "0");
		assert_eq!(unmatched, vec!["9999".to_owned()]);
	}

	#[test]
	fn no_match_exits_nonzero() {
		assert!(Stop::finish(0, 0, 0).is_err());
	}

	#[test]
	fn refused_only_exits_nonzero_with_daemon_pointer() {
		let error = Stop::finish(0, 0, 1).unwrap_err().to_string();

		assert!(error.contains("wsync daemon stop"));
	}

	#[test]
	fn stopping_or_pruning_exits_zero() {
		assert!(Stop::finish(1, 0, 0).is_ok());
		assert!(Stop::finish(0, 1, 0).is_ok());
		assert!(Stop::finish(1, 0, 1).is_ok());
	}
}
