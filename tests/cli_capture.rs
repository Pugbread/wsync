//! Integration coverage for `wsync capture` (capture.json): the real binary
//! against a real daemon with a fake plugin serving the capture op family, so
//! the chunk pump, the local PNG encode + decode-back verification, the
//! `--raw` shape, and the pre-network validation are exercised end to end.
//!
//! The fake plugin serves a small synthetic RGBA image big enough to need
//! more than one 64 KiB chunk, so the offset/EOF accounting is really paged.

mod common;

use serde_json::json;
use std::{fs, sync::Arc};

use common::{
	chunk_answer, cli_json, cli_stderr, journal_args, journal_ops, spawn_cli_plugin, start_daemon, CliAnswer,
	CliJournal, CliSandbox, TestDaemon,
};

/// 128x160 RGBA = 81 920 bytes — two chunks at the 64 KiB chunk size
const WIDTH: u32 = 128;
const HEIGHT: u32 = 160;

/// Deterministic non-trivial pixels, so an off-by-one anywhere in the pump or
/// the encode shows up as a byte mismatch instead of passing on zeroes
fn synthetic_rgba() -> Vec<u8> {
	(0..(WIDTH * HEIGHT * 4) as usize)
		.map(|index| ((index * 31 + 7) % 251) as u8)
		.collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
	use sha2::{Digest, Sha256};

	format!("{:x}", Sha256::digest(bytes))
}

/// A fake plugin serving the full capture op family for one synthetic image.
/// `advertised_sha` lets a test lie about the digest
fn capture_plugin(daemon: &TestDaemon, name: &'static str, pixels: Vec<u8>, advertised_sha: String) -> CliJournal {
	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(move |op, args| match op {
			"capture_prepare" => CliAnswer::Value(json!({
				"captureId": "cap-1",
				"width": WIDTH,
				"height": HEIGHT,
				"bytes": pixels.len(),
				"sha256": advertised_sha,
			})),
			"capture_read" => CliAnswer::Value(chunk_answer(&pixels, args)),
			"capture_close" => CliAnswer::Value(json!({})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

#[test]
fn capture_ui_pumps_chunks_and_writes_a_verified_png() {
	let daemon = start_daemon(None);
	let pixels = synthetic_rgba();
	let sha = sha256_hex(&pixels);
	let journal = capture_plugin(&daemon, "capture-plugin", pixels.clone(), sha);

	let sandbox = CliSandbox::new();
	// A nested output directory proves the CLI creates parents
	let output_path = sandbox.work.path().join("captures").join("shot.png");

	let output = sandbox.run(&[
		"capture",
		"ui",
		"StarterGui/HUD",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--size",
		"128x160",
		"-o",
		&output_path.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "capture failed: {}", cli_stderr(&output));

	// The op sequence: one prepare, a really-chunked pump, one close
	let ops = journal_ops(&journal);

	assert_eq!(ops.first().map(String::as_str), Some("capture_prepare"));
	assert!(
		ops.iter().filter(|op| *op == "capture_read").count() >= 2,
		"an 80 KiB image must take more than one 64 KiB chunk: {ops:?}"
	);
	assert_eq!(ops.last().map(String::as_str), Some("capture_close"));

	// The prepare op carried the kind, target, and validated options
	let prepare = journal_args(&journal, "capture_prepare");

	assert_eq!(prepare["kind"], "ui");
	assert_eq!(prepare["target"], "StarterGui/HUD");
	assert_eq!(prepare["options"]["size"], json!({ "width": 128, "height": 160 }));

	// `--raw` reports the artifact, never pixels
	let raw = cli_json(&output);

	assert_eq!(raw["ok"], true);
	assert_eq!(raw["path"], output_path.to_string_lossy().into_owned());
	assert_eq!(raw["mime"], "image/png");
	assert_eq!(raw["width"], u64::from(WIDTH));
	assert_eq!(raw["height"], u64::from(HEIGHT));
	assert_eq!(raw["capture"]["captureId"], "cap-1");

	// The artifact on disk decodes back to exactly the served pixels
	let file_bytes = fs::read(&output_path).expect("the PNG was not written");

	assert_eq!(raw["bytes"], file_bytes.len() as u64);
	assert_eq!(raw["sha256"], sha256_hex(&file_bytes));

	let decoder = png::Decoder::new(&file_bytes[..]);
	let mut reader = decoder.read_info().expect("the artifact is not a PNG");
	let mut decoded = vec![0u8; reader.output_buffer_size()];
	let info = reader.next_frame(&mut decoded).expect("the PNG frame does not decode");

	assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
	assert_eq!(info.color_type, png::ColorType::Rgba);
	assert_eq!(info.bit_depth, png::BitDepth::Eight);

	decoded.truncate(info.buffer_size());
	assert_eq!(decoded, pixels, "the PNG does not decode back to the served pixels");

	// No temp file left behind
	let leftovers: Vec<_> = fs::read_dir(output_path.parent().unwrap())
		.unwrap()
		.filter_map(Result::ok)
		.filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
		.collect();

	assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
}

#[test]
fn a_sha_mismatch_aborts_with_no_artifact_and_still_closes() {
	let daemon = start_daemon(None);
	let pixels = synthetic_rgba();
	// The advertised digest is for different bytes than the chunks serve
	let journal = capture_plugin(&daemon, "corrupt-capture-plugin", pixels, sha256_hex(b"other bytes"));

	let sandbox = CliSandbox::new();
	let output_path = sandbox.work.path().join("corrupt.png");

	let output = sandbox.run(&[
		"capture",
		"viewport",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"-o",
		&output_path.to_string_lossy(),
		"--raw",
	]);

	assert!(!output.status.success(), "a corrupted transfer was accepted");

	let message = cli_stderr(&output);

	assert!(
		message.contains("SHA-256") || message.contains("corrupted"),
		"the digest mismatch must be named: {message}"
	);

	assert!(!output_path.exists(), "a corrupted capture left an artifact on disk");

	// The dead render is still released
	assert!(
		journal_ops(&journal).iter().any(|op| op == "capture_close"),
		"capture_close must run even when the pump fails: {:?}",
		journal_ops(&journal)
	);
}

#[test]
fn out_of_range_prepared_metadata_is_refused_before_the_pump() {
	let daemon = start_daemon(None);

	// A hostile/buggy plugin prepares dimensions past every limit; the CLI
	// must refuse before asking for a single chunk
	let journal = spawn_cli_plugin(
		&daemon,
		"oversize-prepare-plugin",
		Arc::new(|op, _args| match op {
			"capture_prepare" => CliAnswer::Value(json!({
				"captureId": "cap-big",
				"width": 9000,
				"height": 9000,
				"bytes": 9000u64 * 9000 * 4,
				"sha256": "00",
			})),
			"capture_close" => CliAnswer::Value(json!({})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let output_path = sandbox.work.path().join("never.png");

	let output = sandbox.run(&[
		"capture",
		"viewport",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"-o",
		&output_path.to_string_lossy(),
	]);

	assert!(!output.status.success(), "an out-of-range prepare was accepted");

	let ops = journal_ops(&journal);

	assert!(
		!ops.iter().any(|op| op == "capture_read"),
		"no chunk may be requested for an out-of-range prepare: {ops:?}"
	);
	assert!(
		ops.iter().any(|op| op == "capture_close"),
		"the refused capture must still be closed: {ops:?}"
	);
	assert!(!output_path.exists());
}

#[test]
fn invalid_options_are_rejected_before_any_request() {
	let daemon = start_daemon(None);
	// A plugin that would answer anything is connected the whole time: the
	// refusals below must come from the CLI, not from an absent plugin
	let pixels = synthetic_rgba();
	let sha = sha256_hex(&pixels);
	let journal = capture_plugin(&daemon, "validation-plugin", pixels, sha);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let cases: Vec<(Vec<&str>, &str)> = vec![
		// Oversize axis, oversize region, malformed size
		(vec!["capture", "ui", "--size", "5000x100"], "4096"),
		(vec!["capture", "viewport", "--region", "0,0,5000,100"], "4096"),
		(vec!["capture", "ui", "--size", "1920"], "WIDTHxHEIGHT"),
		// Model-only numeric ranges
		(vec!["capture", "model", "Workspace/Boss", "--padding", "5.0"], "1.0"),
		(vec!["capture", "model", "Workspace/Boss", "--fov", "300"], "120"),
		// Camera-CFrame arity and direction degeneracy
		(
			vec!["capture", "model", "Workspace/Boss", "--camera-cframe", "1,2,3"],
			"12",
		),
		(
			vec!["capture", "model", "Workspace/Boss", "--direction", "0,0,0"],
			"zero vector",
		),
	];

	for (args, expected) in cases {
		let mut full = args.clone();

		full.extend(["--project", &project, "--port", &port]);

		let output = sandbox.run(&full);

		assert!(!output.status.success(), "accepted invalid invocation: {args:?}");

		let message = cli_stderr(&output);

		assert!(
			message.contains(expected),
			"refusal for {args:?} must mention `{expected}`: {message}"
		);
	}

	// Kind-inapplicable flags are refused by the argument parser itself
	let output = sandbox.run(&[
		"capture",
		"ui",
		"--view",
		"front",
		"--project",
		&project,
		"--port",
		&port,
	]);

	assert!(!output.status.success(), "`capture ui --view` must not parse");

	// Authored-camera exclusivity is enforced at parse time too
	let output = sandbox.run(&[
		"capture",
		"model",
		"Workspace/Boss",
		"--view",
		"front",
		"--camera-cframe",
		"0,10,20,1,0,0,0,1,0,0,0,1",
		"--project",
		&project,
		"--port",
		&port,
	]);

	assert!(
		!output.status.success(),
		"--view and --camera-cframe together must not parse"
	);

	// Not one of the refusals above cost a network request
	let ops = journal_ops(&journal);

	assert!(ops.is_empty(), "invalid options reached the plugin: {ops:?}");
}

#[test]
fn human_output_names_the_artifact_and_default_options_stay_minimal() {
	let daemon = start_daemon(None);
	let pixels = synthetic_rgba();
	let sha = sha256_hex(&pixels);
	let journal = capture_plugin(&daemon, "human-capture-plugin", pixels, sha);

	let sandbox = CliSandbox::new();
	let output_path = sandbox.work.path().join("model.png");

	let output = sandbox.run(&[
		"capture",
		"model",
		"Workspace/Boss",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"-o",
		&output_path.to_string_lossy(),
	]);

	assert!(output.status.success(), "capture failed: {}", cli_stderr(&output));
	assert!(output_path.exists());

	let message = cli_stderr(&output);

	assert!(
		message.contains("Captured") && message.contains("128x160"),
		"the human line must report kind and dimensions: {message}"
	);

	// A bare model capture sends only the kind and target — no invented
	// options
	let prepare = journal_args(&journal, "capture_prepare");

	assert_eq!(prepare["kind"], "model");
	assert_eq!(prepare["target"], "Workspace/Boss");
	assert!(
		prepare.get("options").is_none(),
		"a flagless capture must not invent options: {prepare}"
	);
}
