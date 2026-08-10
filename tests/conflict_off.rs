//! With `conflict_engine = false` the daemon must behave exactly like the
//! engine-less fork: last-writer-wins, no parking, an empty `/resolve`
//! listing. Config is process-global, so this test owns its own binary.

mod common;

use futures_util::SinkExt;
use serde_json::{json, Value};
use std::{fs, time::Duration};
use tokio_tungstenite::tungstenite::Message;

use common::{connect_client, start_daemon, wait_for_frame};

#[tokio::test]
async fn engine_off_keeps_last_writer_wins() {
	wsync::config::Config::new_mut().conflict_engine = false;

	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "off-plugin").await;

	tokio::time::sleep(Duration::from_millis(600)).await;

	// Resolve the script ref
	let snapshot: Value = reqwest::get(daemon.http("/snapshot"))
		.await
		.unwrap()
		.json()
		.await
		.unwrap();

	let hello = snapshot["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == "ReplicatedStorage")
		.unwrap()["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == "Hello")
		.unwrap()["id"]
		.as_str()
		.unwrap()
		.to_owned();

	// Disk edit propagates...
	fs::write(daemon.src_dir().join("Hello.luau"), "return \"disk-edit\"\n").unwrap();

	wait_for_frame(&mut plugin, Duration::from_secs(15), |frame| {
		frame["type"] == "sync" && !frame["updates"].as_array().unwrap().is_empty()
	})
	.await
	.expect("no sync frame after the disk edit");

	// ...and a racing Studio push simply wins (today's behavior, kept
	// verbatim when the engine is off)
	let push = json!({
		"type": "push",
		"ops": {
			"additions": [],
			"updates": [{ "id": hello, "properties": { "Source": { "String": "return \"studio-wins\"" } } }],
			"removals": [],
		},
	});

	plugin.send(Message::Text(push.to_string())).await.unwrap();

	let result = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "push-result"
	})
	.await
	.expect("no push-result frame");

	assert_eq!(result["ok"], true);
	assert_eq!(result["applied"], 1);
	assert_eq!(result["conflicts"], 0);

	// The push landed on disk (LWW) and nothing was parked
	let deadline = std::time::Instant::now() + Duration::from_secs(5);

	loop {
		let content = fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap();

		if content == "return \"studio-wins\"" {
			break;
		}

		assert!(std::time::Instant::now() < deadline, "push never landed: {content:?}");
		std::thread::sleep(Duration::from_millis(50));
	}

	let listed: Value = reqwest::get(daemon.http("/resolve"))
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	assert_eq!(listed["conflicts"], json!([]));
}
