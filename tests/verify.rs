//! `wsync verify` (verify.json): a real daemon over a code-scope scratch
//! project, a fake plugin that plays a Studio session (per-path editor text,
//! a script list, a duplicate pair), and the real binary — every report
//! category is produced from genuine disk and daemon state.

mod common;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::HashMap,
	fs,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use common::{
	cli_json, cli_stderr, cli_stdout, scratch_project_scoped, spawn_cli_plugin, start_daemon_in, CliAnswer, CliSandbox,
	TestDaemon,
};

/// One script as the fake Studio holds it: editor text and committed Source
#[derive(Clone)]
struct StudioScript {
	editor: String,
	committed: String,
}

impl StudioScript {
	fn saved(text: &str) -> Self {
		Self {
			editor: text.to_owned(),
			committed: text.to_owned(),
		}
	}
}

fn sha256(text: &str) -> String {
	format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// A fake plugin answering `verify_sources` / `verify_tree` / `sync_barrier`
/// from `studio` (Studio path → script) plus canned duplicate groups
fn spawn_studio(daemon: &TestDaemon, studio: HashMap<String, StudioScript>, duplicates: Value) {
	let studio = Arc::new(studio);

	spawn_cli_plugin(
		daemon,
		"verify-plugin",
		Arc::new(move |op, args| match op {
			"sync_barrier" => CliAnswer::Value(json!({ "applied": true, "settled": true })),
			"verify_sources" => {
				let results: Vec<Value> = args["entries"]
					.as_array()
					.unwrap()
					.iter()
					.map(|entry| {
						let path = entry["path"].as_str().unwrap_or_default();

						match studio.get(path) {
							None => json!({ "ref": entry["ref"], "found": false, "tracked": false }),
							Some(script) => {
								let mut result = json!({
									"ref": entry["ref"],
									"found": true,
									"tracked": true,
									"class": "ModuleScript",
									"path": path,
									"bytes": script.editor.len(),
								});

								if let Some(expected) = entry["sha256"].as_str() {
									let actual = sha256(&script.editor);

									result["match"] = json!(actual == expected);
									result["sha256"] = json!(actual);
								}

								if script.editor != script.committed {
									result["draft"] = json!({ "bytes": script.committed.len() });
								}

								result
							}
						}
					})
					.collect();

				CliAnswer::Value(json!({ "results": results }))
			}
			"verify_tree" => {
				let mut scripts: Vec<Value> = studio
					.keys()
					.map(|path| json!({ "path": path, "class": "ModuleScript" }))
					.collect();

				scripts.sort_by_key(|script| script["path"].as_str().unwrap().to_owned());

				CliAnswer::Value(json!({
					"scripts": scripts,
					"total": scripts.len(),
					"nodes": scripts.len() + 1,
					"truncated": false,
					"missingRoots": [],
					"duplicates": duplicates,
				}))
			}
			_ => CliAnswer::Failure("UNKNOWN_OP", "not served by this fake"),
		}),
	);
}

fn tree() -> Value {
	json!({
		"$className": "DataModel",
		"ReplicatedStorage": { "$path": "src" },
	})
}

fn now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn run_verify(daemon: &TestDaemon, extra: &[&str]) -> std::process::Output {
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let mut args = vec!["verify", "--project", project.as_str(), "--port", port.as_str()];

	args.extend_from_slice(extra);
	sandbox.run(&args)
}

fn paths(report: &Value, key: &str) -> Vec<String> {
	let mut paths: Vec<String> = report[key]
		.as_array()
		.unwrap_or_else(|| panic!("{key} is not a list: {report}"))
		.iter()
		.map(|entry| entry["path"].as_str().unwrap_or_default().to_owned())
		.collect();

	paths.sort();
	paths
}

#[test]
fn verify_reports_every_category_and_fails() {
	let dir = scratch_project_scoped(
		tree(),
		&[
			("src/Match.luau", "return 'match'\n"),
			// CRLF on disk folds to LF exactly like the sync engine
			("src/Crlf.luau", "return 'crlf'\r\n"),
			("src/Differs.luau", "return 'disk'\n"),
			("src/Gone.luau", "return 'only on disk'\n"),
			("src/Draft.luau", "return 'saved'\n"),
			("src/Stuck.luau", "return 'rewritten after the backlog took it'\n"),
		],
		Some("code"),
	);

	// A backlog entry for Stuck.luau, as a mid-session clash would leave it
	let backlog = dir.path().join(".wsync-backups").join("backlog");
	fs::create_dir_all(&backlog).unwrap();
	fs::write(
		backlog.join("index.json"),
		json!([{
			"id": "4f0c8a2e-0000-4000-8000-000000000001",
			"path": "src/Stuck.luau",
			"reason": "conflict",
			"capturedAt": now(),
			"bytes": 10,
		}])
		.to_string(),
	)
	.unwrap();

	let daemon = start_daemon_in(dir);

	let studio = HashMap::from([
		(
			"ReplicatedStorage/Match".to_owned(),
			StudioScript::saved("return 'match'\n"),
		),
		(
			"ReplicatedStorage/Crlf".to_owned(),
			StudioScript::saved("return 'crlf'\n"),
		),
		(
			"ReplicatedStorage/Differs".to_owned(),
			StudioScript::saved("return 'studio version'\n"),
		),
		(
			"ReplicatedStorage/Draft".to_owned(),
			StudioScript {
				editor: "return 'saved'\n".to_owned(),
				committed: "return 'older'\n".to_owned(),
			},
		),
		(
			"ReplicatedStorage/StudioOnly".to_owned(),
			StudioScript::saved("return 'never written to disk'\n"),
		),
	]);

	spawn_studio(
		&daemon,
		studio,
		json!([{ "parent": "ReplicatedStorage", "name": "Twin", "count": 2, "classes": ["ModuleScript", "Folder"] }]),
	);

	let output = run_verify(&daemon, &["--raw"]);

	assert!(!output.status.success(), "a report with problems must exit non-zero");
	assert!(cli_stderr(&output).contains("problem(s)"), "{}", cli_stderr(&output));

	let report = cli_json(&output);

	assert_eq!(report["ok"], true, "{report}");
	assert_eq!(report["clean"], false);
	assert_eq!(report["scope"], "code");
	assert_eq!(report["settle"]["ok"], true, "the barrier ran first: {report}");

	assert_eq!(paths(&report, "mismatch"), vec!["ReplicatedStorage/Differs"]);

	let mismatch = &report["mismatch"][0];
	assert_eq!(mismatch["fsPath"], "src/Differs.luau");
	assert_eq!(mismatch["diskBytes"], "return 'disk'\n".len());
	assert_eq!(mismatch["studioBytes"], "return 'studio version'\n".len());

	assert_eq!(
		paths(&report, "missingInStudio"),
		vec!["ReplicatedStorage/Gone", "ReplicatedStorage/Stuck"]
	);
	assert_eq!(paths(&report, "missingOnDisk"), vec!["ReplicatedStorage/StudioOnly"]);
	assert_eq!(paths(&report, "draft"), vec!["ReplicatedStorage/Draft"]);
	assert_eq!(paths(&report, "backlogStuck"), vec!["src/Stuck.luau"]);
	assert!(report["backlogStuck"][0]["why"]
		.as_str()
		.unwrap()
		.contains("never reached Studio"));

	assert_eq!(report["duplicateSiblings"]["studio"][0]["name"], "Twin");
	assert_eq!(report["counts"]["mismatch"], 1);
	assert_eq!(report["counts"]["draft"], 1);
	assert_eq!(report["counts"]["staleDaemon"], 0);
	assert_eq!(report["checked"]["scripts"], 6);
	assert_eq!(report["hazards"]["count"], 0);

	// Human output groups by category
	let human = run_verify(&daemon, &["--no-wait"]);
	let text = cli_stdout(&human);

	assert!(!human.status.success());

	for heading in [
		"Content mismatch (1)",
		"Missing in Studio (2)",
		"Missing on disk (1)",
		"Duplicate siblings (1)",
		"Backlog-stuck paths (1)",
		"Unsaved editor drafts (informational) (1)",
	] {
		assert!(text.contains(heading), "missing `{heading}` in:\n{text}");
	}
}

#[test]
fn verify_is_clean_when_disk_and_studio_agree() {
	let dir = scratch_project_scoped(
		tree(),
		&[("src/One.luau", "return 1\n"), ("src/Nested/Two.luau", "return 2\n")],
		Some("code"),
	);
	let daemon = start_daemon_in(dir);

	spawn_studio(
		&daemon,
		HashMap::from([
			("ReplicatedStorage/One".to_owned(), StudioScript::saved("return 1\n")),
			(
				"ReplicatedStorage/Nested/Two".to_owned(),
				StudioScript::saved("return 2\n"),
			),
		]),
		json!([]),
	);

	let raw = run_verify(&daemon, &["--raw"]);

	assert!(raw.status.success(), "a clean verify must exit 0: {}", cli_stderr(&raw));

	let report = cli_json(&raw);

	assert_eq!(report["clean"], true, "{report}");
	assert_eq!(report["checked"]["scripts"], 2);
	assert_eq!(report["checked"]["studioScripts"], 2);

	let human = run_verify(&daemon, &[]);

	assert!(human.status.success());
	assert!(
		cli_stderr(&human).contains("disk and Studio agree"),
		"{}",
		cli_stderr(&human)
	);
}

#[test]
fn verify_without_a_plugin_is_an_error_not_a_clean_report() {
	let dir = scratch_project_scoped(tree(), &[("src/One.luau", "return 1\n")], Some("code"));
	let daemon = start_daemon_in(dir);

	let raw = run_verify(&daemon, &["--raw"]);

	assert!(!raw.status.success());
	assert_eq!(cli_json(&raw)["ok"], false);
	assert!(
		cli_stderr(&raw).contains("Verify could not run"),
		"{}",
		cli_stderr(&raw)
	);
}

#[test]
fn verify_flags_a_flat_file_next_to_a_same_named_folder() {
	// `Pair.luau` and `Pair/` both project to an instance named `Pair`: two
	// siblings no path can tell apart
	let dir = scratch_project_scoped(
		tree(),
		&[
			("src/Pair.luau", "return 'flat'\n"),
			("src/Pair/Child.luau", "return 'nested'\n"),
		],
		Some("code"),
	);
	let daemon = start_daemon_in(dir);

	spawn_studio(
		&daemon,
		HashMap::from([
			(
				"ReplicatedStorage/Pair".to_owned(),
				StudioScript::saved("return 'flat'\n"),
			),
			(
				"ReplicatedStorage/Pair/Child".to_owned(),
				StudioScript::saved("return 'nested'\n"),
			),
		]),
		json!([]),
	);

	let raw = run_verify(&daemon, &["--raw", "--no-wait"]);
	let report = cli_json(&raw);

	assert!(!raw.status.success(), "{report}");
	assert_eq!(report["counts"]["duplicateSiblings"], 1, "{report}");

	let group = &report["duplicateSiblings"]["daemon"][0];

	assert_eq!(group["name"], "Pair");
	assert_eq!(group["parent"], "ReplicatedStorage");
	assert_eq!(group["count"], 2);

	let mut fs_paths: Vec<&str> = group["fsPaths"]
		.as_array()
		.unwrap()
		.iter()
		.filter_map(Value::as_str)
		.collect();
	fs_paths.sort();
	assert_eq!(fs_paths, vec!["src/Pair", "src/Pair.luau"]);
	assert_eq!(report["settle"]["ran"], false);

	// The disk scanner sees the same pair as a (non-failing) warning
	assert_eq!(report["hazards"]["available"], true, "{report}");
	assert_eq!(report["hazards"]["count"], 0, "{report}");
	assert!(report["hazards"]["warnings"].as_u64().unwrap() >= 1, "{report}");
}

#[test]
fn verify_includes_disk_hazard_errors() {
	// Two init files in one folder: the projection silently picks one
	let dir = scratch_project_scoped(
		tree(),
		&[
			("src/Twice/init.luau", "return 'module'\n"),
			("src/Twice/init.server.luau", "print('script')\n"),
		],
		Some("code"),
	);
	let daemon = start_daemon_in(dir);

	// Studio holds what the projection picked (the first matching sync rule)
	spawn_studio(
		&daemon,
		HashMap::from([(
			"ReplicatedStorage/Twice".to_owned(),
			StudioScript::saved("print('script')\n"),
		)]),
		json!([]),
	);

	let raw = run_verify(&daemon, &["--raw", "--no-wait"]);
	let report = cli_json(&raw);

	assert!(!raw.status.success(), "a hazard error fails verify: {report}");
	assert_eq!(report["clean"], false);
	assert_eq!(report["counts"]["mismatch"], 0, "{report}");
	assert!(report["counts"]["hazards"].as_u64().unwrap() >= 1, "{report}");

	let finding = &report["hazards"]["findings"][0];

	assert_eq!(finding["kind"], "ambiguous-init", "{report}");
	assert!(finding["paths"]
		.as_array()
		.unwrap()
		.iter()
		.all(|path| path.as_str().unwrap().starts_with("src/Twice/")));
}
