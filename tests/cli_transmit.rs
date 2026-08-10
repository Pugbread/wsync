//! Integration coverage for `wsync transmit` (transmit.json): the real
//! binary against a real daemon with a fake plugin serving the final
//! transmit contract — `transmit_prepare` in exactly one of `source`/`paths`
//! mode answering `{items, failures}`, each item a standard capture session
//! pumped through `capture_read` and released with `capture_close`.
//!
//! Covered: the prepare argument shapes, the client-side `--from` walk (the
//! `find` op, after the source batch), multi-item directory output with
//! defensive name handling, single-item file output, PNG decode-back
//! verification, per-item failure tolerance (partial success exits zero,
//! total failure exits non-zero), and pre-network validation.

mod common;

use serde_json::{json, Value};
use std::{collections::HashMap, fs, sync::Arc};

use common::{
	chunk_answer, cli_json, cli_stderr, cli_stdout, journal_args, journal_ops, spawn_cli_plugin, start_daemon,
	CliAnswer, CliJournal, CliSandbox, TestDaemon,
};

fn sha256_hex(bytes: &[u8]) -> String {
	use sha2::{Digest, Sha256};

	format!("{:x}", Sha256::digest(bytes))
}

/// Deterministic RGBA pixels for a given seed and size
fn pixels(seed: usize, width: u32, height: u32) -> Vec<u8> {
	(0..(width * height * 4) as usize)
		.map(|index| ((index * 13 + seed * 71 + 5) % 249) as u8)
		.collect()
}

/// One prepared transmit item in the plugin's answer shape
fn item(capture_id: &str, name: &str, path: Option<&str>, width: u32, height: u32, bytes: &[u8]) -> Value {
	let mut value = json!({
		"captureId": capture_id,
		"width": width,
		"height": height,
		"bytes": bytes.len(),
		"sha256": sha256_hex(bytes),
		"name": name,
	});

	if let Some(path) = path {
		value["path"] = json!(path);
	}

	value
}

/// A fake plugin serving the final transmit contract: `source` prepares
/// answer `source_items`, `paths` prepares answer per-path entries from
/// `path_items`, unknown paths land in `failures`, and every item's pixels
/// are served through the shared `capture_read` chunk pump
struct TransmitScript {
	source_items: Vec<Value>,
	/// path → (item, pixels)
	path_items: HashMap<String, (Value, Vec<u8>)>,
	/// captureId → pixels (for source items)
	source_pixels: HashMap<String, Vec<u8>>,
	/// `find` answers: class → matches
	find_matches: HashMap<String, Vec<Value>>,
}

fn transmit_plugin(daemon: &TestDaemon, name: &'static str, script: TransmitScript) -> CliJournal {
	let stores: HashMap<String, Vec<u8>> = script
		.source_pixels
		.iter()
		.map(|(id, bytes)| (id.clone(), bytes.clone()))
		.chain(
			script
				.path_items
				.values()
				.map(|(item, bytes)| (item["captureId"].as_str().unwrap_or_default().to_owned(), bytes.clone())),
		)
		.collect();

	let script = Arc::new(script);
	let stores = Arc::new(stores);

	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(move |op, args| match op {
			"transmit_prepare" => {
				// The final contract: exactly one of source/paths
				let has_source = args.get("source").is_some();
				let has_paths = args.get("paths").is_some();

				if has_source == has_paths {
					return CliAnswer::Failure("BAD_ARGS", "transmit_prepare takes exactly one of source/paths");
				}

				if has_source {
					return CliAnswer::Value(json!({
						"items": script.source_items,
						"failures": [],
					}));
				}

				let mut items: Vec<Value> = Vec::new();
				let mut failures: Vec<Value> = Vec::new();

				for path in args["paths"].as_array().cloned().unwrap_or_default() {
					let path = path.as_str().unwrap_or_default();

					match script.path_items.get(path) {
						Some((item, _)) => items.push(item.clone()),
						None => failures.push(json!({
							"name": path.rsplit('/').next().unwrap_or(path),
							"path": path,
							"error": { "code": "NOT_FOUND", "message": "no image-bearing instance here" },
						})),
					}
				}

				CliAnswer::Value(json!({ "items": items, "failures": failures }))
			}
			"capture_read" => {
				let capture_id = args["captureId"].as_str().unwrap_or_default();

				match stores.get(capture_id) {
					Some(bytes) => CliAnswer::Value(chunk_answer(bytes, args)),
					None => CliAnswer::Failure("NOT_FOUND", "no such capture"),
				}
			}
			"capture_close" => CliAnswer::Value(json!({})),
			"find" => {
				let class = args["class"].as_str().unwrap_or_default();
				let matches = script.find_matches.get(class).cloned().unwrap_or_default();

				CliAnswer::Value(json!({ "matches": matches, "count": matches.len() }))
			}
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

/// Decodes a written PNG back to RGBA bytes and checks the exact pixels
fn assert_png_pixels(path: &std::path::Path, width: u32, height: u32, expected: &[u8]) {
	let file = fs::read(path).unwrap_or_else(|_| panic!("{} was not written", path.display()));
	let decoder = png::Decoder::new(&file[..]);
	let mut reader = decoder.read_info().expect("the artifact is not a PNG");
	let mut decoded = vec![0u8; reader.output_buffer_size()];
	let info = reader.next_frame(&mut decoded).expect("the PNG frame does not decode");

	assert_eq!((info.width, info.height), (width, height));

	decoded.truncate(info.buffer_size());
	assert_eq!(
		decoded,
		expected,
		"{} does not decode back to the served pixels",
		path.display()
	);
}

#[test]
fn transmit_paths_prepares_once_and_writes_verified_pngs_with_safe_names() {
	let daemon = start_daemon(None);

	let pixels_a = pixels(1, 8, 8);
	// Two 64 KiB chunks, so the pump really pages through transmit too
	let pixels_b = pixels(2, 128, 160);

	let mut path_items = HashMap::new();

	path_items.insert(
		"StarterGui/A".to_owned(),
		(
			item("t-1", "IconA", Some("StarterGui/A"), 8, 8, &pixels_a),
			pixels_a.clone(),
		),
	);
	// A hostile name: the plugin contract pre-sanitizes, but the CLI must
	// never let a separator escape the output directory anyway
	path_items.insert(
		"StarterGui/B".to_owned(),
		(
			item("t-2", "Icon B/2", Some("StarterGui/B"), 128, 160, &pixels_b),
			pixels_b.clone(),
		),
	);

	let journal = transmit_plugin(
		&daemon,
		"transmit-plugin",
		TransmitScript {
			source_items: Vec::new(),
			path_items,
			source_pixels: HashMap::new(),
			find_matches: HashMap::new(),
		},
	);

	let sandbox = CliSandbox::new();
	let renders = sandbox.work.path().join("renders");

	let output = sandbox.run(&[
		"transmit",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"StarterGui/A",
		"--path",
		"StarterGui/B",
		"--output",
		&renders.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "transmit failed: {}", cli_stderr(&output));

	// One paths-mode prepare carrying both paths and the default timeout —
	// and never a source key
	let prepare = journal_args(&journal, "transmit_prepare");

	assert_eq!(prepare["paths"], json!(["StarterGui/A", "StarterGui/B"]));
	assert!(prepare.get("source").is_none());
	assert_eq!(prepare["timeoutMs"], 60_000);
	assert_eq!(
		journal_ops(&journal)
			.iter()
			.filter(|op| *op == "transmit_prepare")
			.count(),
		1
	);

	// Both sessions were pumped and closed
	let ops = journal_ops(&journal);

	assert!(ops.iter().filter(|op| *op == "capture_read").count() >= 3);
	assert_eq!(ops.iter().filter(|op| *op == "capture_close").count(), 2);

	// The artifacts: real PNGs, hostile name defused
	assert_png_pixels(&renders.join("IconA.png"), 8, 8, &pixels_a);
	assert_png_pixels(&renders.join("Icon B-2.png"), 128, 160, &pixels_b);

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], true);
	assert_eq!(raw["written"], 2);
	assert_eq!(raw["failed"], 0);

	let items = raw["items"].as_array().unwrap();

	assert_eq!(items.len(), 2);

	for entry in items {
		assert_eq!(entry["ok"], true);
		assert!(entry["sha256"].as_str().is_some());
		assert!(entry["file"].as_str().is_some());
	}

	// Pixels never reach stdout: the whole raw record stays small
	assert!(
		cli_stdout(&output).len() < 4096,
		"stdout must carry metadata only, never pixel data"
	);
}

#[test]
fn transmit_source_batch_runs_before_the_from_walk_and_batches_stay_sequential() {
	let daemon = start_daemon(None);

	let render_pixels = pixels(3, 8, 8);
	let shot_pixels = pixels(4, 8, 8);

	let mut path_items = HashMap::new();

	path_items.insert(
		"Workspace/Exports/Shot1".to_owned(),
		(
			item("t-shot", "Shot1", Some("Workspace/Exports/Shot1"), 8, 8, &shot_pixels),
			shot_pixels.clone(),
		),
	);

	let mut find_matches = HashMap::new();

	find_matches.insert(
		"ImageLabel".to_owned(),
		vec![json!({ "class": "ImageLabel", "path": "Workspace/Exports/Shot1" })],
	);

	let mut source_pixels = HashMap::new();

	source_pixels.insert("t-render".to_owned(), render_pixels.clone());

	let journal = transmit_plugin(
		&daemon,
		"transmit-source-plugin",
		TransmitScript {
			source_items: vec![item("t-render", "Render", None, 8, 8, &render_pixels)],
			path_items,
			source_pixels,
			find_matches,
		},
	);

	let sandbox = CliSandbox::new();
	let renders = sandbox.work.path().join("out");

	let output = sandbox.run(&[
		"transmit",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--source",
		"return MakeImage()",
		"--from",
		"Workspace/Exports",
		"--output",
		&renders.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "transmit failed: {}", cli_stderr(&output));

	// The op order proves the contract: the source batch is prepared,
	// pumped, and closed BEFORE the find walk, and the paths batch follows
	// sequentially
	let ops = journal_ops(&journal);
	let position = |op: &str, occurrence: usize| -> usize {
		ops.iter()
			.enumerate()
			.filter(|(_, name)| *name == op)
			.map(|(index, _)| index)
			.nth(occurrence)
			.unwrap_or_else(|| panic!("op {op} #{occurrence} missing: {ops:?}"))
	};

	let first_prepare = position("transmit_prepare", 0);
	let first_close = position("capture_close", 0);
	let first_find = position("find", 0);
	let second_prepare = position("transmit_prepare", 1);
	let second_close = position("capture_close", 1);

	assert!(first_prepare < first_close, "the source batch pumps first: {ops:?}");
	assert!(first_close < first_find, "the walk waits for the source batch: {ops:?}");
	assert!(first_find < second_prepare, "the paths batch follows the walk: {ops:?}");
	assert!(second_prepare < second_close, "batches stay sequential: {ops:?}");
	assert_eq!(
		ops.iter().filter(|op| *op == "find").count(),
		4,
		"one find per image class"
	);

	// Each prepare ran in exactly one mode
	let prepares: Vec<Value> = journal
		.lock()
		.unwrap()
		.iter()
		.filter(|(op, _)| op == "transmit_prepare")
		.map(|(_, args)| args.clone())
		.collect();

	assert_eq!(prepares.len(), 2);
	assert!(prepares[0].get("source").is_some() && prepares[0].get("paths").is_none());
	assert!(prepares[1].get("paths").is_some() && prepares[1].get("source").is_none());
	assert_eq!(prepares[1]["paths"], json!(["Workspace/Exports/Shot1"]));

	// The find walk queried the requested subtree
	let find = journal_args(&journal, "find");

	assert_eq!(find["under"], "Workspace/Exports");

	// Both batches' artifacts landed in the directory
	assert_png_pixels(&renders.join("Render.png"), 8, 8, &render_pixels);
	assert_png_pixels(&renders.join("Shot1.png"), 8, 8, &shot_pixels);
	assert_eq!(cli_json(&output)["written"], 2);
}

#[test]
fn transmit_single_item_writes_the_named_file() {
	let daemon = start_daemon(None);
	let only = pixels(5, 8, 8);

	let mut path_items = HashMap::new();

	path_items.insert(
		"StarterGui/Example".to_owned(),
		(
			item("t-one", "Example", Some("StarterGui/Example"), 8, 8, &only),
			only.clone(),
		),
	);

	let _journal = transmit_plugin(
		&daemon,
		"transmit-single-plugin",
		TransmitScript {
			source_items: Vec::new(),
			path_items,
			source_pixels: HashMap::new(),
			find_matches: HashMap::new(),
		},
	);

	let sandbox = CliSandbox::new();
	let file = sandbox.work.path().join("exports").join("example.png");

	let output = sandbox.run(&[
		"transmit",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"StarterGui/Example",
		"--output",
		&file.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "transmit failed: {}", cli_stderr(&output));

	// A single item with a file --output lands exactly there, not under an
	// invented name
	assert_png_pixels(&file, 8, 8, &only);

	let raw = cli_json(&output);

	assert_eq!(raw["written"], 1);
	assert_eq!(raw["items"][0]["file"], file.to_string_lossy().into_owned());
}

#[test]
fn transmit_tolerates_partial_failures_and_fails_only_when_nothing_writes() {
	let daemon = start_daemon(None);

	let good = pixels(6, 8, 8);
	let bad = pixels(7, 8, 8);

	let mut corrupted = item("t-bad", "Broken", Some("StarterGui/Broken"), 8, 8, &bad);

	// The advertised digest names different bytes than the chunks serve
	corrupted["sha256"] = json!(sha256_hex(b"other bytes"));

	let mut path_items = HashMap::new();

	path_items.insert(
		"StarterGui/Good".to_owned(),
		(
			item("t-good", "Good", Some("StarterGui/Good"), 8, 8, &good),
			good.clone(),
		),
	);
	path_items.insert("StarterGui/Broken".to_owned(), (corrupted, bad));

	let journal = transmit_plugin(
		&daemon,
		"transmit-partial-plugin",
		TransmitScript {
			source_items: Vec::new(),
			path_items,
			source_pixels: HashMap::new(),
			find_matches: HashMap::new(),
		},
	);

	let sandbox = CliSandbox::new();
	let renders = sandbox.work.path().join("partial");

	let output = sandbox.run(&[
		"transmit",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"StarterGui/Good",
		"--path",
		"StarterGui/Broken",
		"--output",
		&renders.to_string_lossy(),
		"--raw",
	]);

	// Partial success is a success: the failure is reported, not fatal
	assert!(
		output.status.success(),
		"partial success must exit zero: {}",
		cli_stderr(&output)
	);

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], true);
	assert_eq!(raw["written"], 1);
	assert_eq!(raw["failed"], 1);

	let failed = raw["items"]
		.as_array()
		.unwrap()
		.iter()
		.find(|entry| entry["ok"] == false)
		.expect("the corrupted item must be reported");

	assert!(
		failed["error"]
			.as_str()
			.is_some_and(|error| error.contains("SHA-256") || error.contains("corrupted")),
		"the digest mismatch must be named: {failed}"
	);

	assert_png_pixels(&renders.join("Good.png"), 8, 8, &good);
	assert!(
		!renders.join("Broken.png").exists(),
		"a corrupted item must leave no artifact"
	);

	// Both sessions were closed, pumped or not
	assert_eq!(
		journal_ops(&journal).iter().filter(|op| *op == "capture_close").count(),
		2
	);

	// A batch where nothing succeeds exits non-zero: both prepare-side
	// failures and an empty result count
	let output = sandbox.run(&[
		"transmit",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"StarterGui/Missing",
		"--output",
		&sandbox.work.path().join("nothing").to_string_lossy(),
		"--raw",
	]);

	assert!(!output.status.success(), "a fully failed transmit must exit non-zero");

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], false);
	assert_eq!(raw["written"], 0);
	assert_eq!(raw["failed"], 1);
	assert!(
		raw["items"][0]["error"]
			.as_str()
			.is_some_and(|error| error.contains("no image-bearing instance")),
		"the prepare-side failure must carry the plugin's message: {raw}"
	);
}

#[test]
fn transmit_validates_before_any_network_work() {
	let daemon = start_daemon(None);
	let journal = transmit_plugin(
		&daemon,
		"transmit-validation-plugin",
		TransmitScript {
			source_items: Vec::new(),
			path_items: HashMap::new(),
			source_pixels: HashMap::new(),
			find_matches: HashMap::new(),
		},
	);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let out = sandbox.work.path().join("never").to_string_lossy().into_owned();

	// Nothing to transmit
	let output = sandbox.run(&["transmit", "--project", &project, "--port", &port, "--output", &out]);

	assert!(!output.status.success());
	assert!(
		cli_stderr(&output).contains("--source") && cli_stderr(&output).contains("--path"),
		"the refusal must name the inputs: {}",
		cli_stderr(&output)
	);

	// --source and --source-file conflict at parse time
	let output = sandbox.run(&[
		"transmit",
		"--project",
		&project,
		"--port",
		&port,
		"--source",
		"return 1",
		"--source-file",
		"render.luau",
		"--output",
		&out,
	]);

	assert!(!output.status.success(), "--source with --source-file must not parse");

	// --output is required
	let output = sandbox.run(&["transmit", "--project", &project, "--port", &port, "--path", "X"]);

	assert!(!output.status.success(), "--output is required");

	// Not one refusal cost a network request
	assert!(
		journal_ops(&journal).is_empty(),
		"validation must precede the network: {:?}",
		journal_ops(&journal)
	);
}
