//! Mutation audit-log tests (Design §6.4/§12): every `/request` response
//! whose `meta.mutation == true` appends one pinned JSON line to
//! `writes.log`, the file rotates at 10 MiB to one `.1` generation, and
//! `/hello` exposes the resolved path (plus the build identity fields).

mod common;

use serde_json::{json, Value};
use std::{collections::HashMap, fs, time::Duration};

use common::{get_json, post_json, spawn_fake_plugin, start_daemon_with_state_dir, state_dir, FakePluginScript};

const ROTATE_BYTES: usize = 10 * 1024 * 1024;

fn read_lines(path: &std::path::Path) -> Vec<Value> {
	fs::read_to_string(path)
		.unwrap_or_default()
		.lines()
		.map(|line| serde_json::from_str(line).unwrap())
		.collect()
}

#[tokio::test]
async fn hello_exposes_writes_log_and_build_identity() {
	let daemon = start_daemon_with_state_dir();

	let (status, hello) = get_json(&daemon, "/hello").await;
	assert_eq!(status, 200);

	// The daemon reports where its audit log lives so `status` can check it
	let expected = state_dir(&daemon).join("writes.log");
	assert_eq!(hello["writesLog"], expected.to_string_lossy().as_ref());

	// Build identity: a commit string (short hash or "unknown") and a dirty
	// boolean stamped at compile time
	let commit = hello["commit"].as_str().expect("commit must be a string");
	assert!(!commit.is_empty());
	assert!(hello["dirty"].is_boolean(), "dirty must be a boolean: {hello}");
}

#[tokio::test]
async fn mutating_responses_append_pinned_audit_lines() {
	let daemon = start_daemon_with_state_dir();

	let plugin = spawn_fake_plugin(
		&daemon,
		"audit-plugin",
		FakePluginScript {
			canned: HashMap::from([
				(
					"set".to_owned(),
					json!({
						"ok": true,
						"value": { "applied": true },
						"meta": {
							"op": "set",
							"durationMs": 3,
							"protocol": 1,
							"mutation": true,
							"audit": {
								"op": "set",
								"target": "Workspace/Part",
								"class": "Part",
								"prop": "Anchored",
							},
						},
					}),
				),
				(
					"rm".to_owned(),
					json!({
						"ok": false,
						"error": { "code": "PLUGIN_ERROR", "message": "locked" },
						"meta": {
							"op": "rm",
							"mutation": true,
							"audit": { "op": "rm", "target": "Workspace/Locked", "count": 1 },
						},
					}),
				),
				(
					"version".to_owned(),
					json!({
						"ok": true,
						"value": { "pluginVersion": "1.0.0" },
						"meta": { "op": "version", "mutation": false },
					}),
				),
			]),
			..FakePluginScript::default()
		},
	)
	.await;

	let log_path = state_dir(&daemon).join("writes.log");

	// A mutating success audits with the pinned fields, lifted out of the
	// response meta's nested `audit` object (the plugin's wire shape)
	let (_, reply) = post_json(
		&daemon,
		"/request",
		&json!({ "op": "set", "args": { "path": "Workspace/Part" } }),
	)
	.await;
	assert_eq!(reply["ok"], true);

	let lines = read_lines(&log_path);
	assert_eq!(lines.len(), 1, "one mutating response, one line");

	let line = &lines[0];
	assert_eq!(line["op"], "set");
	assert_eq!(line["target"], "Workspace/Part");
	assert_eq!(line["class"], "Part");
	assert_eq!(line["prop"], "Anchored");
	assert_eq!(line["ok"], true);
	assert_eq!(line["source"], "cli");
	assert!(line.get("count").is_none(), "absent meta fields stay absent");
	assert!(line.get("audit").is_none(), "the audit object is flattened, not copied");

	let ts = line["ts"].as_str().unwrap();
	assert!(
		chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
		"ts must be RFC 3339: {ts}"
	);

	// A mutating *failure* still audits, with ok: false
	let (_, reply) = post_json(&daemon, "/request", &json!({ "op": "rm", "args": {} })).await;
	assert_eq!(reply["ok"], false);

	let lines = read_lines(&log_path);
	assert_eq!(lines.len(), 2);
	assert_eq!(lines[1]["op"], "rm");
	assert_eq!(lines[1]["ok"], false);
	assert_eq!(lines[1]["target"], "Workspace/Locked");
	assert_eq!(lines[1]["count"], 1);

	// Non-mutating responses never audit
	let (_, reply) = post_json(&daemon, "/request", &json!({ "op": "version", "args": {} })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(read_lines(&log_path).len(), 2, "version is not a mutation");

	// Daemon-side ops are read-only and never audit either
	let (_, reply) = post_json(
		&daemon,
		"/request",
		&json!({ "op": "path", "args": { "target": "ReplicatedStorage/Hello" } }),
	)
	.await;
	assert_eq!(reply["ok"], true);
	assert_eq!(read_lines(&log_path).len(), 2);

	plugin.abort();
}

#[tokio::test]
async fn each_op_type_audits_its_real_target() {
	let daemon = start_daemon_with_state_dir();

	// One canned response per mutating op family, each carrying the exact
	// audit object the plugin's WriteRules.shapeAudit builds for it
	// (plugin/src/Remote/Handlers/{Write,History,Clipboard}.luau)
	let ops: Vec<(&str, Value)> = vec![
		(
			"new",
			json!({ "op": "new", "target": "Workspace/Part", "class": "Part" }),
		),
		(
			"mv",
			json!({
				"op": "mv",
				"target": "Workspace/Part",
				"class": "Part",
				"to": "ServerStorage/Part",
			}),
		),
		(
			"set",
			json!({ "op": "set", "target": "Workspace/Part", "class": "Part", "prop": "Anchored" }),
		),
		(
			"set_batch",
			json!({ "op": "set_batch", "target": "Workspace/Part", "count": 3 }),
		),
		("rm", json!({ "op": "rm", "target": "Workspace/Part", "class": "Part" })),
		(
			"set_attr",
			json!({ "op": "set_attr", "target": "Workspace/Part", "class": "Part", "prop": "Health" }),
		),
		(
			"rm_attr",
			json!({ "op": "rm_attr", "target": "Workspace/Part", "class": "Part", "prop": "Health" }),
		),
		(
			"add_tag",
			json!({ "op": "add_tag", "target": "Workspace/Part", "class": "Part", "prop": "Enemy" }),
		),
		(
			"rm_tag",
			json!({ "op": "rm_tag", "target": "Workspace/Part", "class": "Part", "prop": "Enemy" }),
		),
		(
			"call",
			json!({ "op": "call", "target": "Workspace/Part", "class": "Part", "prop": "MakeJoints" }),
		),
		(
			"clipboard_paste_commit",
			json!({ "op": "clipboard_paste_commit", "target": "Workspace/Pasted", "count": 2 }),
		),
		// DataModel-scoped ops legitimately audit an empty target
		("eval", json!({ "op": "eval", "target": "" })),
		("save", json!({ "op": "save", "target": "" })),
	];

	let canned = ops
		.iter()
		.map(|(op, audit)| {
			(
				(*op).to_owned(),
				json!({
					"ok": true,
					"value": null,
					"meta": {
						"op": op,
						"durationMs": 1,
						"protocol": 1,
						"mutation": true,
						"audit": audit,
					},
				}),
			)
		})
		.collect::<HashMap<_, _>>();

	let plugin = spawn_fake_plugin(
		&daemon,
		"audit-ops-plugin",
		FakePluginScript {
			canned,
			..FakePluginScript::default()
		},
	)
	.await;

	for (op, _) in &ops {
		let (_, reply) = post_json(&daemon, "/request", &json!({ "op": op, "args": {} })).await;
		assert_eq!(reply["ok"], true, "op {op} must succeed: {reply}");
	}

	let lines = read_lines(&state_dir(&daemon).join("writes.log"));
	assert_eq!(lines.len(), ops.len(), "every mutating op appends one line");

	for ((op, audit), line) in ops.iter().zip(&lines) {
		assert_eq!(line["op"], *op);
		assert_eq!(line["target"], audit["target"], "op {op} must audit its real target");

		// Every pinned optional field the op reported must land verbatim
		for field in ["class", "prop", "to", "count"] {
			match audit.get(field) {
				Some(expected) => assert_eq!(&line[field], expected, "op {op} field {field}"),
				None => assert!(line.get(field).is_none(), "op {op} must not invent {field}"),
			}
		}
	}

	plugin.abort();
}

#[tokio::test]
async fn writes_log_rotates_once_past_ten_mebibytes() {
	let daemon = start_daemon_with_state_dir();

	let plugin = spawn_fake_plugin(
		&daemon,
		"rotate-plugin",
		FakePluginScript {
			canned: HashMap::from([(
				"set".to_owned(),
				json!({
					"ok": true,
					"value": null,
					"meta": { "op": "set", "mutation": true, "audit": { "op": "set", "target": "X" } },
				}),
			)]),
			..FakePluginScript::default()
		},
	)
	.await;

	// Pre-size the live log to the rotation threshold
	let log_path = state_dir(&daemon).join("writes.log");
	fs::create_dir_all(log_path.parent().unwrap()).unwrap();
	fs::write(&log_path, vec![b'x'; ROTATE_BYTES]).unwrap();

	let (_, reply) = post_json(&daemon, "/request", &json!({ "op": "set", "args": {} })).await;
	assert_eq!(reply["ok"], true);

	// Give the daemon a beat in case of filesystem laziness
	tokio::time::sleep(Duration::from_millis(100)).await;

	let rotated = log_path.with_extension("log.1");
	assert!(rotated.exists(), "writes.log.1 must hold the rotated generation");
	assert_eq!(fs::metadata(&rotated).unwrap().len() as usize, ROTATE_BYTES);

	let lines = read_lines(&log_path);
	assert_eq!(lines.len(), 1, "the live log restarts with the new line");
	assert_eq!(lines[0]["target"], "X");

	plugin.abort();
}
