//! `lint` — `luau-lsp analyze` orchestration with named Roblox definitions,
//! selectable DataModel typing, and an optional `luau-compile` pass
//! (lint.json).
//!
//! WSync owns the whole setup so one command is a complete Luau audit:
//!
//! * **Definitions** — the current `globalTypes.d.luau` is downloaded and
//!   cached in the state directory (offline runs fall back to the cache; no
//!   cache is a hard error naming the cache path), then injected as the
//!   named `@roblox` definitions set. An explicit `--definitions:@roblox=…`
//!   after `--` replaces the bundled set and skips the download.
//! * **Sourcemap** — a temporary sourcemap is generated in-process from the
//!   project (the engine's own machinery; no daemon involved) so luau-lsp
//!   understands the DataModel. `--data-model auto` additionally merges the
//!   complete live Studio tree into it and keeps strict DataModel types
//!   when the project's daemon and plugin answer; otherwise it reports a
//!   relaxed filesystem fallback. `studio` requires the live strict check,
//!   `filesystem` is a strict offline audit (which may flag Studio-only
//!   children), `loose` keeps DataModel typing gradual.
//! * **Compile** — `--compile auto` runs every in-scope script through
//!   `luau-compile --null` at `-O0`, `-O1`, and `-O2` when the compiler
//!   resolves, catching bytecode-generation failures (the 200-local-register
//!   limit and friends) that no type checker reports.
//!
//! Diagnostics are parsed from luau-lsp's default and GNU formatters;
//! `--formatter=plain` is rejected because luau-lsp can emit TypeErrors in
//! that mode while exiting zero. The command exits non-zero on any
//! error-level diagnostic in scope or any failed required stage.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use serde_json::{json, Map, Value};
use std::{
	env, fs,
	path::{Path, PathBuf},
	process::Command,
	time::Duration,
};

use crate::{
	cli::client::{print_json, Client, Target, Targeting},
	config::Config,
	core::Core,
	daemon,
	ext::PathExt,
	project::{self, Project},
	wsync_info, wsync_warn,
};

/// Where the current Roblox definitions come from unless overridden
const GLOBAL_TYPES_URL: &str =
	"https://raw.githubusercontent.com/JohnnyMorganz/luau-lsp/main/scripts/globalTypes.d.luau";

/// Environment override for the definitions download (tests point this at a
/// stub or an unreachable port)
const GLOBAL_TYPES_URL_ENV: &str = "WSYNC_GLOBAL_TYPES_URL";

/// A cached definitions file younger than this is used without any network
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long the definitions download may take before the offline fallback
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the live-tree fetch for the DataModel merge may take
const LIVE_TREE_TIMEOUT_MS: u64 = 30_000;

/// Default vendor ignores (lint.json): dependency and tooling folders hidden
/// unless `--no-vendor-ignores` or an explicit `--path` targets them.
/// `Madwork*` and `.wsync-*` are prefix patterns; the rest match exactly
const VENDOR_PATTERNS: [&str; 10] = [
	"Packages",
	"_Index",
	"Madwork*",
	"PlayerModule",
	"node_modules",
	"tools",
	".git",
	".codex",
	".vscode",
	".wsync-*",
];

/// The three compile passes of `--compile auto|required`
const COMPILE_LEVELS: [&str; 3] = ["-O0", "-O1", "-O2"];

/// Files per `luau-compile` invocation, so huge trees stay under argv limits
const COMPILE_BATCH: usize = 100;

/// Run `luau-lsp analyze` with Roblox definitions, DataModel coverage, and
/// an optional `luau-compile` pass
#[derive(Parser)]
pub struct Lint {
	#[command(flatten)]
	targeting: Targeting,

	/// File or directory to lint (repeatable; default: the whole workspace
	/// minus vendor ignores)
	#[arg(long = "path", value_name = "FILE-OR-DIR")]
	paths: Vec<PathBuf>,

	/// DataModel typing: auto merges the live Studio tree when available,
	/// studio requires it, filesystem is strict disk-only, loose is gradual
	#[arg(long = "data-model", value_enum, value_name = "MODE", default_value = "auto")]
	data_model: DataModelMode,

	/// Bytecode pass: auto runs when luau-compile resolves, required fails
	/// without it, off disables it
	#[arg(long, value_enum, value_name = "MODE", default_value = "auto")]
	compile: CompileMode,

	/// Explicit luau-lsp binary (before WSYNC_LUAU_LSP, PATH, and the
	/// Rokit/Aftman bins)
	#[arg(long = "luau-lsp", value_name = "PATH")]
	luau_lsp: Option<PathBuf>,

	/// Explicit luau-compile binary (before WSYNC_LUAU_COMPILE,
	/// LUAU_COMPILE, PATH, and the Rokit/Aftman bins)
	#[arg(long = "luau-compile", value_name = "PATH")]
	luau_compile: Option<PathBuf>,

	/// Extra ignore glob, workspace-relative (repeatable; passed to
	/// luau-lsp and applied to the compile pass)
	#[arg(long, value_name = "GLOB")]
	ignore: Vec<String>,

	/// Show only diagnostics inside the --path scopes; out-of-scope
	/// diagnostics are counted as suppressed and never fail the run
	#[arg(long = "scope-only", requires = "paths", conflicts_with = "owned_only")]
	scope_only: bool,

	/// Like --scope-only, additionally suppressing diagnostics in vendor
	/// folders inside the scopes
	#[arg(long = "owned-only", requires = "paths")]
	owned_only: bool,

	/// Print only the totals, not every diagnostic
	#[arg(long)]
	summary: bool,

	/// Print machine-readable JSON (structured diagnostics plus coverage
	/// metadata)
	#[arg(long)]
	raw: bool,

	/// Skip the generated sourcemap (disables WSync DataModel coverage)
	#[arg(long = "no-sourcemap")]
	no_sourcemap: bool,

	/// Disable the default vendor ignores
	#[arg(long = "no-vendor-ignores")]
	no_vendor_ignores: bool,

	/// Extra arguments passed to luau-lsp analyze verbatim
	#[arg(last = true, value_name = "LUAU-LSP ARGS")]
	passthrough: Vec<String>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum DataModelMode {
	Auto,
	Studio,
	Filesystem,
	Loose,
}

impl DataModelMode {
	fn as_str(self) -> &'static str {
		match self {
			DataModelMode::Auto => "auto",
			DataModelMode::Studio => "studio",
			DataModelMode::Filesystem => "filesystem",
			DataModelMode::Loose => "loose",
		}
	}
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CompileMode {
	Auto,
	Required,
	Off,
}

impl CompileMode {
	fn as_str(self) -> &'static str {
		match self {
			CompileMode::Auto => "auto",
			CompileMode::Required => "required",
			CompileMode::Off => "off",
		}
	}
}

/// What the passthrough scan learned before anything runs
struct Passthrough {
	gnu: bool,
	replaces_sourcemap: bool,
	replaces_roblox_definitions: bool,
	has_no_strict_flag: bool,
}

/// One structured diagnostic from either tool
#[derive(Clone)]
struct Diagnostic {
	file: String,
	line: u64,
	column: u64,
	tag: String,
	message: String,
	severity: &'static str,
	source: &'static str,
	/// Optimization levels a compiler diagnostic reproduced at
	levels: Vec<String>,
}

impl Diagnostic {
	fn to_json(&self) -> Value {
		let mut record = json!({
			"file": self.file,
			"line": self.line,
			"column": self.column,
			"tag": self.tag,
			"severity": self.severity,
			"message": self.message,
			"source": self.source,
		});

		if !self.levels.is_empty() {
			record["levels"] = json!(self.levels);
		}

		record
	}

	fn render(&self) -> String {
		let levels = if self.levels.is_empty() {
			String::new()
		} else {
			format!(" [luau-compile {}]", self.levels.join("/"))
		};

		format!(
			"{}({},{}): {}: {}{levels}",
			self.file, self.line, self.column, self.tag, self.message
		)
	}
}

/// The DataModel plan as it actually landed, for reporting
struct DataModelReport {
	requested: &'static str,
	effective: String,
	strictness: &'static str,
	live: bool,
	live_nodes: Option<u64>,
	live_truncated: bool,
	sourcemap: &'static str,
	merged_nodes: u64,
	fallback_reason: Option<String>,
}

impl Lint {
	pub fn main(self) -> Result<()> {
		let passthrough = scan_passthrough(&self.passthrough)?;

		if (self.no_sourcemap || passthrough.replaces_sourcemap)
			&& matches!(self.data_model, DataModelMode::Studio | DataModelMode::Filesystem)
		{
			bail!(
				"--data-model {} requires the generated sourcemap — it cannot be combined with {}",
				self.data_model.as_str(),
				if self.no_sourcemap {
					"--no-sourcemap"
				} else {
					"a --sourcemap passthrough override"
				}
			);
		}

		let project_path = project::resolve(self.targeting.project.clone().unwrap_or_default())?;

		if !project_path.exists() {
			bail!(
				"No project files found in {}",
				project_path.get_parent().to_string().bold()
			);
		}

		Config::load_workspace(project_path.get_parent());

		let workspace = project_path.get_parent().resolve()?;

		// Explicit targets are validated before any tool runs
		let mut targets: Vec<PathBuf> = Vec::new();

		for path in &self.paths {
			let target = path.resolve()?;

			if !target.exists() {
				bail!("--path {} does not exist", target.to_string().bold());
			}

			targets.push(target);
		}

		let vendor_ignores = self.active_vendor_ignores(&workspace, &targets);
		let state_dir = daemon::state_dir(None)?;

		// Sourcemap: generated in-process, then (auto/studio) merged with the
		// live tree
		let sourcemap_state = if self.no_sourcemap {
			"disabled"
		} else if passthrough.replaces_sourcemap {
			"external"
		} else {
			"generated"
		};

		let mut temp_sourcemap: Option<PathBuf> = None;
		let mut data_model = DataModelReport {
			requested: self.data_model.as_str(),
			effective: String::new(),
			strictness: "none",
			live: false,
			live_nodes: None,
			live_truncated: false,
			sourcemap: sourcemap_state,
			merged_nodes: 0,
			fallback_reason: None,
		};

		if sourcemap_state == "generated" {
			let temp = state_dir
				.join("lint")
				.join(format!("sourcemap-{}.json", std::process::id()));

			generate_sourcemap(&project_path, &temp)?;
			temp_sourcemap = Some(temp);
		}

		self.plan_data_model(&mut data_model, temp_sourcemap.as_deref(), &passthrough)?;

		// The result of the run is assembled even when it fails, so `--raw`
		// always prints one parseable record before a non-zero exit
		let outcome = self.run_tools(
			&project_path,
			&workspace,
			&targets,
			&vendor_ignores,
			temp_sourcemap.as_deref(),
			&passthrough,
			&state_dir,
			&mut data_model,
		);

		if let Some(temp) = &temp_sourcemap {
			fs::remove_file(temp).ok();
		}

		outcome
	}

	/// Settles live coverage and strictness for the requested mode. `studio`
	/// failures are hard; `auto` failures downgrade with a recorded reason
	fn plan_data_model(
		&self,
		report: &mut DataModelReport,
		temp_sourcemap: Option<&Path>,
		passthrough: &Passthrough,
	) -> Result<()> {
		match self.data_model {
			DataModelMode::Filesystem => {
				report.effective = "filesystem-strict".to_owned();
				report.strictness = "strict";

				return Ok(());
			}
			DataModelMode::Loose => {
				report.effective = if report.sourcemap == "disabled" {
					report.strictness = "none";

					"none".to_owned()
				} else {
					report.strictness = "gradual";

					"filesystem-loose".to_owned()
				};

				return Ok(());
			}
			DataModelMode::Auto | DataModelMode::Studio => {}
		}

		let required = self.data_model == DataModelMode::Studio;

		let Some(temp_sourcemap) = temp_sourcemap else {
			// auto without a generated sourcemap has nothing to merge into
			report.effective = if report.sourcemap == "disabled" {
				"none".to_owned()
			} else {
				report.strictness = "gradual";

				"external".to_owned()
			};
			report.fallback_reason = Some("no generated sourcemap to merge the live tree into".to_owned());

			return Ok(());
		};

		let live = if required {
			let client = Client::connect(&self.targeting)?;

			Some(self.fetch_live_tree(&client)?)
		} else {
			self.try_live_tree(report)
		};

		match live {
			Some(tree) => {
				let root = tree.get("root").cloned().unwrap_or(Value::Null);
				let merged = merge_live_tree(temp_sourcemap, &root)?;

				report.live = true;
				report.live_nodes = tree.get("visitedNodes").and_then(Value::as_u64);
				report.live_truncated = tree.get("truncated").and_then(Value::as_bool) == Some(true);
				report.merged_nodes = merged;
				report.effective = "studio-strict".to_owned();
				report.strictness = "strict";

				if report.live_truncated {
					wsync_warn!("The live tree was truncated by the plugin — DataModel coverage is partial");
				}
			}
			None => {
				report.effective = "filesystem-relaxed".to_owned();
				report.strictness = "gradual";

				if !self.raw {
					wsync_warn!(
						"No live Studio tree ({}) — falling back to relaxed filesystem DataModel typing",
						report.fallback_reason.as_deref().unwrap_or("daemon or plugin missing")
					);
				}
			}
		}

		// A user-supplied --no-strict-dm-types wins over the mode
		if passthrough.has_no_strict_flag {
			report.strictness = "gradual";
		}

		Ok(())
	}

	/// The live tree for `--data-model auto` — every failure is a recorded
	/// fallback, never an error
	fn try_live_tree(&self, report: &mut DataModelReport) -> Option<Value> {
		let target = match Target::resolve(&self.targeting) {
			Ok(target) => target,
			Err(err) => {
				report.fallback_reason = Some(err.to_string());

				return None;
			}
		};

		let hello = match target.probe() {
			Ok(hello) => hello,
			Err(_) => {
				report.fallback_reason = Some(format!(
					"no daemon answers on port {} (from the {})",
					target.port,
					target.port_source.as_str()
				));

				return None;
			}
		};

		// A daemon serving a different project must not leak its tree into
		// this project's sourcemap
		if let Some(canonical) = &target.canonical {
			if &hello.canonical_project != canonical {
				report.fallback_reason = Some(format!(
					"the daemon on port {} serves a different project ({})",
					target.port, hello.project
				));

				return None;
			}
		}

		let client = Client::open_probed(&target, Some(hello))?;

		match self.fetch_live_tree(&client) {
			Ok(tree) => Some(tree),
			Err(err) => {
				report.fallback_reason = Some(err.to_string());

				None
			}
		}
	}

	fn fetch_live_tree(&self, client: &Client) -> Result<Value> {
		client
			.request_with_timeout("tree", json!({ "path": "", "depth": 1000 }), LIVE_TREE_TIMEOUT_MS)?
			.into_value(false)
			.context("The Studio plugin could not serve the live tree")
	}

	/// The default vendor patterns minus any pattern an explicit `--path`
	/// target sits inside — the requested target is never silently skipped
	fn active_vendor_ignores(&self, workspace: &Path, targets: &[PathBuf]) -> Vec<&'static str> {
		if self.no_vendor_ignores {
			return Vec::new();
		}

		VENDOR_PATTERNS
			.iter()
			.copied()
			.filter(|pattern| {
				!targets.iter().any(|target| {
					target
						.strip_prefix(workspace)
						.map(|relative| {
							relative
								.components()
								.any(|component| matches_vendor(&component.as_os_str().to_string_lossy(), pattern))
						})
						.unwrap_or(false)
				})
			})
			.collect()
	}

	/// Runs the analyzer and the compiler, then reports and settles the exit
	#[allow(clippy::too_many_arguments)]
	fn run_tools(
		&self,
		project_path: &Path,
		workspace: &Path,
		targets: &[PathBuf],
		vendor_ignores: &[&'static str],
		temp_sourcemap: Option<&Path>,
		passthrough: &Passthrough,
		state_dir: &Path,
		data_model: &mut DataModelReport,
	) -> Result<()> {
		// Definitions: the named @roblox set, unless the passthrough replaces
		// it
		let definitions = if passthrough.replaces_roblox_definitions {
			None
		} else {
			Some(resolve_global_types(state_dir, self.raw)?)
		};

		let (luau_lsp, lsp_source) = resolve_tool(
			self.luau_lsp.as_deref(),
			&["WSYNC_LUAU_LSP"],
			"luau-lsp",
			"Install it with `rokit add JohnnyMorganz/luau-lsp` or `aftman add JohnnyMorganz/luau-lsp` \
			 (WSync recommends luau-lsp 1.68.1 or newer)",
		)?;

		// Analyzer argv
		let mut argv: Vec<String> = vec!["analyze".to_owned()];

		if let Some(temp) = temp_sourcemap {
			argv.push(format!("--sourcemap={}", temp.to_string()));
		}

		if let Some((path, _)) = &definitions {
			argv.push(format!("--definitions=@roblox={}", path.to_string()));
		}

		if data_model.strictness == "gradual" && data_model.sourcemap != "disabled" && !passthrough.has_no_strict_flag {
			argv.push("--no-strict-dm-types".to_owned());
		}

		for pattern in vendor_ignores {
			argv.push(format!("--ignore=**/{pattern}/**"));
			argv.push(format!("--ignore={pattern}/**"));
		}

		for glob in &self.ignore {
			argv.push(format!("--ignore={glob}"));
		}

		argv.extend(self.passthrough.iter().cloned());

		if targets.is_empty() {
			argv.push(".".to_owned());
		} else {
			for target in targets {
				argv.push(relative_to(target, workspace));
			}
		}

		let output = Command::new(&luau_lsp)
			.args(&argv)
			.current_dir(workspace)
			.output()
			.with_context(|| format!("Failed to run {} (from {lsp_source})", luau_lsp.to_string()))?;

		let combined = format!(
			"{}\n{}",
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr)
		);

		let mut analysis = parse_output(&combined, passthrough.gnu);
		let analyzer_exit = output.status.code().unwrap_or(-1);

		// A non-zero analyzer exit with nothing parsed is a tool failure
		// (bad flags, unreadable settings) — not "no diagnostics"
		let analyzer_failed = analyzer_exit != 0 && analysis.diagnostics.is_empty();

		// Scope filtering (--scope-only / --owned-only)
		let scoping = self.scope_only || self.owned_only;
		let mut suppressed: Vec<Diagnostic> = Vec::new();

		if scoping {
			let scopes = targets;
			let (kept, cut): (Vec<Diagnostic>, Vec<Diagnostic>) =
				analysis.diagnostics.into_iter().partition(|diagnostic| {
					let resolved = resolve_diagnostic_path(&diagnostic.file, workspace);
					let in_scope = scopes.iter().any(|scope| resolved.starts_with(scope));

					if !in_scope {
						return false;
					}

					if self.owned_only {
						let relative = resolved.strip_prefix(workspace).unwrap_or(&resolved);

						return !relative
							.components()
							.any(|component| is_vendor_component(&component.as_os_str().to_string_lossy()));
					}

					true
				});

			analysis.diagnostics = kept;
			suppressed = cut;
		}

		// Compile pass
		let compiler = self.run_compile_pass(workspace, targets, vendor_ignores)?;

		let analyzer_errors = analysis
			.diagnostics
			.iter()
			.filter(|diagnostic| diagnostic.severity == "error")
			.count();
		let warnings = analysis.diagnostics.len() - analyzer_errors;
		let compiler_errors = compiler.diagnostics.len();
		let errors = analyzer_errors + compiler_errors;
		let suppressed_errors = suppressed
			.iter()
			.filter(|diagnostic| diagnostic.severity == "error")
			.count();

		let ok = !analyzer_failed && errors == 0 && compiler.status != "failed";

		if self.raw {
			print_json(&json!({
				"ok": ok,
				"project": project_path.to_string(),
				"workspace": workspace.to_string(),
				"dataModel": {
					"requested": data_model.requested,
					"effective": data_model.effective,
					"strictness": data_model.strictness,
					"live": data_model.live,
					"liveNodes": data_model.live_nodes,
					"liveTruncated": data_model.live_truncated,
					"sourcemap": data_model.sourcemap,
					"mergedNodes": data_model.merged_nodes,
					"fallbackReason": data_model.fallback_reason,
				},
				"analyzer": {
					"tool": luau_lsp.to_string(),
					"status": if analyzer_failed { "failed" } else { "completed" },
					"exitCode": analyzer_exit,
					"diagnostics": analysis.diagnostics.iter().map(Diagnostic::to_json).collect::<Vec<Value>>(),
					"suppressed": { "count": suppressed.len(), "errors": suppressed_errors },
					"messages": analysis.messages,
					"unparsed": analysis.unparsed,
					"definitions": definitions.as_ref().map(|(path, source)| json!({
						"set": "@roblox",
						"path": path.to_string(),
						"source": source,
					})),
				},
				"compiler": {
					"mode": self.compile.as_str(),
					"status": compiler.status,
					"tool": compiler.tool.as_ref().map(|tool| tool.to_string()),
					"levels": COMPILE_LEVELS,
					"files": compiler.files,
					"diagnostics": compiler.diagnostics.iter().map(Diagnostic::to_json).collect::<Vec<Value>>(),
					"reason": compiler.reason,
				},
				"scope": {
					"paths": targets.iter().map(|target| target.to_string()).collect::<Vec<String>>(),
					"mode": if self.owned_only { "owned-only" } else if self.scope_only { "scope-only" } else { "all" },
				},
				"vendorIgnores": vendor_ignores,
				"errors": errors,
				"warnings": warnings,
			}));
		} else {
			if !self.summary {
				for diagnostic in analysis.diagnostics.iter().chain(compiler.diagnostics.iter()) {
					println!("{}", diagnostic.render());
				}
			}

			let compile_note = match compiler.status {
				"completed" => format!(", compile {} over {} file(s)", COMPILE_LEVELS.join("/"), compiler.files),
				"unavailable" => ", compile skipped (luau-compile unavailable)".to_owned(),
				"skipped" => String::new(),
				other => format!(", compile {other}"),
			};

			let scope_note = if scoping {
				format!(
					", {} suppressed outside scope ({} error(s))",
					suppressed.len(),
					suppressed_errors
				)
			} else {
				String::new()
			};

			wsync_info!(
				"lint: {} error(s), {} warning(s) — data model {} ({}{}){compile_note}{scope_note}",
				errors.to_string().bold(),
				warnings,
				data_model.requested,
				data_model.effective,
				data_model
					.live_nodes
					.map(|nodes| format!(", {nodes} live nodes"))
					.unwrap_or_default(),
			);
		}

		if analyzer_failed {
			let tail: Vec<&str> = analysis
				.unparsed
				.iter()
				.chain(analysis.messages.iter())
				.map(String::as_str)
				.take(6)
				.collect();

			bail!(
				"luau-lsp analyze failed (exit {analyzer_exit}) without diagnostics: {}",
				if tail.is_empty() {
					"no output".to_owned()
				} else {
					tail.join(" | ")
				}
			);
		}

		if compiler.status == "failed" {
			bail!(
				"the required luau-compile stage failed: {}",
				compiler.reason.as_deref().unwrap_or("unknown")
			);
		}

		if errors > 0 {
			bail!("lint found {errors} error(s)");
		}

		Ok(())
	}

	/// The `-O0`/`-O1`/`-O2` bytecode pass over every in-scope script
	fn run_compile_pass(
		&self,
		workspace: &Path,
		targets: &[PathBuf],
		vendor_ignores: &[&'static str],
	) -> Result<CompileReport> {
		if self.compile == CompileMode::Off {
			return Ok(CompileReport {
				status: "skipped",
				..CompileReport::default()
			});
		}

		let resolved = resolve_tool(
			self.luau_compile.as_deref(),
			&["WSYNC_LUAU_COMPILE", "LUAU_COMPILE"],
			"luau-compile",
			"It ships with Luau: `rokit add luau-lang/luau`, `aftman add luau-lang/luau`, or `brew install luau`",
		);

		let (tool, _) = match resolved {
			Ok(tool) => tool,
			Err(err) => {
				if self.compile == CompileMode::Required {
					return Ok(CompileReport {
						status: "failed",
						reason: Some(err.to_string()),
						..CompileReport::default()
					});
				}

				return Ok(CompileReport {
					status: "unavailable",
					reason: Some(err.to_string()),
					..CompileReport::default()
				});
			}
		};

		// In-scope scripts: the explicit targets, or the workspace, minus
		// active vendor ignores and user globs
		let user_globs: Vec<glob::Pattern> = self
			.ignore
			.iter()
			.filter_map(|pattern| glob::Pattern::new(pattern).ok())
			.collect();

		let mut files: Vec<PathBuf> = Vec::new();

		if targets.is_empty() {
			collect_scripts(workspace, workspace, vendor_ignores, &user_globs, &mut files);
		} else {
			for target in targets {
				if target.is_dir() {
					collect_scripts(target, workspace, vendor_ignores, &user_globs, &mut files);
				} else if is_script(target) {
					files.push(target.clone());
				}
			}
		}

		files.sort();
		files.dedup();

		if files.is_empty() {
			return Ok(CompileReport {
				status: "completed",
				tool: Some(tool),
				..CompileReport::default()
			});
		}

		let mut diagnostics: Vec<Diagnostic> = Vec::new();
		let mut failure: Option<String> = None;

		for level in COMPILE_LEVELS {
			for batch in files.chunks(COMPILE_BATCH) {
				let output = Command::new(&tool)
					.arg("--null")
					.arg(level)
					.args(batch.iter().map(|file| relative_to(file, workspace)))
					.current_dir(workspace)
					.output()
					.with_context(|| format!("Failed to run {}", tool.to_string()))?;

				let stderr = String::from_utf8_lossy(&output.stderr);
				let parsed = parse_output(&stderr, false);
				let mut found = false;

				for mut diagnostic in parsed.diagnostics {
					found = true;
					diagnostic.source = "compiler";
					diagnostic.severity = "error";

					// The same failure usually reproduces at every level;
					// collapse it into one record carrying the levels
					if let Some(existing) = diagnostics.iter_mut().find(|existing| {
						existing.file == diagnostic.file
							&& existing.line == diagnostic.line
							&& existing.column == diagnostic.column
							&& existing.tag == diagnostic.tag
							&& existing.message == diagnostic.message
					}) {
						if !existing.levels.iter().any(|existing| existing == level) {
							existing.levels.push(level.to_owned());
						}
					} else {
						diagnostic.levels = vec![level.to_owned()];
						diagnostics.push(diagnostic);
					}
				}

				// A failing exit with no parsed diagnostic is a tool-level
				// failure (bad binary, bad flags), not a compile error
				if !output.status.success() && !found && failure.is_none() {
					failure = Some(format!(
						"{} exited {} at {level} without diagnostics: {}",
						tool.to_string(),
						output.status.code().unwrap_or(-1),
						crate::cli::client::truncate(stderr.trim(), 200)
					));
				}
			}
		}

		if let Some(reason) = failure {
			return Ok(CompileReport {
				status: "failed",
				tool: Some(tool),
				files: files.len(),
				diagnostics,
				reason: Some(reason),
			});
		}

		Ok(CompileReport {
			status: "completed",
			tool: Some(tool),
			files: files.len(),
			diagnostics,
			reason: None,
		})
	}
}

#[derive(Default)]
struct CompileReport {
	status: &'static str,
	tool: Option<PathBuf>,
	files: usize,
	diagnostics: Vec<Diagnostic>,
	reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Passthrough scanning
// ---------------------------------------------------------------------------

/// Reads the `-- <luau-lsp args>` tail before anything runs: rejects the
/// plain formatter, and detects the overrides that change WSync's own setup
fn scan_passthrough(args: &[String]) -> Result<Passthrough> {
	let mut result = Passthrough {
		gnu: false,
		replaces_sourcemap: false,
		replaces_roblox_definitions: false,
		has_no_strict_flag: false,
	};

	let mut index = 0;

	while index < args.len() {
		let arg = &args[index];

		// A flag's value, either `--flag=value` or the next token
		let value_of = |name: &str| -> Option<String> {
			if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
				return Some(value.to_owned());
			}

			if arg == name {
				return args.get(index + 1).cloned();
			}

			None
		};

		if let Some(formatter) = value_of("--formatter") {
			match formatter.as_str() {
				"plain" => bail!(
					"--formatter=plain is rejected: luau-lsp 1.68.1 can emit TypeErrors in that mode while \
					 returning a successful process status, which would let broken code pass the lint"
				),
				"gnu" => result.gnu = true,
				_ => {}
			}
		}

		if value_of("--sourcemap").is_some() {
			result.replaces_sourcemap = true;
		}

		for flag in ["--definitions", "--defs"] {
			if let Some(value) = value_of(flag) {
				if value.starts_with("@roblox=") {
					result.replaces_roblox_definitions = true;
				}
			}

			// The registry's `--definitions:@roblox=…` spelling
			if arg.starts_with(&format!("{flag}:@roblox=")) {
				result.replaces_roblox_definitions = true;
			}
		}

		if arg == "--no-strict-dm-types" {
			result.has_no_strict_flag = true;
		}

		index += 1;
	}

	Ok(result)
}

// ---------------------------------------------------------------------------
// Sourcemap generation and the live merge
// ---------------------------------------------------------------------------

/// Generates the temporary sourcemap in-process — the engine's machinery,
/// no daemon. Non-scripts are included so DataModel coverage is complete
fn generate_sourcemap(project_path: &Path, temp: &Path) -> Result<()> {
	if let Some(parent) = temp.parent() {
		fs::create_dir_all(parent)
			.with_context(|| format!("Failed to create the lint scratch directory {}", parent.to_string()))?;
	}

	let project = Project::load(project_path)?;
	let core = Core::new(project, false)?;

	core.sourcemap(Some(temp.to_owned()), true)
		.context("Failed to generate the temporary sourcemap")
}

/// Merges the live Studio tree into the generated sourcemap on disk: nodes
/// the disk projection lacks are added (name + class, no file mappings), so
/// strict DataModel types stop flagging Studio-only instances. Returns how
/// many nodes were added
fn merge_live_tree(sourcemap_path: &Path, live_root: &Value) -> Result<u64> {
	let text = fs::read_to_string(sourcemap_path)
		.with_context(|| format!("Failed to read the generated sourcemap {}", sourcemap_path.to_string()))?;

	let mut sourcemap: Value = serde_json::from_str(&text).context("The generated sourcemap does not parse")?;

	let mut added = 0;

	if live_root.is_object() {
		merge_node(&mut sourcemap, live_root, &mut added);
	}

	fs::write(sourcemap_path, serde_json::to_string(&sourcemap)?)
		.with_context(|| format!("Failed to write the merged sourcemap {}", sourcemap_path.to_string()))?;

	Ok(added)
}

fn merge_node(map_node: &mut Value, live_node: &Value, added: &mut u64) {
	let Some(live_children) = live_node.get("children").and_then(Value::as_array) else {
		return;
	};

	if !map_node.is_object() {
		return;
	}

	if map_node.get("children").is_none() {
		map_node["children"] = json!([]);
	}

	for live_child in live_children {
		let Some(name) = live_child.get("name").and_then(Value::as_str) else {
			continue;
		};

		let children = map_node["children"].as_array_mut().expect("children was just ensured");

		let position = children
			.iter()
			.position(|child| child.get("name").and_then(Value::as_str) == Some(name));

		match position {
			Some(position) => merge_node(&mut children[position], live_child, added),
			None => {
				children.push(convert_live_node(live_child, added));
			}
		}
	}
}

/// A live-only subtree as sourcemap nodes: name and class, no file paths
fn convert_live_node(live_node: &Value, added: &mut u64) -> Value {
	*added += 1;

	let mut node = Map::new();

	node.insert(
		"name".to_owned(),
		live_node.get("name").cloned().unwrap_or_else(|| json!("")),
	);
	node.insert(
		"className".to_owned(),
		live_node.get("class").cloned().unwrap_or_else(|| json!("Instance")),
	);

	if let Some(children) = live_node.get("children").and_then(Value::as_array) {
		if !children.is_empty() {
			node.insert(
				"children".to_owned(),
				Value::Array(children.iter().map(|child| convert_live_node(child, added)).collect()),
			);
		}
	}

	Value::Object(node)
}

// ---------------------------------------------------------------------------
// Definitions cache
// ---------------------------------------------------------------------------

/// The `@roblox` definitions: a fresh cache is used as-is; otherwise the
/// current file is downloaded into the state directory. Offline runs fall
/// back to a stale cache; offline with no cache is a hard error naming the
/// cache path
fn resolve_global_types(state_dir: &Path, raw: bool) -> Result<(PathBuf, &'static str)> {
	let cache = state_dir.join("lint").join("globalTypes.d.luau");

	if let Ok(metadata) = fs::metadata(&cache) {
		let fresh = metadata
			.modified()
			.ok()
			.and_then(|modified| modified.elapsed().ok())
			.is_some_and(|age| age < CACHE_MAX_AGE);

		if fresh {
			return Ok((cache, "cache"));
		}
	}

	let url = env::var(GLOBAL_TYPES_URL_ENV)
		.ok()
		.filter(|value| !value.trim().is_empty())
		.unwrap_or_else(|| GLOBAL_TYPES_URL.to_owned());

	match download_global_types(&url) {
		Ok(body) => {
			if let Some(parent) = cache.parent() {
				fs::create_dir_all(parent)
					.with_context(|| format!("Failed to create the cache directory {}", parent.to_string()))?;
			}

			let temp = cache.with_extension(format!("tmp-{}", std::process::id()));

			fs::write(&temp, &body).with_context(|| format!("Failed to write {}", temp.to_string()))?;
			fs::rename(&temp, &cache).with_context(|| format!("Failed to move {} into place", cache.to_string()))?;

			Ok((cache, "downloaded"))
		}
		Err(err) if cache.exists() => {
			if !raw {
				wsync_warn!(
					"Definitions download failed ({err}) — using the cached copy at {}",
					cache.to_string()
				);
			}

			Ok((cache, "stale-cache"))
		}
		Err(err) => bail!(
			"No Roblox definitions available: downloading {url} failed ({err}) and no cache exists at {}. \
			 Place a globalTypes.d.luau there manually, or pass `--definitions:@roblox=<path>` after `--`",
			cache.to_string()
		),
	}
}

fn download_global_types(url: &str) -> Result<String> {
	let response = reqwest::blocking::Client::builder()
		.timeout(DOWNLOAD_TIMEOUT)
		.build()?
		.get(url)
		.send()?
		.error_for_status()?;

	let body = response.text()?;

	// A captive portal or an error page must never be cached as definitions
	if !body.contains("declare") {
		bail!("the response does not look like a Luau definitions file");
	}

	Ok(body)
}

// ---------------------------------------------------------------------------
// Toolchain resolution
// ---------------------------------------------------------------------------

/// The documented resolution chain: explicit flag, environment variables,
/// PATH, then the Rokit and Aftman bins — ending in an actionable error
fn resolve_tool(
	explicit: Option<&Path>,
	env_vars: &[&str],
	name: &str,
	install_hint: &str,
) -> Result<(PathBuf, String)> {
	if let Some(explicit) = explicit {
		let explicit = explicit.resolve()?;

		if !explicit.is_file() {
			bail!("--{name} points at {}, which does not exist", explicit.to_string());
		}

		return Ok((explicit, format!("--{name}")));
	}

	for env_var in env_vars {
		if let Some(value) = env::var_os(env_var).filter(|value| !value.is_empty()) {
			let path = PathBuf::from(&value);

			if !path.is_file() {
				bail!("{env_var} points at {}, which does not exist", path.to_string());
			}

			return Ok((path, format!("${env_var}")));
		}
	}

	let binary = if cfg!(windows) {
		format!("{name}.exe")
	} else {
		name.to_owned()
	};

	if let Some(paths) = env::var_os("PATH") {
		for dir in env::split_paths(&paths) {
			let candidate = dir.join(&binary);

			if candidate.is_file() {
				return Ok((candidate, "PATH".to_owned()));
			}
		}
	}

	if let Some(base) = directories::BaseDirs::new() {
		for tool_dir in [".rokit", ".aftman"] {
			let candidate = base.home_dir().join(tool_dir).join("bin").join(&binary);

			if candidate.is_file() {
				return Ok((candidate, format!("~/{tool_dir}/bin")));
			}
		}
	}

	bail!(
		"{name} is not installed (checked --{name}, {}, PATH, ~/.rokit/bin, and ~/.aftman/bin). {install_hint}",
		env_vars.join(", ")
	)
}

// ---------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------

struct ParsedOutput {
	diagnostics: Vec<Diagnostic>,
	/// `[WARN]`/`[ERROR]` analyzer log lines — configuration problems live
	/// here
	messages: Vec<String>,
	/// Non-empty lines that matched nothing — preserved, never dropped
	unparsed: Vec<String>,
}

/// Parses combined tool output. luau-lsp writes diagnostics to stderr in
/// both the default and GNU formats; both are recognized, and adversarial
/// names (parentheses, colons) are survived by scanning for the *last*
/// well-formed position marker
fn parse_output(text: &str, gnu: bool) -> ParsedOutput {
	let mut result = ParsedOutput {
		diagnostics: Vec::new(),
		messages: Vec::new(),
		unparsed: Vec::new(),
	};

	for line in text.lines() {
		let line = line.trim_end();

		if line.trim().is_empty() {
			continue;
		}

		if line.starts_with("[INFO]") || line.starts_with("[DEBUG]") || line.starts_with("[VERBOSE]") {
			continue;
		}

		if line.starts_with("[WARN]") || line.starts_with("[ERROR]") {
			result.messages.push(line.to_owned());

			continue;
		}

		// luau-compile's stdout stats line
		if line.starts_with("Compiled ") && line.contains(" KLOC ") {
			continue;
		}

		let parsed = if gnu {
			parse_gnu_line(line).or_else(|| parse_default_line(line))
		} else {
			parse_default_line(line)
		};

		match parsed {
			Some(diagnostic) => result.diagnostics.push(diagnostic),
			None => result.unparsed.push(line.to_owned()),
		}
	}

	result
}

/// `path(line,col): Tag: message` — the rightmost well-formed `(l,c): `
/// marker wins, so a path containing `(1,2)` cannot fool the parser
fn parse_default_line(line: &str) -> Option<Diagnostic> {
	for (index, _) in line.rmatch_indices("): ") {
		let head = &line[..index];

		let Some(open) = head.rfind('(') else { continue };
		let position = &head[open + 1..];

		let Some((line_text, column_text)) = position.split_once(',') else {
			continue;
		};

		let (Ok(line_number), Ok(column)) = (line_text.trim().parse::<u64>(), column_text.trim().parse::<u64>()) else {
			continue;
		};

		let file = head[..open].trim();

		if file.is_empty() {
			continue;
		}

		let rest = &line[index + 3..];
		let Some((tag, message)) = split_tag(rest) else {
			continue;
		};

		return Some(Diagnostic {
			file: normalize_file(file),
			line: line_number,
			column,
			severity: severity_of(&tag),
			tag,
			message,
			source: "analyzer",
			levels: Vec::new(),
		});
	}

	None
}

/// `path:1.1-1.24: Tag: message` — luau-lsp's GNU formatter
fn parse_gnu_line(line: &str) -> Option<Diagnostic> {
	let (head, rest) = line.split_once(": ")?;
	let (file, range) = head.rsplit_once(':')?;

	if file.is_empty()
		|| range.is_empty()
		|| !range
			.chars()
			.all(|character| character.is_ascii_digit() || character == '.' || character == '-')
	{
		return None;
	}

	let start = range.split('-').next()?;
	let (line_text, column_text) = start.split_once('.')?;
	let line_number = line_text.parse::<u64>().ok()?;
	let column = column_text.parse::<u64>().ok()?;

	let (tag, message) = split_tag(rest)?;

	Some(Diagnostic {
		file: normalize_file(file),
		line: line_number,
		column,
		severity: severity_of(&tag),
		tag,
		message,
		source: "analyzer",
		levels: Vec::new(),
	})
}

/// `Tag: message` → the tag must look like a diagnostic name, not prose
fn split_tag(rest: &str) -> Option<(String, String)> {
	let (tag, message) = rest.split_once(": ").unwrap_or((rest, ""));
	let tag = tag.trim();

	let named = !tag.is_empty()
		&& tag.len() <= 64
		&& tag
			.chars()
			.all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-');

	if !named {
		return None;
	}

	Some((tag.to_owned(), message.trim().to_owned()))
}

fn severity_of(tag: &str) -> &'static str {
	if tag.contains("Error") {
		"error"
	} else {
		"warning"
	}
}

/// Strips the `./` prefix luau-compile puts on relative paths, so analyzer
/// and compiler diagnostics for one file collate
fn normalize_file(file: &str) -> String {
	file.strip_prefix("./").unwrap_or(file).to_owned()
}

fn resolve_diagnostic_path(file: &str, workspace: &Path) -> PathBuf {
	let path = Path::new(file);

	if path.is_absolute() {
		path.to_owned()
	} else {
		workspace.join(path)
	}
}

// ---------------------------------------------------------------------------
// Script collection for the compile pass
// ---------------------------------------------------------------------------

fn is_script(path: &Path) -> bool {
	matches!(path.get_ext().to_ascii_lowercase().as_str(), "luau" | "lua")
}

fn matches_vendor(component: &str, pattern: &str) -> bool {
	match pattern.strip_suffix('*') {
		Some(prefix) => component.starts_with(prefix),
		None => component == pattern,
	}
}

fn is_vendor_component(component: &str) -> bool {
	VENDOR_PATTERNS.iter().any(|pattern| matches_vendor(component, pattern))
}

fn collect_scripts(
	dir: &Path,
	workspace: &Path,
	vendor_ignores: &[&'static str],
	user_globs: &[glob::Pattern],
	files: &mut Vec<PathBuf>,
) {
	let Ok(entries) = fs::read_dir(dir) else { return };

	let mut entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();

	entries.sort();

	for entry in entries {
		let name = entry.get_name().to_owned();
		let relative = entry.strip_prefix(workspace).unwrap_or(&entry).to_owned();
		let relative_text = relative.to_string_lossy().replace('\\', "/");

		if user_globs.iter().any(|pattern| pattern.matches(&relative_text)) {
			continue;
		}

		if entry.is_dir() {
			if !vendor_ignores.iter().any(|pattern| matches_vendor(&name, pattern)) {
				collect_scripts(&entry, workspace, vendor_ignores, user_globs, files);
			}
		} else if is_script(&entry) {
			files.push(entry);
		}
	}
}

fn relative_to(path: &Path, workspace: &Path) -> String {
	path.strip_prefix(workspace)
		.map(|relative| relative.to_string_lossy().into_owned())
		.unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_lines_parse_including_adversarial_names() {
		let diagnostic = parse_default_line("src/Hello.luau(1,7): TypeError: boom").unwrap();

		assert_eq!(diagnostic.file, "src/Hello.luau");
		assert_eq!((diagnostic.line, diagnostic.column), (1, 7));
		assert_eq!(diagnostic.tag, "TypeError");
		assert_eq!(diagnostic.severity, "error");
		assert_eq!(diagnostic.message, "boom");

		// A path containing a fake position marker
		let diagnostic = parse_default_line("src/evil(1,2)/thing.luau(3,4): LocalUnused: shadowed").unwrap();

		assert_eq!(diagnostic.file, "src/evil(1,2)/thing.luau");
		assert_eq!((diagnostic.line, diagnostic.column), (3, 4));
		assert_eq!(diagnostic.severity, "warning");

		// luau-compile's ./-prefixed paths normalize
		let diagnostic = parse_default_line("./src/Broken.luau(3,1): SyntaxError: Expected identifier").unwrap();

		assert_eq!(diagnostic.file, "src/Broken.luau");

		assert!(parse_default_line("[INFO] not a diagnostic").is_none());
		assert!(parse_default_line("just some text").is_none());
		assert!(parse_default_line("(1,2): : empty tag").is_none());
	}

	#[test]
	fn gnu_lines_parse() {
		let diagnostic = parse_gnu_line("src/Hello.luau:1.1-1.24: TypeError: bad type").unwrap();

		assert_eq!(diagnostic.file, "src/Hello.luau");
		assert_eq!((diagnostic.line, diagnostic.column), (1, 1));
		assert_eq!(diagnostic.tag, "TypeError");

		assert!(parse_gnu_line("src/Hello.luau(1,1): TypeError: default form").is_none());
	}

	#[test]
	fn parsing_sorts_logs_diagnostics_and_unparsed() {
		let text = "[INFO] Loading definitions file: @roblox - defs\n\
			[WARN] client does not allow registration\n\
			src/A.luau(1,1): TypeError: x\n\
			Compiled 0 KLOC into 0 KB bytecode (read 0.00s, parse 0.00s, compile 0.00s)\n\
			something unstructured\n";

		let parsed = parse_output(text, false);

		assert_eq!(parsed.diagnostics.len(), 1);
		assert_eq!(parsed.messages.len(), 1);
		assert_eq!(parsed.unparsed, vec!["something unstructured".to_owned()]);
	}

	#[test]
	fn passthrough_scanning_detects_overrides_and_rejects_plain() {
		assert!(scan_passthrough(&["--formatter=plain".to_owned()]).is_err());
		assert!(scan_passthrough(&["--formatter".to_owned(), "plain".to_owned()]).is_err());

		let scanned = scan_passthrough(&["--formatter=gnu".to_owned()]).unwrap();

		assert!(scanned.gnu);

		let scanned = scan_passthrough(&["--sourcemap=own.json".to_owned()]).unwrap();

		assert!(scanned.replaces_sourcemap);

		for spelling in [
			"--definitions=@roblox=my.d.luau",
			"--definitions:@roblox=my.d.luau",
			"--defs=@roblox=my.d.luau",
		] {
			let scanned = scan_passthrough(&[spelling.to_owned()]).unwrap();

			assert!(scanned.replaces_roblox_definitions, "{spelling} must replace the set");
		}

		// A different named set coexists with the bundled one
		let scanned = scan_passthrough(&["--definitions=@testez=t.d.luau".to_owned()]).unwrap();

		assert!(!scanned.replaces_roblox_definitions);
	}

	#[test]
	fn vendor_matching_covers_exact_and_prefix_patterns() {
		assert!(matches_vendor("Packages", "Packages"));
		assert!(matches_vendor("MadworkProfileService", "Madwork*"));
		assert!(matches_vendor(".wsync-backup", ".wsync-*"));
		assert!(!matches_vendor("MyPackages", "Packages"));
		assert!(is_vendor_component("_Index"));
		assert!(!is_vendor_component("src"));
	}

	#[test]
	fn live_merge_adds_missing_nodes_only() {
		let mut sourcemap = json!({
			"name": "root", "className": "DataModel",
			"children": [
				{ "name": "ReplicatedStorage", "className": "ReplicatedStorage", "children": [
					{ "name": "Hello", "className": "ModuleScript", "filePaths": ["src/Hello.luau"] }
				]}
			]
		});

		let live = json!({
			"name": "game", "class": "DataModel",
			"children": [
				{ "name": "ReplicatedStorage", "class": "ReplicatedStorage", "children": [
					{ "name": "Hello", "class": "ModuleScript", "children": [] },
					{ "name": "StudioOnly", "class": "Folder", "children": [
						{ "name": "Inner", "class": "Part", "children": [] }
					]}
				]},
				{ "name": "Workspace", "class": "Workspace", "children": [] }
			]
		});

		let mut added = 0;

		merge_node(&mut sourcemap, &live, &mut added);

		// StudioOnly + Inner + Workspace
		assert_eq!(added, 3);

		let replicated = &sourcemap["children"][0];

		assert_eq!(replicated["children"].as_array().unwrap().len(), 2);
		assert_eq!(replicated["children"][1]["name"], "StudioOnly");
		assert_eq!(replicated["children"][1]["className"], "Folder");
		assert_eq!(replicated["children"][1]["children"][0]["name"], "Inner");
		// The disk-backed node kept its file mapping
		assert_eq!(replicated["children"][0]["filePaths"][0], "src/Hello.luau");
		assert_eq!(sourcemap["children"][1]["name"], "Workspace");
	}
}
