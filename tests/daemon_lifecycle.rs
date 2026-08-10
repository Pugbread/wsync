//! Engine-side lifecycle contracts the desktop app and Studio plugin rely on
//! (Design §3.2): daemons bind the IPv4 loopback that every desktop probe
//! targets, and a portless `daemon start` scans the configured port range
//! instead of hard-erroring when the default port already serves another
//! project.

mod common;

use serde_json::{json, Value};
use std::{
	fs,
	io::{BufRead, BufReader, Read},
	net::TcpStream,
	path::{Path, PathBuf},
	process::Child,
	sync::mpsc,
	thread,
	time::Duration,
};

use common::{cli_stderr, start_daemon, CliSandbox};

/// The desktop's loopback client (`desktop/src-tauri/src/loopback.rs`) probes
/// `127.0.0.1` only, while `localhost` resolves to `::1` first on macOS —
/// binding the first resolved address left every desktop probe, heartbeat and
/// plugin port scan blind to the daemon (the wave-5 desktop regression)
#[tokio::test]
async fn server_binds_the_ipv4_loopback() {
	let daemon = start_daemon(None);

	// Raw IPv4 connect: refused outright if the listener sits on ::1 only
	TcpStream::connect(("127.0.0.1", daemon.port)).expect("the listener must accept on 127.0.0.1");

	// The desktop-style probe: `GET /hello` on an explicit 127.0.0.1 URL,
	// no hostname resolution involved
	let hello: Value = reqwest::get(format!("http://127.0.0.1:{}/hello", daemon.port))
		.await
		.expect("GET /hello over 127.0.0.1 must connect")
		.json()
		.await
		.expect("/hello must answer with the hello document");

	assert_eq!(hello["bootId"], daemon.boot_id.as_str());
	assert_eq!(hello["port"], daemon.port);
}

/// A `daemon start` child that is killed when the test ends (or panics), so
/// failed runs never leak daemons onto the scan range
struct DaemonProcess {
	child: Child,
}

impl Drop for DaemonProcess {
	fn drop(&mut self) {
		self.child.kill().ok();
		self.child.wait().ok();
	}
}

/// First line of the child's stdout (the `--raw` readiness report), read with
/// a timeout; the reader thread keeps draining afterwards so the daemon never
/// blocks on a full pipe
fn first_stdout_line(child: &mut Child, timeout: Duration) -> Option<String> {
	let stdout = child.stdout.take().expect("stdout was piped");
	let (line_tx, line_rx) = mpsc::channel();

	thread::spawn(move || {
		let mut reader = BufReader::new(stdout);
		let mut line = String::new();

		reader.read_line(&mut line).ok();
		line_tx.send(line).ok();

		let mut rest = String::new();

		loop {
			rest.clear();

			match reader.read_line(&mut rest) {
				Ok(0) | Err(_) => break,
				Ok(_) => {}
			}
		}
	});

	line_rx.recv_timeout(timeout).ok()
}

/// Writes a minimal place project (mirrors `common::scratch_project`, but
/// named — the scan test needs two distinct canonical projects)
fn place_project(base: &Path, name: &str, game_id: u64) -> PathBuf {
	let root = base.join(name);

	fs::create_dir_all(root.join("src")).unwrap();
	fs::write(root.join("src").join("Hello.luau"), "return \"hello\"\n").unwrap();

	let project = json!({
		"name": name,
		"tree": {
			"$className": "DataModel",
			"ReplicatedStorage": { "$path": "src" },
		},
		"gameId": game_id,
		"placeIds": [game_id],
	});

	fs::write(
		root.join("default.project.json"),
		serde_json::to_string_pretty(&project).unwrap(),
	)
	.unwrap();

	root
}

/// Spawns `wsync daemon start --raw` for the project and waits for its
/// readiness line; panics with the child's stderr when the daemon dies instead
fn start_daemon_process(sandbox: &CliSandbox, project: &Path) -> (DaemonProcess, Value) {
	let mut daemon = DaemonProcess {
		child: sandbox.spawn(&[
			"daemon",
			"start",
			"--project",
			&project.to_string_lossy(),
			"--managed-by",
			"test",
			"--raw",
		]),
	};

	let line =
		first_stdout_line(&mut daemon.child, Duration::from_secs(20)).expect("daemon start never reported readiness");

	if line.trim().is_empty() {
		let mut stderr = String::new();

		if let Some(mut pipe) = daemon.child.stderr.take() {
			pipe.read_to_string(&mut stderr).ok();
		}

		panic!("daemon start exited without a readiness line; stderr:\n{stderr}");
	}

	let report: Value =
		serde_json::from_str(line.trim()).unwrap_or_else(|err| panic!("readiness line is not JSON ({err}): {line}"));

	assert_eq!(report["ok"], true, "daemon start must report ok: {report}");

	(daemon, report)
}

fn hello_on(port: u16) -> Value {
	reqwest::blocking::Client::builder()
		.timeout(Duration::from_secs(2))
		.build()
		.unwrap()
		.get(format!("http://127.0.0.1:{port}/hello"))
		.send()
		.unwrap_or_else(|err| panic!("GET /hello on 127.0.0.1:{port} must connect: {err}"))
		.json()
		.expect("/hello must answer with the hello document")
}

/// Design §3.2: default port 7978, scan range 7978–7990. A second project's
/// portless `daemon start` must scan past the occupied default instead of
/// hard-erroring — the exact flow behind the desktop broker's auto-serve,
/// where the hard error meant only one project could ever be served.
/// The range is remapped to 17978–17990 through the sandbox's global config
/// so real daemons on this machine can never collide with the test
#[test]
fn portless_daemon_start_scans_the_port_range() {
	let sandbox = CliSandbox::new();

	fs::write(
		sandbox.home.path().join(".wsync").join("config.toml"),
		"check_updates = false\ninstall_plugin = false\nport = 17978\nport_scan_max = 17990\n",
	)
	.unwrap();

	let project_a = place_project(sandbox.work.path(), "fixture-a", 5550001);
	let project_b = place_project(sandbox.work.path(), "fixture-b", 5550002);
	let project_c = place_project(sandbox.work.path(), "fixture-c", 5550003);

	// The first portless start takes the default port
	let (_daemon_a, report_a) = start_daemon_process(&sandbox, &project_a);
	assert_eq!(report_a["port"], 17978, "the first daemon must take the default port");

	// The second portless start scans to the next free port
	let (_daemon_b, report_b) = start_daemon_process(&sandbox, &project_b);
	assert_eq!(
		report_b["port"], 17979,
		"a portless start must scan past a default port serving another project"
	);

	// Both daemons answer the desktop-style IPv4 probe with their own project
	for (port, project) in [(17978u16, &project_a), (17979, &project_b)] {
		let hello = hello_on(port);

		assert_eq!(
			hello["canonicalProject"],
			project.join("default.project.json").to_string_lossy().into_owned(),
			"the daemon on port {port} must serve its own project"
		);
	}

	// An explicitly pinned port stays a hard error — only portless starts scan
	let output = sandbox.run(&[
		"daemon",
		"start",
		"--project",
		&project_c.to_string_lossy(),
		"--port",
		"17978",
		"--managed-by",
		"test",
		"--raw",
	]);

	assert!(
		!output.status.success(),
		"a pinned port serving another project must remain a hard error"
	);
	assert!(
		cli_stderr(&output).contains("already serving a different project"),
		"the pinned-port error must name the conflict; stderr:\n{}",
		cli_stderr(&output)
	);
}
