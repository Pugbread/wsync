//! Disk layout hazards end to end: the hazard scanner (`core::hazards`),
//! `wsync doctor`'s offline checks, and a live daemon that must survive two
//! init files in one folder — the incident where it died with a stack
//! overflow and an empty log.

mod common;

use serde_json::{json, Value};
use std::{
	collections::BTreeSet,
	fs,
	net::TcpListener,
	path::{Path, PathBuf},
	thread,
	time::{Duration, Instant},
};

use common::{cli_json, cli_stdout, scratch_project_custom, start_daemon_in, CliSandbox};
use wsync::{
	core::hazards::{self, HazardKind, Severity, SOURCE_SIZE_LIMIT},
	project::Project,
};

/// A project on disk mapping `ServerScriptService` to `src`
fn project(dir: &Path, files: &[(&str, &str)], extra: Value) -> PathBuf {
	for (path, contents) in files {
		let path = dir.join(path);
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, contents).unwrap();
	}

	let mut project = json!({
		"name": "hazards",
		"tree": { "$className": "DataModel", "ServerScriptService": { "$path": "src" } },
		"scope": "full",
	});

	for (key, value) in extra.as_object().unwrap() {
		project[key] = value.clone();
	}

	let path = dir.join("default.project.json");
	fs::write(&path, serde_json::to_string_pretty(&project).unwrap()).unwrap();

	path
}

fn scan(project_path: &Path) -> hazards::ScanReport {
	hazards::scan(&Project::load(project_path).unwrap())
}

fn kinds(report: &hazards::ScanReport) -> BTreeSet<HazardKind> {
	report.hazards.iter().map(|hazard| hazard.kind).collect()
}

fn hazard(report: &hazards::ScanReport, kind: HazardKind) -> &hazards::Hazard {
	report
		.hazards
		.iter()
		.find(|hazard| hazard.kind == kind)
		.unwrap_or_else(|| panic!("no {kind} hazard in {:#?}", report.hazards))
}

/// Every layout the incident left behind, one of each
const INCIDENT: &[(&str, &str)] = &[
	("src/CarFlareClient/init.client.luau", "print('client')"),
	("src/CarFlareClient/init.luau", "return {}"),
	("src/Shared/EditableVideo.luau", "return 'flat'"),
	("src/Shared/EditableVideo/init.luau", "return 'folder'"),
	("src/Shared/Moved.luau", "return 'moved'"),
	("src/Shared/Moved/.DS_Store", ""),
	("src/Shared/Foo.luau", "return 1"),
	("src/Shared/Foo.server.luau", "print(1)"),
	("src/Shared/.Staged.wsync-reshape/reshape.json", "{}"),
];

#[test]
fn the_scanner_reports_every_hazard_with_its_winner() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().canonicalize().unwrap();
	let project_path = project(&root, INCIDENT, json!({}));

	let report = scan(&project_path);

	assert_eq!(
		kinds(&report),
		BTreeSet::from([
			HazardKind::AmbiguousInit,
			HazardKind::FlatFolderPair,
			HazardKind::EmptyFolder,
			HazardKind::ReshapeLeftover,
			HazardKind::DuplicateSibling,
		]),
		"{:#?}",
		report.hazards
	);
	assert_eq!(report.hazards.len(), 5);
	assert_eq!(report.errors(), 4);
	assert_eq!(report.warnings(), 1);
	assert_eq!(report.roots, vec![root.join("src")]);

	// The first sync rule wins, exactly as the snapshot walk resolves it
	let init = hazard(&report, HazardKind::AmbiguousInit);
	assert_eq!(init.severity, Severity::Error);
	assert_eq!(
		init.winner.as_deref(),
		Some(root.join("src/CarFlareClient/init.client.luau").as_path())
	);
	assert_eq!(init.paths[0], root.join("src/CarFlareClient/init.client.luau"));
	assert!(init.message.contains("ignores init.luau"), "{}", init.message);
	assert!(init.fix.contains("init.luau"), "{}", init.fix);

	let pair = hazard(&report, HazardKind::FlatFolderPair);
	assert_eq!(
		pair.winner.as_deref(),
		Some(root.join("src/Shared/EditableVideo").as_path())
	);
	assert!(pair.paths.contains(&root.join("src/Shared/EditableVideo.luau")));

	let empty = hazard(&report, HazardKind::EmptyFolder);
	assert_eq!(
		empty.winner.as_deref(),
		Some(root.join("src/Shared/Moved.luau").as_path())
	);
	assert!(empty.fix.contains("Delete the empty folder"), "{}", empty.fix);

	let duplicate = hazard(&report, HazardKind::DuplicateSibling);
	assert_eq!(duplicate.severity, Severity::Warning);
	assert_eq!(duplicate.paths.len(), 2);

	// The JSON form the doctor and a future `verify` print
	let json = report.to_json();
	assert_eq!(json["errors"], 4);
	assert_eq!(json["hazards"][0]["kind"], "ambiguous-init");
	assert_eq!(json["hazards"][0]["severity"], "error");
}

#[test]
fn a_clean_project_scans_clean() {
	let dir = tempfile::tempdir().unwrap();
	let project_path = project(
		dir.path(),
		&[
			("src/Client/init.client.luau", "print(1)"),
			("src/Client/Util.luau", "return 1"),
			("src/Shared/Config.luau", "return {}"),
			// A folder with real children beside a same-named data sidecar
			// is not a pair
			("src/Shared/Assets/Icon.luau", "return 1"),
			("src/Shared/Assets.meta.json", "{}"),
		],
		json!({}),
	);

	let report = scan(&project_path);

	assert!(report.is_clean(), "{:#?}", report.hazards);
	assert_eq!(report.directories, 4);
}

#[test]
fn oversized_files_respect_ignore_globs() {
	let dir = tempfile::tempdir().unwrap();
	let big = "-".repeat(SOURCE_SIZE_LIMIT as usize + 1);
	let project_path = project(
		dir.path(),
		&[("src/Huge.luau", &big), ("src/Vendor/Huge.luau", &big)],
		json!({ "globIgnorePaths": ["**/Vendor/**"] }),
	);

	let report = scan(&project_path);

	assert_eq!(kinds(&report), BTreeSet::from([HazardKind::OversizedFile]));
	assert_eq!(report.hazards.len(), 1, "the ignored copy is not flagged");
	assert!(report.hazards[0].paths[0].ends_with("src/Huge.luau"));
	assert_eq!(report.hazards[0].severity, Severity::Warning);
}

/// Code scope projects only `.luau` sources, so a same-named text file is
/// no duplicate there
#[test]
fn the_scanner_follows_the_projects_scope() {
	let dir = tempfile::tempdir().unwrap();
	let files = [("src/Foo.luau", "return 1"), ("src/Foo.txt", "text")];

	let full = project(dir.path(), &files, json!({}));
	assert_eq!(kinds(&scan(&full)), BTreeSet::from([HazardKind::DuplicateSibling]));

	let code = project(dir.path(), &files, json!({ "scope": "code" }));
	assert!(scan(&code).is_clean());
}

#[test]
fn a_project_mapping_its_own_directory_is_a_scan_error() {
	let dir = tempfile::tempdir().unwrap();
	let project_path = project(dir.path(), &[("Hello.luau", "return 1")], json!({}));

	fs::write(
		&project_path,
		r#"{"name":"cycle","tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"."}}}"#,
	)
	.unwrap();

	let report = scan(&project_path);
	let error = hazard(&report, HazardKind::ScanError);

	assert!(error.message.contains("contains itself"), "{}", error.message);
}

/// A port with nothing listening
fn free_port() -> u16 {
	TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn doctor_runs_the_disk_checks_offline_and_fails_on_hazards() {
	let sandbox = CliSandbox::new();
	let dir = sandbox.work.path().join("project");
	fs::create_dir_all(&dir).unwrap();
	project(&dir, INCIDENT, json!({}));

	let port = free_port().to_string();
	let output = sandbox.run(&["doctor", "--project", &dir.to_string_lossy(), "--port", &port, "--raw"]);

	assert!(!output.status.success(), "error-level hazards fail the doctor");

	let report = cli_json(&output);

	assert_eq!(report["ok"], false);
	assert_eq!(report["offline"], true);

	let check = |id: &str| {
		report["checks"]
			.as_array()
			.unwrap()
			.iter()
			.find(|check| check["id"] == id)
			.unwrap_or_else(|| panic!("no {id} check in {report}"))
			.clone()
	};

	assert_eq!(check("daemon")["status"], "warn", "offline is reported, not failed");
	assert_eq!(check("disk-hazards")["status"], "fail");
	assert!(check("disk-hazards")["detail"]
		.as_str()
		.unwrap()
		.starts_with("offline check:"));

	let kinds: BTreeSet<&str> = report["hazards"]["hazards"]
		.as_array()
		.unwrap()
		.iter()
		.map(|hazard| hazard["kind"].as_str().unwrap())
		.collect();

	assert_eq!(
		kinds,
		BTreeSet::from([
			"ambiguous-init",
			"flat-folder-pair",
			"empty-folder",
			"reshape-leftover",
			"duplicate-sibling"
		])
	);

	// The human report labels the section and names each fix
	let human = sandbox.run(&["doctor", "--project", &dir.to_string_lossy(), "--port", &port]);
	let text = cli_stdout(&human);

	assert!(!human.status.success());
	assert!(text.contains("Offline checks"), "{text}");
	assert!(text.contains("fix: Keep exactly one init file"), "{text}");
}

#[test]
fn doctor_passes_offline_on_a_clean_project() {
	let sandbox = CliSandbox::new();
	let dir = sandbox.work.path().join("project");
	fs::create_dir_all(&dir).unwrap();
	project(&dir, &[("src/Main.server.luau", "print(1)")], json!({}));

	let port = free_port().to_string();
	let output = sandbox.run(&["doctor", "--project", &dir.to_string_lossy(), "--port", &port, "--raw"]);
	let report = cli_json(&output);

	assert!(output.status.success(), "{report}");
	assert_eq!(report["ok"], true);
	assert_eq!(report["offline"], true);
	assert_eq!(report["hazards"]["errors"], 0);
}

fn get_json(url: &str) -> Value {
	reqwest::blocking::Client::builder()
		.timeout(Duration::from_secs(5))
		.build()
		.unwrap()
		.get(url)
		.send()
		.unwrap_or_else(|err| panic!("GET {url} must answer: {err}"))
		.json()
		.unwrap()
}

/// The children of `ServerScriptService` in the daemon's tree
fn service_children(daemon: &common::TestDaemon) -> Vec<Value> {
	let tree = get_json(&daemon.http("/snapshot"));

	tree["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == "ServerScriptService")
		.and_then(|service| service["children"].as_array().cloned())
		.unwrap_or_default()
}

/// The incident, live: the daemon boots over two init files, and survives a
/// second init file appearing while it runs (the processor thread used to
/// overflow its stack and abort the whole process)
#[test]
fn a_daemon_boots_over_and_survives_two_init_files() {
	let dir = scratch_project_custom(
		json!({ "$className": "DataModel", "ServerScriptService": { "$path": "src" } }),
		&[
			("src/CarFlareClient/init.client.luau", "print('client')"),
			("src/CarFlareClient/init.luau", "return {}"),
			("src/Other/init.server.luau", "print('server')"),
			("src/Other/Child.luau", "return 1"),
		],
	);

	let daemon = start_daemon_in(dir);

	let classes = |children: &[Value]| -> Vec<(String, String)> {
		let mut classes: Vec<(String, String)> = children
			.iter()
			.map(|child| {
				(
					child["name"].as_str().unwrap().to_owned(),
					child["class"].as_str().unwrap_or_default().to_owned(),
				)
			})
			.collect();
		classes.sort();
		classes
	};

	assert_eq!(
		classes(&service_children(&daemon)),
		vec![
			("CarFlareClient".into(), "Script".into()),
			("Other".into(), "Script".into())
		]
	);

	// A second init file appears live, then more edits in the same folder
	let other = daemon.src_dir().join("Other");
	fs::write(other.join("init.luau"), "return 'module'").unwrap();

	for round in 0..3 {
		thread::sleep(Duration::from_millis(400));
		fs::write(other.join("Child.luau"), format!("return {round}")).unwrap();
	}

	// The daemon keeps answering, and `Other` stays the `.server` script
	// with its child
	let deadline = Instant::now() + Duration::from_secs(10);

	loop {
		let children = service_children(&daemon);
		let other = children.iter().find(|child| child["name"] == "Other").cloned();
		let settled = other.as_ref().is_some_and(|other| {
			other["class"] == "Script"
				&& other["children"].as_array().is_some_and(|children| {
					children.len() == 1 && children[0]["properties"]["Source"].to_string().contains("return 2")
				})
		});

		if settled {
			break;
		}

		assert!(
			Instant::now() < deadline,
			"the tree never settled: {:#}",
			json!(children)
		);

		thread::sleep(Duration::from_millis(200));
	}

	assert_eq!(get_json(&daemon.http("/hello"))["name"], "wsync-fixture");
}
