//! Chunked byte transfer shared by the artifact surfaces (`capture`, `copy`,
//! `paste`).
//!
//! The plugin serves large payloads in bounded base64 chunks behind a
//! prepare/read/close (or begin/chunk/commit) op family. This module owns the
//! pump: fixed 64 KiB chunks, the per-chunk 5 s default deadline, offset and
//! EOF accounting, the declared-total cross-check on every chunk, and the
//! final SHA-256 verification — so a torn, reordered, or corrupted transfer
//! is always a hard error and never a silently wrong artifact. Progress is
//! reported on stderr only when stderr is a terminal, keeping piped output
//! byte-clean.

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, IsTerminal, Write};

use crate::cli::client::Client;

/// Chunk size for both directions — the plugin's documented `maxLen` ceiling
pub const CHUNK_BYTES: usize = 65536;

/// Renders a byte count the way a human reads one
pub fn human_size(bytes: u64) -> String {
	const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];

	let mut value = bytes as f64;
	let mut unit = 0;

	while value >= 1024.0 && unit < UNITS.len() - 1 {
		value /= 1024.0;
		unit += 1;
	}

	if unit == 0 {
		format!("{bytes} B")
	} else {
		format!("{value:.1} {}", UNITS[unit])
	}
}

/// Hex SHA-256 of a byte buffer, the digest form every transfer op family
/// advertises
pub fn sha256_hex(bytes: &[u8]) -> String {
	format!("{:x}", Sha256::digest(bytes))
}

/// Terminal-only progress line. Every call rewrites one stderr line; `done`
/// erases it, so a successful transfer leaves no residue in the transcript
struct Progress {
	tty: bool,
	label: &'static str,
}

impl Progress {
	fn new(label: &'static str) -> Self {
		Self {
			tty: io::stderr().is_terminal(),
			label,
		}
	}

	fn report(&self, transferred: u64, total: u64) {
		if !self.tty {
			return;
		}

		let percent = transferred
			.saturating_mul(100)
			.checked_div(total)
			.map_or(100, |percent| percent.min(100));

		eprint!(
			"\r{}: {percent:>3}% ({} / {})",
			self.label,
			human_size(transferred),
			human_size(total)
		);
		io::stderr().flush().ok();
	}

	fn done(&self) {
		if self.tty {
			// Wide enough to cover the longest line `report` can draw
			eprint!("\r{:<60}\r", "");
			io::stderr().flush().ok();
		}
	}
}

/// One prepared server-side payload: the read op that serves it, the id it
/// is addressed by, and the size and digest the prepare op declared
pub struct Prepared<'a> {
	pub op: &'a str,
	pub id_key: &'static str,
	pub id: &'a str,
	pub bytes: u64,
	pub sha256: &'a str,
	/// What the progress line and error messages call the payload
	pub label: &'static str,
}

/// Pulls a prepared payload through repeated `{<id_key>, offset, maxLen}` →
/// `{data, eof, totalBytes}` reads, then verifies length and SHA-256 against
/// what the prepare op declared. Any inconsistency — a drifting `totalBytes`,
/// an overrun, a stalled chunk, a digest mismatch — aborts the command
pub fn pull(client: &Client, prepared: &Prepared, raw: bool) -> Result<Vec<u8>> {
	let Prepared {
		op,
		id_key,
		id,
		bytes: declared_bytes,
		sha256: declared_sha,
		label,
	} = *prepared;

	let progress = Progress::new(label);
	let mut buffer: Vec<u8> = Vec::with_capacity(declared_bytes as usize);

	loop {
		let args = json!({ id_key: id, "offset": buffer.len() as u64, "maxLen": CHUNK_BYTES as u64 });
		let value = client.request(op, args)?.into_value(raw)?;

		let total = value
			.get("totalBytes")
			.and_then(Value::as_u64)
			.with_context(|| format!("The plugin answered `{op}` without `totalBytes`"))?;

		// The total is fixed at prepare time; a chunk that claims otherwise
		// means the far side re-rendered or lost the transfer
		if total != declared_bytes {
			progress.done();
			bail!("The {label} changed size mid-transfer (prepared {declared_bytes} bytes, chunk reports {total})");
		}

		let encoded = value.get("data").and_then(Value::as_str).unwrap_or("");
		let chunk = base64::engine::general_purpose::STANDARD
			.decode(encoded)
			.with_context(|| format!("The plugin answered `{op}` with undecodable base64"))?;

		let eof = value.get("eof").and_then(Value::as_bool) == Some(true);

		if chunk.is_empty() && !eof {
			progress.done();
			bail!("The {label} transfer stalled: an empty chunk arrived before EOF");
		}

		if buffer.len() + chunk.len() > declared_bytes as usize {
			progress.done();
			bail!(
				"The {label} transfer overran its declared size ({} of {declared_bytes} bytes, then {} more)",
				buffer.len(),
				chunk.len()
			);
		}

		buffer.extend_from_slice(&chunk);
		progress.report(buffer.len() as u64, declared_bytes);

		if eof {
			break;
		}
	}

	progress.done();

	if buffer.len() as u64 != declared_bytes {
		bail!(
			"The {label} transfer ended early: {} of {declared_bytes} bytes arrived",
			buffer.len()
		);
	}

	let digest = sha256_hex(&buffer);

	if !digest.eq_ignore_ascii_case(declared_sha) {
		bail!("The {label} arrived corrupted: SHA-256 {digest} does not match the prepared {declared_sha}");
	}

	Ok(buffer)
}

/// Pushes a local payload through repeated `{<id_key>, offset, data}` chunk
/// ops. The receiving side re-verifies the digest at commit; this half only
/// has to deliver every byte in order
pub fn push(
	client: &Client,
	op: &str,
	id_key: &'static str,
	id: &str,
	bytes: &[u8],
	label: &'static str,
	raw: bool,
) -> Result<()> {
	let progress = Progress::new(label);

	for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
		let offset = index * CHUNK_BYTES;

		let args = json!({
			id_key: id,
			"offset": offset as u64,
			"data": base64::engine::general_purpose::STANDARD.encode(chunk),
		});

		client.request(op, args)?.into_value(raw)?;
		progress.report((offset + chunk.len()) as u64, bytes.len() as u64);
	}

	progress.done();

	Ok(())
}
