//! Integration coverage for the Open Cloud command family (`wsync upload`,
//! `wsync monetization`): the real binary against a local HTTP stub selected
//! through `WSYNC_CLOUD_BASE_URL`, asserting the documented auth headers,
//! multipart shapes, operation polling, per-file failure tolerance, manifest
//! output, and the credential chain (auth store first, then the named env
//! var, then the fallback env vars).

mod common;

use serde_json::{json, Value};
use std::{
	fs,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
};

use common::{cli_stderr, cli_stdout, start_cloud_stub, CliSandbox, CloudStub, StubRequest};

/// Every JSON line on stdout (`upload`/`images` print NDJSON under `--raw`)
fn ndjson(output: &std::process::Output) -> Vec<Value> {
	cli_stdout(output)
		.lines()
		.filter(|line| !line.trim().is_empty())
		.map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("stdout is not NDJSON ({err}): {line}")))
		.collect()
}

/// A scratch project directory carrying ids for creator/universe defaults
fn cloud_project(sandbox: &CliSandbox, game_id: u64, group_id: u64) -> std::path::PathBuf {
	let dir = sandbox.work.path().join("project");

	fs::create_dir_all(dir.join("src")).unwrap();
	fs::write(dir.join("src").join("Hello.luau"), "return 1\n").unwrap();
	fs::write(
		dir.join("default.project.json"),
		serde_json::to_string_pretty(&json!({
			"name": "cloud-fixture",
			"tree": { "$className": "DataModel", "ReplicatedStorage": { "$path": "src" } },
			"gameId": game_id,
			"groupId": group_id,
		}))
		.unwrap(),
	)
	.unwrap();

	dir
}

/// An assets-API stub: create answers an operation, the operation completes
/// on the second poll, and `fail_name` (matched on the multipart request
/// JSON's displayName) is refused with a 400
fn assets_stub(fail_name: Option<&'static str>) -> CloudStub {
	let polls = Arc::new(AtomicUsize::new(0));

	start_cloud_stub(Arc::new(move |request: &StubRequest| {
		if request.method == "POST" && request.path == "/assets/v1/assets" {
			let display_name = request
				.part_json("request")
				.and_then(|body| body.get("displayName").and_then(Value::as_str).map(str::to_owned))
				.unwrap_or_default();

			if Some(display_name.as_str()) == fail_name {
				return (400, json!({ "message": "moderated name" }));
			}

			return (
				200,
				json!({ "path": format!("operations/op-{display_name}"), "operationId": format!("op-{display_name}") }),
			);
		}

		if request.method == "GET" && request.path.starts_with("/assets/v1/operations/") {
			// First poll: still running; afterwards: done
			if polls.fetch_add(1, Ordering::SeqCst) == 0 {
				return (200, json!({ "done": false }));
			}

			return (200, json!({ "done": true, "response": { "assetId": "98765" } }));
		}

		(404, json!({ "message": "unexpected route" }))
	}))
}

#[test]
fn upload_sends_multipart_polls_the_operation_and_uses_the_stored_credential() {
	let stub = assets_stub(None);
	let sandbox = CliSandbox::new();

	// The credential chain's first tier: the auth store
	let stored = sandbox.run_with_stdin(&["auth", "set", "--from-stdin"], b"sk-store-secret\n");

	assert!(stored.status.success(), "auth set failed: {}", cli_stderr(&stored));

	let icon = sandbox.work.path().join("icon.png");
	let pixels: Vec<u8> = vec![137, 80, 78, 71, 13, 10, 26, 10, 1, 2, 3];

	fs::write(&icon, &pixels).unwrap();

	let output = sandbox.run_with_envs(
		&["upload", &icon.to_string_lossy(), "--creator", "user:5501", "--raw"],
		&[("WSYNC_CLOUD_BASE_URL", &stub.base_url)],
	);

	assert!(output.status.success(), "upload failed: {}", cli_stderr(&output));

	// The create request: auth header, multipart shape, inferred type,
	// creator context
	let creates = stub.requests_to("/assets/v1/assets");

	assert_eq!(creates.len(), 1);
	assert_eq!(creates[0].header("x-api-key"), Some("sk-store-secret"));
	assert!(creates[0].header("authorization").is_none());

	let request_json = creates[0].part_json("request").expect("no `request` multipart field");

	assert_eq!(request_json["assetType"], "Image");
	assert_eq!(request_json["displayName"], "icon");
	assert_eq!(request_json["creationContext"]["creator"]["userId"], "5501");

	let file_part = creates[0]
		.multipart()
		.into_iter()
		.find(|part| part.name.as_deref() == Some("fileContent"))
		.expect("no fileContent part");

	assert_eq!(file_part.filename.as_deref(), Some("icon.png"));
	assert_eq!(file_part.content_type.as_deref(), Some("image/png"));
	assert_eq!(file_part.data, pixels);

	// The operation was really polled to completion (pending, then done)
	assert_eq!(stub.requests_to("/assets/v1/operations/op-icon").len(), 2);

	let records = ndjson(&output);

	assert_eq!(records.len(), 1);
	assert_eq!(records[0]["ok"], true);
	assert_eq!(records[0]["status"], "uploaded");
	assert_eq!(records[0]["assetId"], "98765");
	assert_eq!(records[0]["operationId"], "op-icon");
}

#[test]
fn upload_recurses_skips_defaults_the_group_creator_and_tolerates_failures() {
	let stub = assets_stub(Some("b"));
	let sandbox = CliSandbox::new();
	let project = cloud_project(&sandbox, 777, 9988);

	let assets = sandbox.work.path().join("assets");

	fs::create_dir_all(assets.join("sub")).unwrap();
	fs::write(assets.join("a.png"), b"png-a").unwrap();
	fs::write(assets.join("b.mp3"), b"mp3-b").unwrap();
	fs::write(assets.join("notes.txt"), b"not an asset").unwrap();
	fs::write(assets.join("sub").join("c.jpg"), b"jpg-c").unwrap();

	let manifest = sandbox.work.path().join("uploaded.json");

	let output = sandbox.run_with_envs(
		&[
			"upload",
			&assets.to_string_lossy(),
			"--project",
			&project.to_string_lossy(),
			"--manifest",
			&manifest.to_string_lossy(),
			"--no-wait",
			"--raw",
		],
		&[
			("WSYNC_CLOUD_BASE_URL", &stub.base_url),
			("ROBLOX_API_KEY", "sk-env-key"),
		],
	);

	// One file failed, so the batch exits non-zero — after finishing
	assert!(!output.status.success(), "a failed upload must fail the batch");

	// Every record arrived: the skip, two pending creates, one failure
	let records = ndjson(&output);

	assert_eq!(records.len(), 4);

	let by_suffix = |suffix: &str| -> &Value {
		records
			.iter()
			.find(|record| record["file"].as_str().is_some_and(|file| file.ends_with(suffix)))
			.unwrap_or_else(|| panic!("no record for {suffix}"))
	};

	assert_eq!(by_suffix("notes.txt")["status"], "skipped");
	assert_eq!(by_suffix("a.png")["status"], "pending");
	assert_eq!(by_suffix("a.png")["operationId"], "op-a");
	assert_eq!(by_suffix("c.jpg")["status"], "pending");
	assert_eq!(by_suffix("b.mp3")["status"], "failed");
	assert!(
		by_suffix("b.mp3")["error"]
			.as_str()
			.is_some_and(|error| error.contains("moderated name")),
		"the upstream error must be preserved: {}",
		by_suffix("b.mp3")["error"]
	);

	// --no-wait: no operation was polled
	assert!(stub.requests_to("/operations/").is_empty());

	// The creator defaulted to the project's groupId
	let creates = stub.requests_to("/assets/v1/assets");

	assert_eq!(creates.len(), 3, "three uploadable files, three creates");

	for create in &creates {
		let request_json = create.part_json("request").unwrap();

		assert_eq!(request_json["creationContext"]["creator"]["groupId"], "9988");
	}

	// The audio file was typed by extension
	let audio = creates
		.iter()
		.find(|create| {
			create
				.part_json("request")
				.is_some_and(|body| body["displayName"] == "b")
		})
		.unwrap();

	assert_eq!(audio.part_json("request").unwrap()["assetType"], "Audio");

	// The manifest carries the complete batch
	let manifest: Value = serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();

	assert_eq!(manifest["ok"], false);
	assert_eq!(manifest["creator"], "group:9988");
	assert_eq!(manifest["pending"], 2);
	assert_eq!(manifest["failed"], 1);
	assert_eq!(manifest["skipped"], 1);
	assert_eq!(manifest["results"].as_array().unwrap().len(), 4);
}

#[test]
fn upload_credential_chain_orders_store_over_named_env_and_supports_bearer() {
	let stub = assets_stub(None);
	let sandbox = CliSandbox::new();
	let icon = sandbox.work.path().join("icon.png");

	fs::write(&icon, b"png").unwrap();

	// No credential anywhere: refused before any HTTP request
	let output = sandbox.run_with_envs(
		&["upload", &icon.to_string_lossy(), "--creator", "user:1", "--raw"],
		&[("WSYNC_CLOUD_BASE_URL", &stub.base_url)],
	);

	assert!(!output.status.success());
	assert!(
		cli_stderr(&output).contains("wsync auth"),
		"the refusal must name the credential chain: {}",
		cli_stderr(&output)
	);
	assert!(
		stub.requests().is_empty(),
		"no request may be sent without a credential"
	);

	// --auth bearer with a fallback env var: Authorization header, no api key
	let output = sandbox.run_with_envs(
		&[
			"upload",
			&icon.to_string_lossy(),
			"--creator",
			"user:1",
			"--auth",
			"bearer",
			"--no-wait",
			"--raw",
		],
		&[
			("WSYNC_CLOUD_BASE_URL", &stub.base_url),
			("ROBLOX_API_KEY", "oauth-token"),
		],
	);

	assert!(output.status.success(), "bearer upload failed: {}", cli_stderr(&output));

	let creates = stub.requests_to("/assets/v1/assets");

	assert_eq!(
		creates.last().unwrap().header("authorization"),
		Some("Bearer oauth-token")
	);
	assert!(creates.last().unwrap().header("x-api-key").is_none());

	// The named env var beats the fallback names…
	let output = sandbox.run_with_envs(
		&[
			"upload",
			&icon.to_string_lossy(),
			"--creator",
			"user:1",
			"--api-key-env",
			"MY_KEY",
			"--no-wait",
			"--raw",
		],
		&[
			("WSYNC_CLOUD_BASE_URL", &stub.base_url),
			("MY_KEY", "sk-named"),
			("ROBLOX_API_KEY", "sk-fallback"),
		],
	);

	assert!(output.status.success());
	assert_eq!(
		stub.requests_to("/assets/v1/assets")
			.last()
			.unwrap()
			.header("x-api-key"),
		Some("sk-named")
	);

	// …but the auth store beats even the named env var (upload.json order)
	let stored = sandbox.run_with_stdin(&["auth", "set", "--from-stdin"], b"sk-store-wins\n");

	assert!(stored.status.success());

	let output = sandbox.run_with_envs(
		&[
			"upload",
			&icon.to_string_lossy(),
			"--creator",
			"user:1",
			"--api-key-env",
			"MY_KEY",
			"--no-wait",
			"--raw",
		],
		&[("WSYNC_CLOUD_BASE_URL", &stub.base_url), ("MY_KEY", "sk-named")],
	);

	assert!(output.status.success());
	assert_eq!(
		stub.requests_to("/assets/v1/assets")
			.last()
			.unwrap()
			.header("x-api-key"),
		Some("sk-store-wins")
	);
}

/// A monetization stub serving the game-pass and developer-product
/// endpoints for universe 777
fn monetization_stub() -> CloudStub {
	start_cloud_stub(Arc::new(|request: &StubRequest| {
		match (request.method.as_str(), request.path.as_str()) {
			("GET", "/game-passes/v1/universes/777/game-passes/creator") => (
				200,
				json!({ "gamePasses": [ { "gamePassId": 4242, "name": "VIP", "price": 499, "isForSale": true } ] }),
			),
			("POST", "/game-passes/v1/universes/777/game-passes") => (200, json!({ "gamePassId": 4242 })),
			("PATCH", "/game-passes/v1/universes/777/game-passes/4242")
			| ("PATCH", "/game-passes/v1/universes/777/game-passes/9") => (200, json!({})),
			("GET", "/developer-products/v2/universes/777/developer-products/creator") => (
				200,
				json!({ "developerProducts": [
					{ "productId": 5, "name": "Coins Small", "price": 49 },
					{ "productId": 6, "name": "Coins Large", "price": 399 },
				] }),
			),
			("POST", "/developer-products/v2/universes/777/developer-products") => (200, json!({ "productId": 7 })),
			("PATCH", "/developer-products/v2/universes/777/developer-products/5")
			| ("PATCH", "/developer-products/v2/universes/777/developer-products/6") => (200, json!({})),
			_ => (404, json!({ "message": "unexpected route" })),
		}
	}))
}

#[test]
fn monetization_creates_edits_and_lists_gamepasses_through_the_aliases() {
	let stub = monetization_stub();
	let sandbox = CliSandbox::new();
	let project = cloud_project(&sandbox, 777, 0);
	let envs = [
		("WSYNC_CLOUD_BASE_URL", stub.base_url.as_str()),
		("ROBLOX_API_KEY", "sk-mono"),
	];

	// Create through the `gp` alias, entry-format parsing included
	let output = sandbox.run_with_envs(
		&[
			"monetization",
			"gp",
			"create",
			"VIP 499 robux",
			"--universe-id",
			"777",
			"--raw",
		],
		&envs,
	);

	assert!(output.status.success(), "create failed: {}", cli_stderr(&output));

	let creates = stub.requests_to("/game-passes/v1/universes/777/game-passes");
	let create = creates.iter().find(|request| request.method == "POST").unwrap();

	assert_eq!(create.header("x-api-key"), Some("sk-mono"));
	assert_eq!(create.part_text("name").as_deref(), Some("VIP"));
	assert_eq!(create.part_text("price").as_deref(), Some("499"));
	assert_eq!(create.part_text("isForSale").as_deref(), Some("true"));

	let record = ndjson(&output);

	assert_eq!(record[0]["ok"], true);
	assert_eq!(record[0]["id"], "4242");

	// Edit by name: resolved through the list endpoint, then patched
	let output = sandbox.run_with_envs(
		&[
			"monetization",
			"gamepass",
			"edit",
			"--name",
			"VIP",
			"--price",
			"699",
			"--universe-id",
			"777",
			"--raw",
		],
		&envs,
	);

	assert!(output.status.success(), "edit failed: {}", cli_stderr(&output));
	assert!(
		!stub
			.requests_to("/game-passes/v1/universes/777/game-passes/creator")
			.is_empty(),
		"--name resolution must consult the list endpoint"
	);

	let patch = stub
		.requests_to("/game-passes/v1/universes/777/game-passes/4242")
		.into_iter()
		.find(|request| request.method == "PATCH")
		.expect("no PATCH reached the item endpoint");

	assert_eq!(patch.part_text("price").as_deref(), Some("699"));
	assert!(
		patch.part_text("name").is_none(),
		"unchanged fields stay out of the form"
	);

	// List with the universe defaulted from the project's gameId
	let output = sandbox.run_with_envs(
		&[
			"monetization",
			"gamepasses",
			"list",
			"--project",
			&project.to_string_lossy(),
			"--raw",
		],
		&envs,
	);

	assert!(output.status.success(), "list failed: {}", cli_stderr(&output));

	let listed = ndjson(&output);

	assert_eq!(listed[0]["ok"], true);
	assert_eq!(listed[0]["universeId"], 777);
	assert_eq!(listed[0]["count"], 1);
	assert_eq!(listed[0]["items"][0]["name"], "VIP");
}

#[test]
fn monetization_images_matches_normalized_filenames_and_image_uses_kind_fields() {
	let stub = monetization_stub();
	let sandbox = CliSandbox::new();
	let envs = [
		("WSYNC_CLOUD_BASE_URL", stub.base_url.as_str()),
		("CLOUD_API_KEY", "sk-imgs"),
	];

	let icons = sandbox.work.path().join("icons");

	fs::create_dir_all(&icons).unwrap();
	fs::write(icons.join("coins-small.png"), b"png-coins").unwrap();
	fs::write(icons.join("stray.png"), b"png-stray").unwrap();

	// `dp images`: coins-small.png matches "Coins Small"; stray.png matches
	// nothing and is reported, not fatal
	let output = sandbox.run_with_envs(
		&[
			"monetization",
			"dp",
			"images",
			&icons.to_string_lossy(),
			"--universe-id",
			"777",
			"--raw",
		],
		&envs,
	);

	assert!(output.status.success(), "images failed: {}", cli_stderr(&output));

	let uploads = stub.requests_to("/developer-products/v2/universes/777/developer-products/5");

	assert_eq!(uploads.len(), 1);
	assert_eq!(uploads[0].method, "PATCH");

	let part = uploads[0]
		.multipart()
		.into_iter()
		.find(|part| part.name.as_deref() == Some("imageFile"))
		.expect("developer products upload through the imageFile field");

	assert_eq!(part.filename.as_deref(), Some("coins-small.png"));
	assert_eq!(part.data, b"png-coins");
	assert!(
		stub.requests_to("/developer-products/6").is_empty(),
		"no image matched Coins Large, so nothing may be uploaded for it"
	);

	let records = ndjson(&output);

	assert!(records.iter().any(|record| record["status"] == "unmatched"));
	assert!(records.iter().any(|record| record["ok"] == true && record["id"] == "5"));

	// `gp image --id`: game passes update through the `file` field
	let shot = sandbox.work.path().join("pass.png");

	fs::write(&shot, b"png-pass").unwrap();

	let output = sandbox.run_with_envs(
		&[
			"monetization",
			"gp",
			"image",
			&shot.to_string_lossy(),
			"--id",
			"9",
			"--universe-id",
			"777",
			"--raw",
		],
		&envs,
	);

	assert!(output.status.success(), "image failed: {}", cli_stderr(&output));

	let uploads = stub.requests_to("/game-passes/v1/universes/777/game-passes/9");

	assert_eq!(uploads.len(), 1);

	let part = uploads[0]
		.multipart()
		.into_iter()
		.find(|part| part.name.as_deref() == Some("file"))
		.expect("game-pass updates upload through the `file` field");

	assert_eq!(part.data, b"png-pass");
}

#[test]
fn monetization_discover_reports_locally_without_a_credential() {
	let sandbox = CliSandbox::new();
	let project = cloud_project(&sandbox, 777, 0);

	fs::write(project.join("src").join("Gamepasses.luau"), "return {}\n").unwrap();
	fs::write(project.join(".env"), "CLOUD_API_KEY=sk-env-file\n").unwrap();

	// No WSYNC_CLOUD_BASE_URL, no credential env: discover is local-only
	let output = sandbox.run(&[
		"monetization",
		"gamepass",
		"discover",
		"--project",
		&project.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "discover failed: {}", cli_stderr(&output));

	let record = ndjson(&output);

	assert_eq!(record[0]["ok"], true);
	assert_eq!(record[0]["universe"]["id"], 777);
	assert_eq!(record[0]["credential"]["configured"], true);
	assert!(
		record[0]["credential"]["source"]
			.as_str()
			.is_some_and(|source| source.contains(".env")),
		"the env-file credential source must be named: {}",
		record[0]["credential"]
	);
	assert!(
		record[0]["files"]
			.as_array()
			.unwrap()
			.iter()
			.any(|file| file.as_str().is_some_and(|file| file.contains("Gamepasses.luau"))),
		"the likely config file must be reported: {}",
		record[0]["files"]
	);

	// The credential value itself must never appear anywhere in the output
	assert!(!cli_stdout(&output).contains("sk-env-file"));
	assert!(!cli_stderr(&output).contains("sk-env-file"));
}
