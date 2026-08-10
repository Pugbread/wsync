//! Integration coverage for `wsync lint` and `wsync repair`.
//!
//! The lint orchestration (argument building, sourcemap generation, the
//! definitions cache, scope filtering, the compile pass, and the toolchain
//! resolution chain) is exercised against stub `luau-lsp`/`luau-compile`
//! shell scripts that record their argv and emit canned diagnostics — so the
//! tests are deterministic on machines without the real toolchain. One
//! additional test runs the real `luau-lsp` against a seeded type error and
//! is skipped (with a note) when no real binary is resolvable.
//!
//! `repair sourcemap` runs against a scratch project on disk; `repair tree`
//! runs against a real daemon with a fake plugin.

mod common;

use serde_json::{json, Value};
use std::{env, fs, path::PathBuf, sync::Arc};

use common::{cli_json, cli_stderr, spawn_cli_plugin, start_daemon, CliAnswer, CliSandbox};

/// A scratch project directory with one mapped source dir
fn lint_project(sandbox: &CliSandbox, files: &[(&str, &str)]) -> PathBuf {
	let dir = sandbox.work.path().join("project");

	for (path, contents) in files {
		let path = dir.join(path);

		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, contents).unwrap();
	}

	fs::write(
		dir.join("default.project.json"),
		serde_json::to_string_pretty(&json!({
			"name": "lint-fixture",
			"tree": { "$className": "DataModel", "ReplicatedStorage": { "$path": "src" } },
			"gameId": 5550001,
		}))
		.unwrap(),
	)
	.unwrap();

	dir
}

/// Seeds a fresh definitions cache in the sandbox state dir, so no lint run
/// ever touches the network
fn seed_definitions(sandbox: &CliSandbox) -> PathBuf {
	let cache = sandbox.state.path().join("lint").join("globalTypes.d.luau");

	fs::create_dir_all(cache.parent().unwrap()).unwrap();
	fs::write(&cache, "declare _WSYNC_TEST_DEFS: number\n").unwrap();

	cache
}

/// Writes an executable stub script (unix only — the suite's stub tests are
/// gated on that)
#[cfg(unix)]
fn write_stub(path: &PathBuf, body: &str) {
	use std::os::unix::fs::PermissionsExt;

	fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
	fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A luau-lsp stub that records argv, copies the generated sourcemap, and
/// emits the given stderr diagnostics with the given exit code
#[cfg(unix)]
fn analyzer_stub(sandbox: &CliSandbox, name: &str, stderr: &str, exit: i32) -> (PathBuf, PathBuf, PathBuf) {
	let stub = sandbox.work.path().join(name);
	let argv_file = sandbox.work.path().join(format!("{name}-argv.txt"));
	let sourcemap_copy = sandbox.work.path().join(format!("{name}-sourcemap.json"));
	let diagnostics_file = sandbox.work.path().join(format!("{name}-diagnostics.txt"));

	fs::write(&diagnostics_file, stderr).unwrap();

	write_stub(
		&stub,
		&format!(
			"printf '%s\\n' \"$@\" > {argv}\n\
			 for a in \"$@\"; do case \"$a\" in --sourcemap=*) cp \"${{a#--sourcemap=}}\" {copy};; esac; done\n\
			 cat {diagnostics} >&2\n\
			 exit {exit}\n",
			argv = argv_file.display(),
			copy = sourcemap_copy.display(),
			diagnostics = diagnostics_file.display(),
			exit = exit,
		),
	);

	(stub, argv_file, sourcemap_copy)
}

fn read_argv(argv_file: &PathBuf) -> Vec<String> {
	fs::read_to_string(argv_file)
		.unwrap_or_else(|_| panic!("the stub never recorded argv at {}", argv_file.display()))
		.lines()
		.map(str::to_owned)
		.collect()
}

#[cfg(unix)]
#[test]
fn lint_orchestrates_the_analyzer_with_sourcemap_definitions_and_vendor_ignores() {
	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Hello.luau", "return 1\n")]);
	let cache = seed_definitions(&sandbox);

	let (stub, argv_file, sourcemap_copy) = analyzer_stub(
		&sandbox,
		"luau-lsp-stub",
		"[INFO] Loading definitions file: @roblox - defs\n\
		 src/Hello.luau(1,1): TypeError: boom\n\
		 src/Hello.luau(2,7): LocalUnused: Variable 'unused' is never used\n",
		1,
	);

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project.to_string_lossy(),
		"--luau-lsp",
		&stub.to_string_lossy(),
		"--compile",
		"off",
		"--raw",
	]);

	// One error-level diagnostic → non-zero exit, after the raw record
	assert!(!output.status.success(), "a TypeError must fail the lint");

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], false);
	assert_eq!(raw["errors"], 1);
	assert_eq!(raw["warnings"], 1);

	let diagnostics = raw["analyzer"]["diagnostics"].as_array().unwrap();

	assert_eq!(diagnostics.len(), 2);
	assert_eq!(diagnostics[0]["tag"], "TypeError");
	assert_eq!(diagnostics[0]["severity"], "error");
	assert_eq!(diagnostics[0]["file"], "src/Hello.luau");
	assert_eq!(diagnostics[0]["line"], 1);
	assert_eq!(diagnostics[1]["tag"], "LocalUnused");
	assert_eq!(diagnostics[1]["severity"], "warning");

	// The [INFO] log line is neither a diagnostic nor unparsed noise
	assert!(raw["analyzer"]["unparsed"].as_array().unwrap().is_empty());

	// auto mode without a daemon: relaxed filesystem fallback, reported
	assert_eq!(raw["dataModel"]["requested"], "auto");
	assert_eq!(raw["dataModel"]["effective"], "filesystem-relaxed");
	assert_eq!(raw["dataModel"]["strictness"], "gradual");
	assert_eq!(raw["dataModel"]["live"], false);
	assert_eq!(raw["dataModel"]["sourcemap"], "generated");
	assert!(raw["dataModel"]["fallbackReason"].is_string());

	// The definitions came from the seeded cache, injected as @roblox
	assert_eq!(raw["analyzer"]["definitions"]["set"], "@roblox");
	assert_eq!(
		raw["analyzer"]["definitions"]["path"],
		cache.to_string_lossy().into_owned()
	);

	// The recorded argv: analyze, the generated sourcemap, the @roblox set,
	// the gradual-mode flag, vendor ignores, and the default `.` target
	let argv = read_argv(&argv_file);

	assert_eq!(argv[0], "analyze");
	assert!(argv.iter().any(|arg| arg.starts_with("--sourcemap=")));
	assert!(argv
		.iter()
		.any(|arg| *arg == format!("--definitions=@roblox={}", cache.to_string_lossy())));
	assert!(argv.iter().any(|arg| arg == "--no-strict-dm-types"));
	assert!(argv.iter().any(|arg| arg == "--ignore=**/Packages/**"));
	assert!(argv.iter().any(|arg| arg == "--ignore=**/_Index/**"));
	assert!(argv.iter().any(|arg| arg == "--ignore=**/.wsync-*/**"));
	assert_eq!(argv.last().map(String::as_str), Some("."));

	// The temporary sourcemap really described the project (and was handed
	// to the analyzer before being cleaned up)
	let sourcemap: Value = serde_json::from_str(&fs::read_to_string(&sourcemap_copy).unwrap()).unwrap();

	assert_eq!(sourcemap["className"], "DataModel");

	let replicated = sourcemap["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == "ReplicatedStorage")
		.expect("the sourcemap lost ReplicatedStorage");
	let hello = replicated["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == "Hello")
		.expect("the sourcemap lost the script node");

	assert_eq!(hello["className"], "ModuleScript");
	assert!(hello["filePaths"]
		.as_array()
		.unwrap()
		.iter()
		.any(|path| path.as_str().is_some_and(|path| path.contains("Hello.luau"))));

	// The temp sourcemap was removed after the run
	let leftovers: Vec<_> = fs::read_dir(sandbox.state.path().join("lint"))
		.unwrap()
		.filter_map(Result::ok)
		.filter(|entry| entry.file_name().to_string_lossy().starts_with("sourcemap-"))
		.collect();

	assert!(leftovers.is_empty(), "temp sourcemaps left behind: {leftovers:?}");
}

#[cfg(unix)]
#[test]
fn lint_rejects_plain_formatter_sourcemap_conflicts_and_scopes_diagnostics() {
	let sandbox = CliSandbox::new();
	let project = lint_project(
		&sandbox,
		&[("src/Hello.luau", "return 1\n"), ("other/Bad.luau", "return 2\n")],
	);

	seed_definitions(&sandbox);

	let (stub, argv_file, _) = analyzer_stub(
		&sandbox,
		"luau-lsp-stub",
		"other/Bad.luau(1,1): TypeError: dependency broken\n\
		 src/Hello.luau(2,7): LocalUnused: in scope\n",
		1,
	);

	let project_arg = project.to_string_lossy().into_owned();
	let stub_arg = stub.to_string_lossy().into_owned();

	// --formatter=plain is refused with the registry's reason, before the
	// analyzer runs
	for spelling in [vec!["--formatter=plain"], vec!["--formatter", "plain"]] {
		let mut args = vec!["lint", "--project", &project_arg, "--luau-lsp", &stub_arg, "--"];

		args.extend(spelling);

		let output = sandbox.run(&args);

		assert!(!output.status.success(), "--formatter=plain must be rejected");
		assert!(
			cli_stderr(&output).contains("plain") && cli_stderr(&output).contains("successful process status"),
			"the refusal must carry the reason: {}",
			cli_stderr(&output)
		);
		assert!(
			!argv_file.exists(),
			"the analyzer must not run under a rejected formatter"
		);
	}

	// studio/filesystem modes cannot lose the generated sourcemap
	let output = sandbox.run(&[
		"lint",
		"--project",
		&project_arg,
		"--data-model",
		"studio",
		"--no-sourcemap",
	]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("sourcemap"));

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project_arg,
		"--data-model",
		"filesystem",
		"--",
		"--sourcemap=own.json",
	]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("sourcemap"));

	// --scope-only needs a scope
	let output = sandbox.run(&["lint", "--project", &project_arg, "--scope-only"]);

	assert!(!output.status.success(), "--scope-only without --path must not parse");

	// Scope filtering: the out-of-scope error is suppressed and does not
	// fail the run; the in-scope warning is kept
	let src_scope = project.join("src");

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project_arg,
		"--luau-lsp",
		&stub_arg,
		"--compile",
		"off",
		"--path",
		&src_scope.to_string_lossy(),
		"--scope-only",
		"--raw",
	]);

	assert!(
		output.status.success(),
		"out-of-scope errors must not fail --scope-only: {}",
		cli_stderr(&output)
	);

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], true);
	assert_eq!(raw["errors"], 0);
	assert_eq!(raw["warnings"], 1);
	assert_eq!(raw["analyzer"]["diagnostics"].as_array().unwrap().len(), 1);
	assert_eq!(raw["analyzer"]["diagnostics"][0]["file"], "src/Hello.luau");
	assert_eq!(raw["analyzer"]["suppressed"]["count"], 1);
	assert_eq!(raw["analyzer"]["suppressed"]["errors"], 1);
	assert_eq!(raw["scope"]["mode"], "scope-only");

	// An explicit --path into a vendor folder drops that folder's default
	// ignore (the requested target is never silently skipped)
	fs::create_dir_all(project.join("Packages")).unwrap();
	fs::write(project.join("Packages").join("Dep.luau"), "return 3\n").unwrap();

	let vendor_scope = project.join("Packages");

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project_arg,
		"--luau-lsp",
		&stub_arg,
		"--compile",
		"off",
		"--path",
		&vendor_scope.to_string_lossy(),
		"--raw",
	]);

	// The stub still reports the canned TypeError, so the exit is non-zero;
	// the argv is what matters here
	assert!(!output.status.success());

	let argv = read_argv(&argv_file);

	assert!(
		!argv.iter().any(|arg| arg.contains("**/Packages/**")),
		"--path Packages must drop the Packages vendor ignore: {argv:?}"
	);
	assert!(
		argv.iter().any(|arg| arg.contains("**/_Index/**")),
		"other vendor ignores stay active: {argv:?}"
	);
	assert!(
		argv.iter().any(|arg| arg == "Packages"),
		"the target must be passed: {argv:?}"
	);
}

#[cfg(unix)]
#[test]
fn lint_compile_pass_runs_three_levels_and_collates_failures() {
	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Hello.luau", "return 1\n")]);

	seed_definitions(&sandbox);

	let (analyzer, _, _) = analyzer_stub(&sandbox, "luau-lsp-stub", "", 0);

	// A compiler stub that fails only at -O2, appending each invocation to a
	// log
	let compiler = sandbox.work.path().join("luau-compile-stub");
	let compile_log = sandbox.work.path().join("compile-log.txt");

	write_stub(
		&compiler,
		&format!(
			"printf '%s ' \"$@\" >> {log}\nprintf '\\n' >> {log}\n\
			 for a in \"$@\"; do\n\
			 \tif [ \"$a\" = \"-O2\" ]; then\n\
			 \t\tprintf './src/Hello.luau(3,1): CompileError: out of registers\\n' >&2\n\
			 \t\texit 1\n\
			 \tfi\n\
			 done\n\
			 exit 0\n",
			log = compile_log.display(),
		),
	);

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project.to_string_lossy(),
		"--luau-lsp",
		&analyzer.to_string_lossy(),
		"--luau-compile",
		&compiler.to_string_lossy(),
		"--compile",
		"required",
		"--raw",
	]);

	assert!(!output.status.success(), "a compile failure must fail the lint");

	// Three invocations: -O0, -O1, -O2, each in --null mode over the script
	let log = fs::read_to_string(&compile_log).unwrap();
	let invocations: Vec<&str> = log.lines().collect();

	assert_eq!(invocations.len(), 3, "one invocation per optimization level: {log}");

	for (index, level) in ["-O0", "-O1", "-O2"].iter().enumerate() {
		assert!(
			invocations[index].contains(level),
			"invocation {index} must be {level}: {log}"
		);
		assert!(invocations[index].contains("--null"));
		assert!(invocations[index].contains("src/Hello.luau"));
	}

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], false);
	assert_eq!(raw["compiler"]["status"], "completed");
	assert_eq!(raw["compiler"]["files"], 1);

	let diagnostics = raw["compiler"]["diagnostics"].as_array().unwrap();

	assert_eq!(diagnostics.len(), 1, "the -O2-only failure is one collated record");
	assert_eq!(diagnostics[0]["tag"], "CompileError");
	assert_eq!(diagnostics[0]["severity"], "error");
	assert_eq!(diagnostics[0]["source"], "compiler");
	assert_eq!(diagnostics[0]["file"], "src/Hello.luau");
	assert_eq!(diagnostics[0]["levels"], json!(["-O2"]));

	// --compile off never invokes the compiler
	fs::remove_file(&compile_log).ok();

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project.to_string_lossy(),
		"--luau-lsp",
		&analyzer.to_string_lossy(),
		"--compile",
		"off",
		"--raw",
	]);

	assert!(
		output.status.success(),
		"clean lint with compile off: {}",
		cli_stderr(&output)
	);
	assert!(!compile_log.exists(), "--compile off must not run the compiler");
	assert_eq!(cli_json(&output)["compiler"]["status"], "skipped");

	// --compile required with nothing resolvable is a failed stage
	let empty_bin = sandbox.work.path().join("empty-bin");

	fs::create_dir_all(&empty_bin).unwrap();

	let output = sandbox.run_with_envs(
		&[
			"lint",
			"--project",
			&project.to_string_lossy(),
			"--luau-lsp",
			&analyzer.to_string_lossy(),
			"--compile",
			"required",
		],
		&[("PATH", &empty_bin.to_string_lossy())],
	);

	assert!(!output.status.success());
	assert!(
		cli_stderr(&output).contains("luau-compile"),
		"the failure must name the missing tool: {}",
		cli_stderr(&output)
	);
}

#[cfg(unix)]
#[test]
fn lint_resolves_the_toolchain_through_the_documented_chain() {
	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Hello.luau", "return 1\n")]);

	seed_definitions(&sandbox);

	let empty_bin = sandbox.work.path().join("empty-bin");

	fs::create_dir_all(&empty_bin).unwrap();

	// PATH-only stubs avoid external commands: with a stripped PATH, /bin/cp
	// would not resolve inside the stub
	let quiet = sandbox.work.path().join("quiet-lsp");

	write_stub(&quiet, "exit 0\n");

	let project_arg = project.to_string_lossy().into_owned();
	let empty_path = empty_bin.to_string_lossy().into_owned();

	// Nothing resolvable → the install-instruction error
	let output = sandbox.run_with_envs(
		&["lint", "--project", &project_arg, "--compile", "off"],
		&[("PATH", empty_path.as_str())],
	);

	assert!(!output.status.success());

	let message = cli_stderr(&output);

	assert!(
		message.contains("luau-lsp") && message.contains("WSYNC_LUAU_LSP") && message.contains("rokit"),
		"the error must document the resolution chain and an install path: {message}"
	);

	// WSYNC_LUAU_LSP env resolves ahead of PATH
	let output = sandbox.run_with_envs(
		&["lint", "--project", &project_arg, "--compile", "off"],
		&[
			("PATH", empty_path.as_str()),
			("WSYNC_LUAU_LSP", &quiet.to_string_lossy()),
		],
	);

	assert!(
		output.status.success(),
		"WSYNC_LUAU_LSP must resolve the analyzer: {}",
		cli_stderr(&output)
	);

	// A luau-lsp on PATH resolves too
	let path_bin = sandbox.work.path().join("path-bin");

	fs::create_dir_all(&path_bin).unwrap();
	fs::copy(&quiet, path_bin.join("luau-lsp")).unwrap();

	{
		use std::os::unix::fs::PermissionsExt;

		fs::set_permissions(path_bin.join("luau-lsp"), fs::Permissions::from_mode(0o755)).unwrap();
	}

	let output = sandbox.run_with_envs(
		&["lint", "--project", &project_arg, "--compile", "off"],
		&[("PATH", &path_bin.to_string_lossy())],
	);

	assert!(
		output.status.success(),
		"a PATH luau-lsp must resolve: {}",
		cli_stderr(&output)
	);
}

#[cfg(unix)]
#[test]
fn lint_definitions_cache_is_required_offline_and_falls_back_when_stale() {
	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Hello.luau", "return 1\n")]);
	let (stub, _, _) = analyzer_stub(&sandbox, "luau-lsp-stub", "", 0);

	// Port 9 answers nothing: the download fails immediately, offline-style
	let dead_url = "http://127.0.0.1:9/globalTypes.d.luau";
	let cache = sandbox.state.path().join("lint").join("globalTypes.d.luau");

	// No cache + no network → a hard error naming the cache path
	let output = sandbox.run_with_envs(
		&[
			"lint",
			"--project",
			&project.to_string_lossy(),
			"--luau-lsp",
			&stub.to_string_lossy(),
			"--compile",
			"off",
		],
		&[("WSYNC_GLOBAL_TYPES_URL", dead_url)],
	);

	assert!(!output.status.success(), "offline with no cache must fail");

	let message = cli_stderr(&output);

	assert!(
		message.contains(&cache.to_string_lossy().into_owned()),
		"the error must name the cache path: {message}"
	);
	assert!(
		message.contains("--definitions"),
		"the error must offer the override: {message}"
	);

	// A stale cache is used as the offline fallback (with a warning)
	seed_definitions(&sandbox);

	let dated = std::process::Command::new("touch")
		.args(["-t", "202001010000", &cache.to_string_lossy()])
		.status()
		.map(|status| status.success())
		.unwrap_or(false);

	if !dated {
		eprintln!("skipping the stale-cache half: `touch -t` unavailable");

		return;
	}

	let output = sandbox.run_with_envs(
		&[
			"lint",
			"--project",
			&project.to_string_lossy(),
			"--luau-lsp",
			&stub.to_string_lossy(),
			"--compile",
			"off",
		],
		&[("WSYNC_GLOBAL_TYPES_URL", dead_url)],
	);

	assert!(
		output.status.success(),
		"a stale cache must carry an offline run: {}",
		cli_stderr(&output)
	);
	assert!(
		cli_stderr(&output).contains("cached copy"),
		"the stale fallback must be reported: {}",
		cli_stderr(&output)
	);
}

/// The real-binary test: runs only when a real luau-lsp is resolvable on
/// this machine's PATH
#[test]
fn lint_finds_a_seeded_type_error_with_a_real_luau_lsp() {
	let Some(real) = env::var_os("PATH").and_then(|paths| {
		env::split_paths(&paths)
			.map(|dir| dir.join("luau-lsp"))
			.find(|path| path.is_file())
	}) else {
		eprintln!("skipping: no real luau-lsp on PATH — the stub tests carry this suite");

		return;
	};

	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Bad.luau", "local x: number = \"nope\"\nreturn x\n")]);

	seed_definitions(&sandbox);

	let output = sandbox.run(&[
		"lint",
		"--project",
		&project.to_string_lossy(),
		"--luau-lsp",
		&real.to_string_lossy(),
		"--compile",
		"off",
		"--data-model",
		"loose",
		"--raw",
	]);

	assert!(!output.status.success(), "the seeded type error must fail the lint");

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], false);
	assert!(raw["errors"].as_u64().unwrap() >= 1);

	let diagnostics = raw["analyzer"]["diagnostics"].as_array().unwrap();

	assert!(
		diagnostics.iter().any(|diagnostic| {
			diagnostic["tag"] == "TypeError"
				&& diagnostic["file"]
					.as_str()
					.is_some_and(|file| file.contains("Bad.luau"))
		}),
		"the real analyzer must report the seeded TypeError: {diagnostics:?}"
	);
}

// ---------------------------------------------------------------------------
// repair
// ---------------------------------------------------------------------------

#[test]
fn repair_sourcemap_rebuilds_the_file_from_disk() {
	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Hello.luau", "return 1\n")]);

	// Default output location: sourcemap.json next to the project file
	let output = sandbox.run(&["repair", "sourcemap", "--project", &project.to_string_lossy(), "--raw"]);

	assert!(
		output.status.success(),
		"repair sourcemap failed: {}",
		cli_stderr(&output)
	);

	let raw = cli_json(&output);
	let default_path = project.join("sourcemap.json");

	assert_eq!(raw["ok"], true);
	assert_eq!(raw["path"], default_path.to_string_lossy().into_owned());
	assert!(raw["bytes"].as_u64().unwrap() > 0);

	let sourcemap: Value = serde_json::from_str(&fs::read_to_string(&default_path).unwrap()).unwrap();

	assert_eq!(sourcemap["className"], "DataModel");
	assert!(
		sourcemap["children"]
			.as_array()
			.unwrap()
			.iter()
			.any(|child| child["name"] == "ReplicatedStorage"),
		"the rebuilt sourcemap must carry the project tree"
	);

	// --output overrides, creating parents
	let custom = project.join("generated").join("sm.json");
	let output = sandbox.run(&[
		"repair",
		"sourcemap",
		"--project",
		&project.to_string_lossy(),
		"--output",
		&custom.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "custom output failed: {}", cli_stderr(&output));
	assert!(custom.is_file());
}

#[test]
fn repair_tree_reports_every_check_against_a_live_daemon() {
	let daemon = start_daemon(None);

	// A plugin that answers the handshake ping
	let _journal = spawn_cli_plugin(
		&daemon,
		"repair-plugin",
		Arc::new(|op, _args| match op {
			"ping" => CliAnswer::Value(json!({ "pong": true })),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();

	let output = sandbox.run(&[
		"repair",
		"tree",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(output.status.success(), "repair tree failed: {}", cli_stderr(&output));

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], true);

	let checks = raw["checks"].as_array().unwrap();
	let status_of = |id: &str| -> &str {
		checks
			.iter()
			.find(|check| check["id"] == id)
			.and_then(|check| check["status"].as_str())
			.unwrap_or_else(|| panic!("missing check {id}: {checks:?}"))
	};

	for id in ["project", "daemon", "plugin", "snapshot", "path-index"] {
		assert_eq!(status_of(id), "pass", "check {id} must pass: {checks:?}");
	}

	// The snapshot check really walked the projected tree
	let snapshot_detail = checks.iter().find(|check| check["id"] == "snapshot").unwrap()["detail"]
		.as_str()
		.unwrap()
		.to_owned();

	assert!(
		snapshot_detail.contains("node"),
		"the snapshot check reports its walk: {snapshot_detail}"
	);
}

#[test]
fn repair_tree_fails_and_skips_downstream_checks_without_a_daemon() {
	let sandbox = CliSandbox::new();
	let project = lint_project(&sandbox, &[("src/Hello.luau", "return 1\n")]);

	// Port 1 answers nothing
	let output = sandbox.run(&[
		"repair",
		"tree",
		"--project",
		&project.to_string_lossy(),
		"--port",
		"1",
		"--raw",
	]);

	assert!(!output.status.success(), "an unreachable daemon must fail repair tree");

	let raw = cli_json(&output);

	assert_eq!(raw["ok"], false);

	let checks = raw["checks"].as_array().unwrap();
	let status_of = |id: &str| -> &str {
		checks
			.iter()
			.find(|check| check["id"] == id)
			.and_then(|check| check["status"].as_str())
			.unwrap()
	};

	assert_eq!(status_of("project"), "pass");
	assert_eq!(status_of("daemon"), "fail");
	assert_eq!(status_of("plugin"), "skip");
	assert_eq!(status_of("path-index"), "skip");
}
