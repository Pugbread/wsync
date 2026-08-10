//! Renders the generated agent docs (Design §10.6).
//!
//! Pure: facts in, strings out. Every command named in the output comes from
//! [`RegistryFacts`], every path and class from [`ProjectFacts`], so the
//! steering can never describe a project or a command surface that does not
//! exist. There are no timestamps — two runs on the same facts are identical.
//!
//! The section arc mirrors Ro-Sync's `ro-sync.md` §0–§7, which is the
//! normative reference for this steering text, with WSync's reality
//! substituted wherever the two differ.

use std::fmt::Write as _;

use super::facts::{CommandFact, DocsInput, EnvFacts, ProjectFacts, RegistryFacts, RuleGroup, Safety, SyncRuleFact};
use crate::middleware::Middleware;

/// Column the generated prose wraps at. Interpolated project paths, command
/// names and class names make hand-wrapped source literals unreliable, so
/// every prose block is reflowed on the way out
const WRAP_WIDTH: usize = 78;

/// Accumulates markdown blocks separated by exactly one blank line
#[derive(Default)]
struct Doc {
	text: String,
}

impl Doc {
	/// Appends one block of markdown, reflowing prose and normalizing the
	/// separation around it
	fn block(&mut self, block: impl AsRef<str>) {
		let block = wrap_block(block.as_ref().trim_matches('\n'));

		if block.is_empty() {
			return;
		}

		self.text.push_str(&block);
		self.text.push_str("\n\n");
	}

	fn blocks<I, S>(&mut self, blocks: I)
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		for block in blocks {
			self.block(block);
		}
	}

	/// A fenced block; skipped entirely when there is nothing to show
	fn code(&mut self, lines: &[String]) {
		if lines.is_empty() {
			return;
		}

		let mut block = String::from("```\n");

		for line in lines {
			block.push_str(line);
			block.push('\n');
		}

		block.push_str("```");

		self.block(block);
	}

	fn finish(self) -> String {
		let mut text = self.text;

		while text.ends_with('\n') {
			text.pop();
		}

		text.push('\n');
		text
	}
}

////////////////////////////////////////////////////////////////////////////////
// Wrapping
////////////////////////////////////////////////////////////////////////////////

/// Leading list marker of a line (`- `, `1. `), with the indent that its
/// continuation lines need
fn list_marker(line: &str) -> Option<(String, String)> {
	let indent = line.len() - line.trim_start().len();
	let rest = &line[indent..];

	let marker_len = if rest.starts_with("- ") || rest.starts_with("* ") {
		2
	} else {
		let digits = rest.chars().take_while(char::is_ascii_digit).count();

		if digits == 0 || !rest[digits..].starts_with(". ") {
			return None;
		}

		digits + 2
	};

	Some((
		format!("{}{}", " ".repeat(indent), &rest[..marker_len]),
		" ".repeat(indent + marker_len),
	))
}

/// Splits on whitespace, except inside a backtick span: `` `wsync tree` ``
/// stays one token so a code span is never split across two lines
fn tokenize(text: &str) -> Vec<String> {
	let mut tokens = Vec::new();
	let mut current = String::new();
	let mut in_code = false;

	for character in text.chars() {
		if character == '`' {
			in_code = !in_code;
			current.push(character);
			continue;
		}

		if character.is_whitespace() && !in_code {
			if !current.is_empty() {
				tokens.push(std::mem::take(&mut current));
			}

			continue;
		}

		current.push(character);
	}

	if !current.is_empty() {
		tokens.push(current);
	}

	tokens
}

/// Greedy word wrap. Words longer than the budget (a long path in backticks)
/// overflow rather than being broken
fn wrap_words(text: &str, first_prefix: &str, prefix: &str) -> String {
	let mut lines: Vec<String> = Vec::new();
	let mut current = String::from(first_prefix);
	let mut has_word = false;

	for word in tokenize(text) {
		let word = word.as_str();
		let candidate = if has_word {
			current.len() + 1 + word.len()
		} else {
			current.len() + word.len()
		};

		if has_word && candidate > WRAP_WIDTH {
			lines.push(std::mem::replace(&mut current, format!("{prefix}{word}")));
		} else {
			if has_word {
				current.push(' ');
			}

			current.push_str(word);
		}

		has_word = true;
	}

	lines.push(current);
	lines.join("\n")
}

/// Reflows one markdown block. Tables and fenced code are left byte-for-byte;
/// paragraphs and list items are re-wrapped so interpolated values cannot make
/// a line run long
fn wrap_block(block: &str) -> String {
	if block.is_empty() {
		return String::new();
	}

	if block
		.lines()
		.any(|line| line.trim_start().starts_with('|') || line.starts_with("```"))
	{
		return block.to_owned();
	}

	let mut out: Vec<String> = Vec::new();
	// The item being accumulated: its first-line prefix, continuation prefix
	// and the text collected so far
	let mut pending: Option<(String, String, String)> = None;

	let flush = |pending: &mut Option<(String, String, String)>, out: &mut Vec<String>| {
		if let Some((first, prefix, text)) = pending.take() {
			out.push(wrap_words(&text, &first, &prefix));
		}
	};

	for line in block.lines() {
		if line.trim().is_empty() {
			flush(&mut pending, &mut out);
			out.push(String::new());
			continue;
		}

		match list_marker(line) {
			Some((first, prefix)) => {
				flush(&mut pending, &mut out);
				let text = line.trim_start()[first.trim_start().len()..].trim().to_owned();
				pending = Some((first, prefix, text));
			}
			None => match pending.as_mut() {
				Some((_, _, text)) => {
					text.push(' ');
					text.push_str(line.trim());
				}
				None => pending = Some((String::new(), String::new(), line.trim().to_owned())),
			},
		}
	}

	flush(&mut pending, &mut out);

	out.join("\n")
}

////////////////////////////////////////////////////////////////////////////////
// Helpers
////////////////////////////////////////////////////////////////////////////////

/// `` `wsync a`, `wsync b`, and `wsync c` ``
fn join_commands(names: &[&str]) -> String {
	let quoted: Vec<String> = names.iter().map(|name| format!("`wsync {name}`")).collect();

	match quoted.len() {
		0 => String::new(),
		1 => quoted[0].clone(),
		2 => format!("{} and {}", quoted[0], quoted[1]),
		_ => format!(
			"{}, and {}",
			quoted[..quoted.len() - 1].join(", "),
			quoted[quoted.len() - 1]
		),
	}
}

/// One honest sentence about commands the registry documents but this build
/// does not implement yet. `None` when everything asked for is available
fn pending_note(registry: &RegistryFacts, names: &[&str], tail: &str) -> Option<String> {
	let pending = registry.pending(names);

	if pending.is_empty() {
		return None;
	}

	let verb = if pending.len() == 1 { "is" } else { "are" };

	Some(format!(
		"{} {verb} in the command registry but not in this build yet. {tail}",
		join_commands(&pending)
	))
}

/// `` `wsync tree`, `wsync ls`, or `wsync get --prop` `` — built from the
/// shipped subset of `entries`, where each entry is `(command, display form)`.
/// `None` when this build has none of them, so the caller can drop the whole
/// sentence rather than steer at a command that does not exist
fn shipped_phrase(registry: &RegistryFacts, entries: &[(&str, &str)], conjunction: &str) -> Option<String> {
	let items: Vec<String> = entries
		.iter()
		.filter(|(name, _)| registry.is_shipped(name))
		.map(|(_, display)| format!("`wsync {display}`"))
		.collect();

	match items.len() {
		0 => None,
		1 => Some(items[0].clone()),
		2 => Some(format!("{} {conjunction} {}", items[0], items[1])),
		_ => Some(format!(
			"{}, {conjunction} {}",
			items[..items.len() - 1].join(", "),
			items[items.len() - 1]
		)),
	}
}

/// First registry example for each shipped command, in the order asked for
fn examples(registry: &RegistryFacts, names: &[&str], per_command: usize) -> Vec<String> {
	registry
		.pick(names)
		.into_iter()
		.flat_map(|command| command.first_examples(per_command).to_vec())
		.collect()
}

/// `- \`wsync get\` — Reads an instance view or one property…`
fn summary_list(commands: &[&CommandFact]) -> Vec<String> {
	commands
		.iter()
		.map(|command| format!("- `wsync {}` — {}", command.name, command.summary))
		.collect()
}

/// A Studio path this project actually maps, for use in examples
fn example_studio_path(project: &ProjectFacts) -> String {
	project
		.roots
		.iter()
		.find(|root| !root.studio_path.is_empty())
		.map(|root| root.studio_path.clone())
		.unwrap_or_else(|| String::from("Workspace"))
}

/// How one sync rule projects onto a Roblox instance
fn instance_for(rule: &SyncRuleFact) -> String {
	match rule.middleware {
		Middleware::ServerScript => String::from("`Script` named `Foo` (`RunContext = Legacy`)"),
		Middleware::RunServerScript => String::from("`Script` named `Foo` (`RunContext = Server`)"),
		Middleware::ClientScript => String::from("`Script` named `Foo` (`RunContext = Client`)"),
		Middleware::LocalScript => String::from("`LocalScript` named `Foo`"),
		Middleware::ModuleScript => String::from("`ModuleScript` named `Foo`"),
		Middleware::StringValue => String::from("`StringValue` named `Foo`, `Value` is the file text"),
		Middleware::RichStringValue => {
			String::from("`StringValue` named `Foo`, `Value` is the Markdown converted to rich text")
		}
		Middleware::LocalizationTable => String::from("`LocalizationTable` named `Foo`"),
		Middleware::JsonModule | Middleware::TomlModule | Middleware::YamlModule | Middleware::MsgpackModule => {
			String::from("`ModuleScript` named `Foo` that returns the decoded table")
		}
		Middleware::JsonModel => String::from("the instance tree the model file describes, rooted at `Foo`"),
		Middleware::RbxmModel | Middleware::RbxmxModel => {
			String::from("the serialized instance tree in the file, rooted at `Foo`")
		}
		Middleware::Project => String::from("a nested project; `$path` resolves through that project file"),
		Middleware::InstanceData => String::from(
			"metadata for the sibling instance: `$className`, properties, attributes, tags, `originalName`",
		),
	}
}

/// The conventions table for one group, generated from the active rules
fn conventions_table(project: &ProjectFacts, group: RuleGroup) -> Option<String> {
	let rules = project.primary_rules_in(group);

	if rules.is_empty() {
		return None;
	}

	let mut table = String::new();
	let _ = writeln!(table, "**{}**", group.heading());
	table.push('\n');
	let _ = writeln!(table, "| On disk | Roblox instance |");
	let _ = writeln!(table, "| --- | --- |");

	for rule in rules {
		let Some(file) = rule.example_file() else {
			continue;
		};

		let _ = writeln!(table, "| `{file}` | {} |", instance_for(rule));
	}

	Some(table.trim_end().to_owned())
}

/// `init`-style forms, generated from the active rules' child patterns
fn init_forms(project: &ProjectFacts) -> Vec<String> {
	project
		.primary_rules_in(RuleGroup::Scripts)
		.into_iter()
		.filter_map(|rule| {
			let init = rule.example_init_file()?;

			Some(format!(
				"- `Foo/{init}` — {}, with the directory's other entries as its children",
				instance_for(rule)
			))
		})
		.collect()
}

////////////////////////////////////////////////////////////////////////////////
// Sections
////////////////////////////////////////////////////////////////////////////////

fn intro(doc: &mut Doc, project: &ProjectFacts) {
	doc.block(format!(
		"WSync mirrors this directory into a Roblox Studio DataModel and back. Read\n\
		 this file before editing: the synced scope is exactly what `{}` maps, and\n\
		 nothing else.",
		project.project_file
	));
}

fn bootstrap(doc: &mut Doc, input: &DocsInput) {
	let (project, registry, env) = (input.project, input.registry, input.env);

	doc.block("## 0. Agent bootstrap");

	doc.block(
		"You are in a WSync project. Do not look for `rbxcloud`, Rojo upload scripts,\n\
		 or ad-hoc Roblox tooling before trying the built-in CLI.",
	);

	doc.block("Use `wsync` directly, but confirm the binary is the modern one first:");
	doc.code(&[String::from("wsync --version"), String::from("wsync --help")]);

	doc.block(
		"If `wsync` is not on `PATH`, do not go looking for unrelated Roblox tooling.\n\
		 Use the installed binary directly:",
	);

	doc.block(
		"- macOS / Linux: `~/.wsync/bin/wsync`\n\
		 - Windows: `%USERPROFILE%\\.wsync\\bin\\wsync.exe`",
	);

	let canonical = ["context", "status", "path"];
	let mut first = Vec::new();

	if registry.is_shipped("context") {
		first.push(String::from("wsync context --project ."));
	}

	if registry.is_shipped("status") {
		first.push(String::from("wsync status --project . --raw"));
	}

	if registry.is_shipped("path") {
		first.push(format!("wsync path --project . {}", example_studio_path(project)));
	}

	if !first.is_empty() {
		doc.block("From the project root, start with:");
		doc.code(&first);
	}

	if let Some(note) = pending_note(
		registry,
		&canonical,
		"Use `wsync --help` to see what this build does expose, and do not \
		 substitute an unrelated tool for a command that is simply not built yet.",
	) {
		doc.block(note);
	}

	doc.block(
		"Do not run `diff`, `changes`, `conflicts`, or live `source` as a startup\n\
		 ritual. Use them only when the task specifically needs that information.",
	);

	let reads = shipped_phrase(
		registry,
		&[("tree", "tree"), ("ls", "ls"), ("meta", "meta"), ("get", "get --prop")],
		"or",
	)
	.map(|phrase| format!(" Use {phrase} when you need to inspect Studio-owned objects."))
	.unwrap_or_default();

	let source = if registry.is_shipped("source") {
		" For code you are about to edit, read the local synced file directly; use live \
		 `wsync source` only when checking a suspected Studio/editor divergence."
	} else {
		" For code you are about to edit, read the local synced file directly."
	};

	doc.block(format!(
		"The live Studio explorer is the source of truth for Explorer shape.{reads}{source} \
		 Disk mirrors only what this project's tree and sync rules map — instances \
		 outside it, excluded by `ignoreGlobs`, or inside an `AvoidSync` subtree exist \
		 in Studio only and have no file."
	));

	doc.block(build_line(env));
}

fn build_line(env: &EnvFacts) -> String {
	let plugin = match &env.plugin_version {
		Some(version) => format!(", Studio plugin {version}"),
		None => String::new(),
	};

	let connection = if env.plugin_connected {
		"A Studio plugin was connected when these docs were generated"
	} else {
		"No Studio plugin was connected when these docs were generated"
	};

	format!(
		"Generated for WSync {} (protocol {}{plugin}). {connection}; check `wsync status\n\
		 --raw` for the current state before assuming a live command will answer.",
		env.cli_version, env.protocol_version
	)
}

fn refresh(doc: &mut Doc, registry: &RegistryFacts) {
	doc.block("## 0b. Refreshing agent docs");

	doc.block(
		"These docs regenerate automatically every time the Studio plugin connects to\n\
		 the project daemon. To force a refresh without a connection, run:",
	);

	if registry.is_shipped("refresh") {
		doc.code(&[String::from("wsync refresh --project .")]);
	} else {
		doc.block(
			"`wsync refresh` is in the command registry but not in this build yet; until\n\
			 it lands, the docs refresh on plugin connect only.",
		);
	}

	doc.block(
		"A refresh rewrites `wsync.md`, `AGENTS.md`, `CLAUDE.md`, and\n\
		 `.codex/config.toml` without discarding project notes. Everything outside the\n\
		 WSync marker blocks is preserved byte for byte, and marker blocks owned by\n\
		 other tools are left untouched. Keep durable Codex notes in `AGENTS.md`\n\
		 outside the marker block; keep Claude-specific notes in `CLAUDE.md` around the\n\
		 `@AGENTS.md` import. `wsync.md` is the generated tool reference — anything\n\
		 written inside its marker block is overwritten on the next refresh.",
	);
}

fn what_syncs(doc: &mut Doc, input: &DocsInput) {
	let (project, registry) = (input.project, input.registry);

	doc.block("## 1. What syncs, what doesn't");

	doc.block(
		"WSync syncs whatever this project maps — not a fixed list of classes. A file\n\
		 syncs when three things hold:",
	);

	doc.block(format!(
		"1. It sits under one of the synced roots in §3.\n\
		 2. A sync rule in §2 matches its name.\n\
		 3. Nothing excludes it: `ignoreGlobs`, the `syncback` filters in `{}`, or an\n\
		 \x20  `AvoidSync = true` attribute on the instance or an ancestor.",
		project.project_file
	));

	let reads = shipped_phrase(
		registry,
		&[
			("tree", "tree"),
			("ls", "ls"),
			("meta", "meta"),
			("get", "get"),
			("props", "props"),
		],
		"and",
	)
	.map(|phrase| format!(" ({phrase})"))
	.unwrap_or_default();

	doc.block(format!(
		"Two-way sync is not limited to scripts. Every file form in §2 round-trips, and \
		 that table is generated from the rules this project actually runs — so it is \
		 the complete list, whatever it contains. Instances the project does not map are \
		 Studio-authoritative: inspect them with live reads{reads}. They are real — they \
		 just have no file. The live tree is a superset of the disk projection by design, \
		 and live commands reach the whole DataModel."
	));

	doc.block(exclusions(project));

	let diagnose = if registry.is_shipped("source") {
		" For normal code work, read the local synced file directly; use live \
		 `wsync source` only as a loose diagnostic when you suspect Studio/editor text \
		 has diverged from disk."
	} else {
		" For normal code work, read the local synced file directly."
	};

	doc.block(format!(
		"Script source has one extra Studio caveat: `script.Source` is not a reliable \
		 truth source while Drafts or an open Script Editor buffer is involved. Studio \
		 does not always push draft/editor text into the `Source` property until the \
		 script is committed. WSync reads and writes editor text through \
		 `ScriptEditorService` and ScriptDocument change events so editor state can \
		 round-trip.{diagnose}"
	));

	doc.block("**When both sides change the same thing.**");

	let mut drift = Vec::new();

	if project.conflict_engine {
		let resolve = registry
			.pick(&["conflicts", "resolve"])
			.iter()
			.map(|command| format!("`wsync {}`", command.name))
			.collect::<Vec<String>>()
			.join(" / ");

		let resolve = if resolve.is_empty() {
			String::from("the desktop app's Conflicts view")
		} else {
			format!("the desktop app's Conflicts view and {resolve}")
		};

		drift.push(format!(
			"- *While connected (item-level races).* The conflict engine parks the racing\n\
			 edit instead of letting one side silently win. Parked conflicts surface in\n\
			 {resolve}. Nothing is applied until someone chooses a side, so a parked\n\
			 conflict is a decision to make, not an error to retry."
		));
	} else {
		drift.push(String::from(
			"- *While connected (item-level races).* The conflict engine is disabled for\n\
			 this workspace (`conflictEngine = false`), so a race resolves\n\
			 last-writer-wins with no parking. Be careful about editing a file that is\n\
			 also being edited in Studio.",
		));
	}

	if project.scope.is_code() {
		// Studio-first (Design §7.0): the connect applies immediately and the
		// leftover is a passive review, not a blocking question
		let cli = match (registry.is_shipped("decision"), registry.is_shipped("diff")) {
			(true, true) => {
				"`wsync diff` lists the pending review, and `wsync decision` acts on it \
				 (`--disk` pushes everything back, `--cancel` dismisses)."
			}
			(true, false) => {
				"`wsync decision` shows and acts on the pending review (`--disk` pushes \
				 everything back, `--cancel` dismisses)."
			}
			(false, true) => "`wsync diff` lists the pending review.",
			(false, false) => "the desktop app lists the pending review.",
		};

		drift.push(format!(
			"- *At connect time (set-level drift).* Studio wins automatically: the daemon\n\
			 applies Studio to disk (fenced, backed up) the moment the comparison\n\
			 commits — no prompt, no waiting page. What stays behind is a passive disk\n\
			 review: disk-only files left untouched on disk and `differs` files whose\n\
			 disk original was preserved. {cli} Nothing blocks on the review — live sync\n\
			 is already running; never push or dismiss it on the user's behalf."
		));
	} else {
		// `decision` answers the choice; `diff` only lists it. Never conflate
		// them. Full scope keeps the pre-ruling decision modal
		let cli = match (registry.is_shipped("decision"), registry.is_shipped("diff")) {
			(true, true) => {
				"`wsync diff` lists the pending review, `wsync decision` reports it, and \
				 the divergence choice itself is answered from the desktop app."
			}
			_ => "the divergence choice is answered from the desktop app.",
		};

		drift.push(format!(
			"- *At connect time (set-level drift).* When Studio and disk both moved while\n\
			 disconnected, WSync freezes an immutable divergence set and asks once. The\n\
			 desktop app raises the overwrite modal — Keep Studio or Keep Disk, with\n\
			 per-file staging — and {cli} Until it is answered the connection stays up but\n\
			 unsynced; never answer it on the user's behalf."
		));
	}

	doc.block(drift.join("\n"));
}

fn exclusions(project: &ProjectFacts) -> String {
	let mut lines = Vec::new();

	if !project.ignore_globs.is_empty() {
		lines.push(format!(
			"- `ignoreGlobs`: {}",
			project
				.ignore_globs
				.iter()
				.map(|glob| format!("`{glob}`"))
				.collect::<Vec<String>>()
				.join(", ")
		));
	}

	if !project.syncback_ignore_globs.is_empty() {
		lines.push(format!(
			"- `syncback.ignoreGlobs`: {}",
			project
				.syncback_ignore_globs
				.iter()
				.map(|glob| format!("`{glob}`"))
				.collect::<Vec<String>>()
				.join(", ")
		));
	}

	if !project.syncback_ignore_names.is_empty() {
		lines.push(format!(
			"- `syncback.ignoreNames`: {}",
			project
				.syncback_ignore_names
				.iter()
				.map(|name| format!("`{name}`"))
				.collect::<Vec<String>>()
				.join(", ")
		));
	}

	if !project.syncback_ignore_classes.is_empty() {
		lines.push(format!(
			"- `syncback.ignoreClasses`: {}",
			project
				.syncback_ignore_classes
				.iter()
				.map(|class| format!("`{class}`"))
				.collect::<Vec<String>>()
				.join(", ")
		));
	}

	if lines.is_empty() {
		return String::from(
			"This project configures no `ignoreGlobs` and no `syncback` filters, so the\n\
			 only exclusions in force are the `AvoidSync` attribute and anything no sync\n\
			 rule matches.",
		);
	}

	format!("Exclusions configured for this project:\n\n{}", lines.join("\n"))
}

fn playtesting(doc: &mut Doc) {
	doc.blocks([
		"## 1b. Playtesting is a separate environment",
		"Roblox Studio playtesting creates a completely separate DataModel clone. The\n\
		 Play/Solo/Run world and the edit-mode Studio workspace do not transfer\n\
		 instance or script changes between each other. Script edits made while\n\
		 playtesting run inside that temporary playtest DataModel and DO NOT mirror\n\
		 back into the edit DataModel. WSync is connected to the edit DataModel and\n\
		 this directory, not the playtest clone.",
		"If you change code while a playtest is running, make the durable edit in this\n\
		 directory or in the non-playtest Studio edit view. Do not assume a script\n\
		 change made during Play/Solo/Run has synced just because it worked in the\n\
		 playtest.",
	]);
}

fn conventions(doc: &mut Doc, input: &DocsInput) {
	let (project, registry) = (input.project, input.registry);

	doc.block("## 2. Filesystem conventions");

	if project.custom_sync_rules {
		doc.block(format!(
			"**This project defines its own `syncRules`.** Project rules replace the\n\
			 built-in set entirely, so the tables below are the complete file-form\n\
			 vocabulary for this project — a file shape that is not listed has no\n\
			 instance form here. The tables are generated from `{}`.",
			project.project_file
		));
	} else {
		doc.block(
			"These tables are generated from the sync rules actually in effect (the\n\
			 built-in defaults; this project overrides none of them). First match wins.",
		);
	}

	doc.block(
		"**Directories**\n\
		 \n\
		 | On disk | Roblox instance |\n\
		 | --- | --- |\n\
		 | `Foo/` | `Folder` named `Foo`, unless it holds an `init` file below |",
	);

	for group in [
		RuleGroup::Scripts,
		RuleGroup::Values,
		RuleGroup::Modules,
		RuleGroup::Models,
		RuleGroup::Meta,
	] {
		if let Some(table) = conventions_table(project, group) {
			doc.block(table);
		}
	}

	let init_forms = init_forms(project);

	if !init_forms.is_empty() {
		doc.block(
			"An instance that has both source and children is a directory plus one\n\
			 matching `init` file. Edit the init file for the instance's own source; edit\n\
			 the sibling entries for its children:",
		);

		doc.block(init_forms.join("\n"));
	}

	doc.block(script_flavor_note(project));

	let mut rules = vec![
		String::from(
			"- The synced roots in §3 are the only valid entry points. Arbitrary folders in\n\
			 \x20 the project root are not children of `game`; they are simply unmapped.",
		),
		String::from(
			"- Empty plain directories are ignored until they contain something syncable,\n\
			 \x20 so a placeholder folder cannot shadow a same-named script.",
		),
		String::from(
			"- File and directory renames/moves sync as Roblox renames and reparents while\n\
			 \x20 they stay under a mapped root.",
		),
	];

	if let Some(pattern) = project.metadata_pattern() {
		rules.push(format!(
			"- `{pattern}` sidecars carry `$className`, properties, attributes, tags and\n\
			 \x20 `originalName` for the instance they sit next to. Property changes made\n\
			 \x20 there do reach Studio — but a property on an instance with no file form\n\
			 \x20 cannot be set from disk at all."
		));
	} else {
		rules.push(String::from(
			"- The rules in effect define no metadata sidecar, so properties, attributes\n\
			 \x20 and tags have no file form in this project at all. Change them in Studio,\n\
			 \x20 or with `wsync set` and the user's consent.",
		));
	}

	let boundaries = shipped_phrase(registry, &[("tree", "tree"), ("meta", "meta")], "or")
		.map(|phrase| format!(" Use {phrase} to find the boundaries."))
		.unwrap_or_default();

	rules.push(format!(
		"- Set a boolean Studio attribute `AvoidSync = true` on any instance to exclude \
		 that subtree from sync in both directions, including divergence comparison. It \
		 is the Studio-side counterpart to `ignoreGlobs` and needs no file edit.{boundaries}"
	));

	rules.push(String::from(
		"- The runtime directories `.wsync-backups/`, `.wsync-artifacts/` and\n\
		 \x20 `.wsync-workflows/` are never synced.",
	));

	doc.block(format!("Additional rules in force here:\n\n{}", rules.join("\n")));

	doc.block(name_safety_note(project));
}

fn script_flavor_note(project: &ProjectFacts) -> String {
	let mut notes = Vec::new();

	// Only meaningful when the rules in effect actually distinguish server and
	// client scripts
	if project.has_middleware(&Middleware::ServerScript) || project.has_middleware(&Middleware::ClientScript) {
		notes.push(
			"Script class and RunContext are encoded in the file suffix: `.server` is a\n\
			 `Script` with `RunContext = Legacy`, `.runserver` a `Script` with\n\
			 `RunContext = Server`, `.client` a `Script` with `RunContext = Client`, and\n\
			 `.local` a `LocalScript`.",
		);
	}

	notes.push(match (project.rojo_mode, project.has_legacy_forms()) {
		(true, true) => {
			"`rojoMode` is on, so syncback writes the Rojo-shaped forms: `init.*` files\n\
			 and `.meta.json` sidecars. The Argon-legacy `.src.*` and `.data.json` forms\n\
			 are still *read* where they exist, they are just never written."
		}
		(true, false) => {
			"`rojoMode` is on. The rules in effect define no Argon-legacy `.src.*` or\n\
			 `.data.json` forms, so syncback writes exactly the shapes in the tables\n\
			 above."
		}
		(false, true) => {
			"`rojoMode` is off, so syncback writes the Argon-legacy forms: `.src.*` init\n\
			 files and `.data.json` sidecars. The Rojo `init.*` and `.meta.json` forms are\n\
			 still read."
		}
		(false, false) => {
			"`rojoMode` is off, but the rules in effect define no Argon-legacy forms, so\n\
			 syncback writes exactly the shapes in the tables above."
		}
	});

	if project.lua_extension && project.has_extension(".lua") {
		notes.push("`luaExtension` is on: new script files are written with the `.lua` form.");
	} else if project.has_extension(".luau") && project.has_extension(".lua") {
		notes.push("New script files use the `.luau` form; `.lua` files are read as well.");
	}

	notes.join("\n\n")
}

fn name_safety_note(project: &ProjectFacts) -> String {
	let rename = if project.rename_instances {
		"Studio names a path cannot express are **not** percent-encoded.\n\
		 `renameInstances` is on, so WSync writes a safe file name and records the\n\
		 real Studio name as `originalName` in the instance's sidecar, which is what\n\
		 makes the name round-trip. `/` and control characters are rejected\n\
		 everywhere; on Windows `< > : \" / \\ | ? *` and the reserved device names\n\
		 are rejected too."
	} else {
		"Studio names a path cannot express are **not** percent-encoded.\n\
		 `renameInstances` is off for this workspace, so an instance whose name a\n\
		 path cannot express is skipped with an error instead of being renamed. `/`\n\
		 and control characters are rejected everywhere; on Windows\n\
		 `< > : \" / \\ | ? *` and the reserved device names are rejected too."
	};

	let duplicates = if project.keep_duplicates {
		"`keepDuplicates` is on: same-named siblings are kept by suffixing the file\n\
		 name, with the shared Studio name preserved in `originalName`."
	} else {
		"`keepDuplicates` is off: when two siblings would land on the same path the\n\
		 second is skipped with an error rather than silently overwriting the first."
	};

	format!("{rename}\n\n{duplicates}")
}

fn synced_roots(doc: &mut Doc, project: &ProjectFacts) {
	doc.block("## 3. Synced roots");

	if project.roots.is_empty() {
		doc.block(format!(
			"`{}` maps no `$path` yet, so nothing in this directory syncs. Add a mapping\n\
			 to the project tree before expecting files to appear in Studio.",
			project.project_file
		));

		return;
	}

	if project.is_place {
		doc.block(format!(
			"The project root mirrors the `game` DataModel. Every entry below is a\n\
			 `$path` mapping in `{}` — this list is the whole projection, and it is\n\
			 generated from the project tree rather than assumed:",
			project.project_file
		));
	} else {
		doc.block(format!(
			"This project is not a place: its tree maps a model/plugin/package root\n\
			 rather than `game`. Every entry below is a `$path` mapping in `{}`:",
			project.project_file
		));
	}

	let mut lines = Vec::new();

	for root in &project.roots {
		let mut line = format!("- `{}` → `{}`", root.label(), root.fs_path);

		if let Some(class) = &root.class_name {
			let _ = write!(line, " (`$className: {class}`)");
		}

		if root.optional {
			line.push_str(" — optional; absent on disk is not an error");
		}

		lines.push(line);
	}

	doc.block(lines.join("\n"));

	doc.block(
		"Anything outside these roots is Studio-only. That is not drift and not a bug:\n\
		 it is the projection working as configured.",
	);
}

fn generated_files(doc: &mut Doc, registry: &RegistryFacts) {
	doc.block("## 4. Generated files (do not edit inside the markers)");

	doc.block(
		"- `wsync.md` — this file, the generated WSync tool reference.\n\
		 - `AGENTS.md` — the same reference embedded for Codex and other agent\n\
		 \x20 runners, inside `<!-- wsync:agent-context:start/end -->`.\n\
		 - `CLAUDE.md` — an `@AGENTS.md` import inside\n\
		 \x20 `<!-- wsync:agents-include:start/end -->`.\n\
		 - `.codex/config.toml` — `project_doc_fallback_filenames` so Codex finds the\n\
		 \x20 docs; every other key in that file is left alone.",
	);

	let refresh = if registry.is_shipped("refresh") {
		"regenerated on plugin connect and by `wsync refresh`"
	} else {
		"regenerated on plugin connect"
	};

	doc.block(format!(
		"They are {refresh}. Notes outside the marker blocks survive; notes inside are\n\
		 replaced. None of these files is a syncable instance — they have no place in\n\
		 the DataModel."
	));

	doc.block(
		"WSync also keeps gitignored runtime directories in the project root when it\n\
		 needs them: `.wsync-backups/` (pre-overwrite copies), `.wsync-artifacts/`, and\n\
		 `.wsync-workflows/` (workflow idempotency records).",
	);
}

fn querying(doc: &mut Doc, input: &DocsInput) {
	let registry = input.registry;

	doc.block("## 5. Querying the live tree");

	if registry.is_shipped("query") {
		doc.block(
			"`wsync query` asks the running daemon and plugin for the live Studio tree and\n\
			 matches a `/`-separated selector against the DataModel. `*` matches one\n\
			 segment (any name); `**` matches zero or more segments.",
		);
	}

	doc.code(&examples(registry, &["query", "path", "meta", "where"], 2));

	if let Some(translators) = shipped_phrase(registry, &[("path", "path"), ("meta", "meta")], "and") {
		let finder = shipped_phrase(registry, &[("where", "where")], "and")
			.map(|phrase| format!(" {phrase} finds live instances when you only know part of a name."))
			.unwrap_or_default();

		doc.block(format!(
			"{translators} translate between Studio instance paths and the syncable files \
			 on disk, through the live tree.{finder} Instances outside the projection — \
			 not under a mapped root, excluded by `ignoreGlobs`, or inside an `AvoidSync` \
			 subtree — are reported as Studio-only instead of being given an invented path."
		));
	}

	if let Some(note) = pending_note(
		registry,
		&["query", "path", "meta", "where"],
		"Use `wsync tree` and `wsync ls` for shape until they land.",
	) {
		doc.block(note);
	}
}

fn linting(doc: &mut Doc, registry: &RegistryFacts) {
	doc.block("## 5b. Linting Luau");

	if !registry.is_shipped("lint") {
		doc.block(
			"`wsync lint` — `luau-lsp analyze` against a generated WSync sourcemap, with\n\
			 selectable live-Studio DataModel coverage and a `luau-compile` pass — is\n\
			 specified in the command registry but **lands in a later build**. It is not\n\
			 available here yet.",
		);

		doc.block(
			"Until it ships, verify edited scripts by reading the file and running\n\
			 whatever linter this repository already configures (`selene.toml` and a\n\
			 `luau-lsp` setup are common). Do not install or invoke an unrelated Roblox\n\
			 toolchain to fill the gap without asking the user first.",
		);

		return;
	}

	doc.block(
		"`wsync lint` runs `luau-lsp analyze` with current Roblox definitions and a\n\
		 temporary WSync sourcemap. The default `--data-model auto` merges the live\n\
		 Studio tree and enables strict DataModel diagnostics when the matching daemon\n\
		 and plugin are connected; offline it reports a relaxed fallback. Use\n\
		 `--data-model studio` to require live strict coverage, `filesystem` for a\n\
		 strict disk-only audit (which can flag Studio-only children), or `loose` for\n\
		 gradual disk types. `--raw` returns structured diagnostics and coverage\n\
		 metadata.",
	);

	doc.code(&examples(registry, &["lint"], 4));
}

fn uploads(doc: &mut Doc, project: &ProjectFacts, registry: &RegistryFacts) {
	doc.block("## 5c. Asset uploads and monetization");

	if !registry.is_shipped("upload") && !registry.is_shipped("monetization") {
		doc.block(
			"`wsync upload` (Roblox Open Cloud asset upload) and `wsync monetization`\n\
			 (game passes and developer products) are specified in the command registry\n\
			 but **land in a later build**. Neither is available here yet.",
		);

		doc.block(
			"Do not reach for `rbxcloud` or an ad-hoc upload script in the meantime, and\n\
			 do not add an Open Cloud dependency on your own initiative — ask the user how\n\
			 they want assets uploaded.",
		);

		return;
	}

	doc.block(
		"`wsync upload` uploads assets through Roblox Open Cloud. It needs neither the\n\
		 daemon nor Studio. The API key is read from WSync's credential store first;\n\
		 `--api-key-env` is an explicit override, and the CLI otherwise falls back to\n\
		 `ROBLOX_API_KEY`, `CLOUD_API_KEY`, and `ROBLOX_OPEN_CLOUD_API_KEY`. Never pass\n\
		 a key on the command line.",
	);

	if let Some(group_id) = project.group_id {
		doc.block(format!(
			"With `--creator` omitted, this project's `groupId` ({group_id}) is used as\n\
			 `group:{group_id}`."
		));
	}

	doc.code(&examples(registry, &["upload", "monetization"], 3));
}

fn live_control(doc: &mut Doc, input: &DocsInput) {
	let (registry, env) = (input.registry, input.env);

	doc.block("## 6. Agent usage — live Studio control");

	doc.block(format!(
		"When the daemon is running and the user has WSync connected to Studio, these\n\
		 subcommands speak to the plugin over WebSocket and inspect or mutate live\n\
		 instances. They work across the entire DataModel — not just what this project\n\
		 projects to disk. Every call that mutates state is appended to `writes.log` in\n\
		 WSync's platform state directory (`{}`).",
		env.writes_log
	));

	doc.block(
		"Treat these live reads as authoritative when deciding what exists in Studio.\n\
		 The filesystem view is intentionally narrower and can omit Studio-only\n\
		 folders, Models, Parts, UI objects, Remotes, and anything else the project\n\
		 does not map.",
	);

	doc.block(
		"Every subcommand accepts `--project <path>`; defaults are not inferred. All\n\
		 instance paths are `/`-separated Studio names rooted at the DataModel — e.g.\n\
		 `Workspace/Camera`, `ReplicatedStorage/Shared/Module`.",
	);

	let read_only = registry.live_tier(Safety::ReadOnly);
	let mutating = registry.live_tier(Safety::Mutating);

	if !read_only.is_empty() {
		doc.block("**Read-only (safe to use unattended):**");
		doc.block(summary_list(&read_only).join("\n"));
		doc.code(&examples(registry, &["get", "ls", "tree", "find"], 1));
	}

	if !mutating.is_empty() {
		doc.block("**Mutating (ask the user first — see §7):**");
		doc.block(summary_list(&mutating).join("\n"));
		doc.code(&examples(registry, &["set", "new", "eval"], 1));
	}

	let guardrails = if registry.is_shipped("set") {
		let reparent = if registry.is_shipped("mv") {
			" — use `wsync mv`"
		} else {
			""
		};

		format!(
			" Two guardrails apply to every write path and are described in full in §6i: \
			 bracket batches with `--waypoint <name>` so one Studio undo reverses them, \
			 and never set `Parent` with `wsync set`{reparent}."
		)
	} else {
		String::new()
	};

	doc.block(format!(
		"All of these time out after 5 seconds if the plugin does not respond; a \
		 non-zero exit code means the request never completed.{guardrails}"
	));
}

fn history_and_health(doc: &mut Doc, registry: &RegistryFacts) {
	let commands = ["status", "doctor", "ping", "version", "logs", "tail", "save"];
	let history = ["waypoint", "undo", "redo"];

	if registry.pick(&commands).is_empty() && registry.pick(&history).is_empty() {
		return;
	}

	doc.block("## 6b. Handshake, health, logs, and change history");

	doc.block(
		"These bracket batches, roll state back, capture output, and verify the plugin\n\
		 is reachable.",
	);

	doc.code(&examples(registry, &commands, 1));

	if !registry.pick(&history).is_empty() {
		doc.block(
			"Change history: one waypoint flanking a batch means a single ctrl-Z in\n\
			 Studio reverses the whole batch. `undo` and `redo` also work from the CLI,\n\
			 and they drive the user's real undo stack — say what you are about to undo\n\
			 before you undo it.",
		);

		doc.code(&examples(registry, &history, 1));
	}
}

fn structured_writes(doc: &mut Doc, input: &DocsInput) {
	let (registry, env) = (input.registry, input.env);
	let commands = ["new", "rm", "mv", "attr", "tag", "call", "select"];

	if registry.pick(&commands).is_empty() {
		return;
	}

	doc.block("## 6c. Structured writes beyond `set` and `eval`");

	let reparent = if registry.is_shipped("mv") {
		" `wsync mv` refuses to cross a top-level service boundary without `--force`, \
		 which catches mistakes like punting something from `Workspace` into \
		 `ServerStorage`."
	} else {
		""
	};

	doc.block(format!(
		"Live-DataModel operations beyond `set` and `eval`. Each write is appended to \
		 `{}`.{reparent}",
		env.writes_log
	));

	doc.code(&examples(registry, &commands, 2));
}

fn clipboard(doc: &mut Doc, registry: &RegistryFacts) {
	let commands = ["copy", "paste"];

	if registry.pick(&commands).is_empty() {
		return;
	}

	doc.block("## 6d. Cross-project Studio clipboard");

	doc.block(
		"`wsync copy` and `wsync paste` move native Roblox instance trees between\n\
		 simultaneously connected projects. The payload is real `.rbxm` produced by\n\
		 Studio's serializer, not a lossy JSON projection, so classes, properties,\n\
		 descendants, attributes, tags, scripts, and references among the copied roots\n\
		 all survive. The clipboard lives in WSync's private state directory: copy in\n\
		 one project, `cd` to another, paste there. Paste is one Studio undo.",
	);

	doc.code(&examples(registry, &commands, 2));
}

fn introspection(doc: &mut Doc, registry: &RegistryFacts) {
	let commands = ["classinfo", "enums", "enum", "find", "find-attr"];

	if registry.pick(&commands).is_empty() {
		return;
	}

	doc.block("## 6e. Introspection — class info, enums, attribute-scoped search");

	doc.block(
		"Read-only helpers for mapping your mental model of the DataModel onto\n\
		 Studio's real type system. Cheap; safe to call freely.",
	);

	doc.code(&examples(registry, &commands, 1));
}

fn agent_runtime(doc: &mut Doc, registry: &RegistryFacts) {
	let commands = ["capture", "playtest", "run", "capabilities", "transmit"];
	let shipped = registry.pick(&commands);
	let pending = pending_note(
		registry,
		&commands,
		"Do not emulate them with `eval` or ad-hoc scripts; tell the user the command \
		 is not built yet.",
	);

	if shipped.is_empty() && pending.is_none() {
		return;
	}

	doc.block("## 6f. Captures, playtests, and workflows");

	if !shipped.is_empty() {
		doc.block(summary_list(&shipped).join("\n"));
		doc.code(&examples(registry, &commands, 1));

		if registry.is_shipped("playtest") || registry.is_shipped("run") {
			doc.block(
				"Playtest runs and workflow runs are user-intent-gated: a workflow does not\n\
				 grant write authority. Inspect the live targets and confirm intent before\n\
				 running one that mutates Studio, starts a test, sends input, or uploads.\n\
				 Runtime changes made during a playtest are temporary and never sync to disk.",
			);
		}
	}

	if let Some(note) = pending {
		doc.block(note);
	}
}

fn command_budget(doc: &mut Doc, input: &DocsInput) {
	let (registry, env) = (input.registry, input.env);

	doc.block("## 6i. LLM-first command budget");

	doc.block(
		"Do not paste or request the full command registry by default. It is large and\n\
		 usually worse for agent reasoning. Use this flow instead:",
	);

	// The numbered flow is normative (Design §10.6), but a step must never
	// instruct an agent to run a command this build does not have
	let mut steps: Vec<String> = Vec::new();

	if registry.is_shipped("context") {
		steps.push(String::from(
			"Run `wsync context --project .` once, and only when you need project\n\
			 context that is not already in `AGENTS.md`.",
		));
	} else {
		steps.push(String::from(
			"Read `AGENTS.md` for project context. `wsync context` is not in this build\n\
			 yet, so do not go hunting for an equivalent.",
		));
	}

	steps.push(String::from(
		"Prefer local file reads and cheap offline commands for normal code work.",
	));
	if let Some(reads) = shipped_phrase(
		registry,
		&[("tree", "tree"), ("ls", "ls"), ("meta", "meta"), ("get", "get --prop")],
		"or",
	) {
		steps.push(format!(
			"For Explorer shape or Studio-owned objects, use focused live reads: {reads}."
		));
	}

	if registry.is_shipped("commands") {
		steps.push(String::from(
			"Use `wsync commands --compact` only when choosing between command families.",
		));
		steps.push(String::from(
			"Run `wsync commands <name>` for exact flags, only for the command you are\n\
			 about to use.",
		));
	}

	steps.push(String::from(
		"Prefer cheap offline commands for path lookup, but never let disk-only\n\
		 inference override a live Studio read.",
	));

	let plan = if registry.is_shipped("plan") {
		" Use `wsync plan` when a dry-run explanation is useful; it is not a mandatory ritual."
	} else {
		""
	};

	steps.push(format!(
		"Before mutating Studio from an LLM workflow, inspect the exact live target\n\
		 with focused read-only commands and confirm explicit user intent.{plan}"
	));

	let steps: Vec<String> = steps
		.iter()
		.enumerate()
		.map(|(index, step)| format!("{}. {step}", index + 1))
		.collect();

	doc.block(steps.join("\n"));

	let mut special: Vec<&str> = Vec::new();

	if registry.is_shipped("source") {
		special.push(
			"- `wsync source` is a loose diagnostic for suspected Studio/editor divergence.\n\
			 For ordinary code inspection and verification, read the local file and lint\n\
			 it instead.",
		);
	}

	if registry.is_shipped("conflicts") {
		special.push(
			"- `wsync conflicts` is for resolving an observed conflict. Do not poll it as a\n\
			 general health check, and do not block normal edits on it.",
		);
	}

	if registry.is_shipped("changes") || registry.is_shipped("diff") {
		special.push(
			"- `wsync changes` / `wsync diff` can be noisy on large or already-drifty\n\
			 projects. Prefer targeted verification after focused code edits.",
		);
	}

	if registry.is_shipped("snapshot") {
		special.push("- `wsync snapshot` is a backup/debug tool. It is never a read step.");
	}

	if !special.is_empty() {
		doc.block("**Special-case commands.**");
		doc.block(special.join("\n"));
	}

	let cheap = examples(registry, &["context", "status", "query", "path", "meta", "services"], 1);

	if !cheap.is_empty() {
		doc.block("**Cheap-first discovery:**");
		doc.code(&cheap);
	}

	let targeted = examples(registry, &["tree", "ls", "get", "props"], 1);

	if !targeted.is_empty() {
		doc.block("**Targeted reads:**");
		doc.code(&targeted);
	}

	if registry.is_shipped("source") {
		doc.block(
			"`wsync source` without `--disk` asks the live plugin for Studio/editor text\n\
			 and goes through `ScriptEditorService` for script source. Treat it as an\n\
			 optional divergence debug tool, not a default verification step. Prefer\n\
			 direct local file reads for the file that lint and Git actually see.",
		);
	}

	let drift_tools = shipped_phrase(
		registry,
		&[
			("source", "source"),
			("conflicts", "conflicts"),
			("changes", "changes"),
			("diff", "diff"),
		],
		"or",
	)
	.map(|phrase| {
		format!(
			" Use {phrase} only when the task specifically points at divergence, a \
			 reported conflict, or sync drift."
		)
	})
	.unwrap_or_default();

	doc.block(format!(
		"For post-edit verification, do not treat an unrelated global divergence \
		 listing as proof that your touched file failed to sync. A WSync project can \
		 have pre-existing Studio-only instances, duplicate-name siblings, or ignored \
		 tooling under other paths. For a normal script edit the preferred verification \
		 is a local file read plus the narrowest relevant lint.{drift_tools}"
	));

	let heavy = examples(registry, &["changes", "find", "logs"], 1);

	if !heavy.is_empty() {
		doc.block("**Higher-token reads; use only when the task needs them:**");
		doc.code(&heavy);
	}

	let backup = examples(registry, &["snapshot"], 1);

	if !backup.is_empty() {
		doc.block("**Backup/debug only:**");
		doc.code(&backup);
	}

	if registry.is_shipped("commands") {
		doc.block(
			"Use plain `wsync commands` only when the user explicitly needs the full\n\
			 machine-readable registry.",
		);
	}

	// Each recipe is dropped when the command it turns on is not built yet
	let mut snippets: Vec<String> = Vec::new();

	let inspect = shipped_phrase(
		registry,
		&[("meta", "meta"), ("get", "get --prop"), ("props", "props")],
		"or",
	);

	if let Some(inspect) = inspect {
		snippets.push(format!(
			"- Inspect one object: {inspect}; use local files for script source."
		));
	}

	let mapping = shipped_phrase(registry, &[("where", "where"), ("query", "query")], "or")
		.map(|phrase| format!(" use {phrase} when mapping Studio names onto files."))
		.unwrap_or_default();

	snippets.push(format!("- Find code: `rg` and local file reads first;{mapping}"));
	snippets.push(String::from(
		"- Verify touched scripts: local read plus the narrowest relevant lint.",
	));

	if registry.is_shipped("conflicts") && registry.is_shipped("resolve") {
		snippets.push(String::from(
			"- Resolve conflict: only after a conflict is reported — `wsync conflicts` →\n\
			 explicit `wsync resolve`.",
		));
	}

	if registry.is_shipped("decision") {
		snippets.push(String::from(
			"- Answer a divergence choice: read the set → ask the user → `wsync decision\n\
			 --disk` or `--studio`.",
		));
	}

	snippets.push(String::from(
		"- Write Studio: inspect the live target → user confirmation → the mutating\n\
		 command, with a waypoint for batches.",
	));

	if registry.is_shipped("upload") || registry.is_shipped("monetization") {
		snippets.push(String::from(
			"- Upload/Open Cloud: enumerate the files first; never start a recursive or\n\
			 bulk upload before the target set is clear.",
		));
	}

	doc.block("**Preferred workflow snippets.**");
	doc.block(snippets.join("\n"));

	if registry.is_shipped("set") {
		doc.block("**Two write-path flags every agent should know.**");

		doc.block(
			"- **`--waypoint <name>`** on `set` (single or `--batch`) records a named Studio\n\
			 \x20 change-history waypoint around the operation, so one ctrl-Z in the editor\n\
			 \x20 reverts the whole thing. Use it for any multi-step write:\n\
			 \x20 `wsync set --project . --batch edits.json --waypoint \"re-skin box\"`.\n\
			 - **`set Parent` is guardrailed.** `wsync set --prop Parent …` refuses with a\n\
			 \x20 loud error by default — raw `Parent` assignment is the single most common\n\
			 \x20 way to corrupt a DataModel. Use `wsync mv --from X --to Y` to reparent. If\n\
			 \x20 you genuinely need the raw write, pass `--force-parent` explicitly. A\n\
			 \x20 `--batch` entry that sets `Parent` rejects the whole batch before any\n\
			 \x20 request is made, and there is no batch-level escape.",
		);
	}

	doc.block(format!(
		"The audit log auto-rotates once it passes 10 MiB: `writes.log` is renamed to\n\
		 `writes.log.1` in the same state directory (overwriting any prior generation)\n\
		 and a fresh `writes.log` takes its place. Only one prior generation is kept.\n\
		 The live path is `{}`.",
		env.writes_log
	));

	doc.block(
		"Any explicit force-overwrite or prune path copies the removed tree to\n\
		 `.wsync-backups/<timestamp>/` before deleting anything. That directory is\n\
		 ignored by sync and by Git; completed transfers are pruned after 7 days or 32\n\
		 transactions, and unproven backups are kept indefinitely. Remove old backups\n\
		 only after confirming the retained place is the one the user wants.",
	);
}

fn safety(doc: &mut Doc, input: &DocsInput) {
	let (registry, env) = (input.registry, input.env);

	doc.block("## 7. Safety note");

	let mutating: Vec<&str> = registry
		.live_tier(Safety::Mutating)
		.iter()
		.map(|command| command.name.as_str())
		.collect();

	let list = if mutating.is_empty() {
		String::from("Live mutating commands")
	} else {
		join_commands(&mutating)
	};

	doc.block(format!(
		"The filesystem projection covers what this project's tree and sync rules map\n\
		 (§2, §3). Everything else in the DataModel is reachable only through live\n\
		 commands, and the mutating ones — {list} — are **user-initiated escape\n\
		 hatches**, not automated tools. Never invoke them from a script or a plugin,\n\
		 and prefer asking the user before running them even at the CLI."
	));

	doc.block(format!(
		"Every successful write is appended to `writes.log` in WSync's platform state\n\
		 directory (`{}`), so the user can audit or replay anything an agent ran on\n\
		 their behalf. The state directory itself is `{}`.",
		env.writes_log, env.state_dir
	));

	doc.block(
		"Properties do round-trip through the filesystem here, via `.meta.json`\n\
		 sidecars on projected instances — but only for instances that have a file at\n\
		 all. A property on a Studio-only instance cannot be changed by editing files;\n\
		 that needs `wsync set`, with the user's consent.",
	);
}

////////////////////////////////////////////////////////////////////////////////
// Entry points
////////////////////////////////////////////////////////////////////////////////

/// Renders the body that lives inside the `wsync:project-memory` markers
pub fn project_memory(input: &DocsInput) -> String {
	let mut doc = Doc::default();

	intro(&mut doc, input.project);
	bootstrap(&mut doc, input);
	refresh(&mut doc, input.registry);
	what_syncs(&mut doc, input);
	playtesting(&mut doc);
	conventions(&mut doc, input);
	synced_roots(&mut doc, input.project);
	generated_files(&mut doc, input.registry);
	querying(&mut doc, input);
	linting(&mut doc, input.registry);
	uploads(&mut doc, input.project, input.registry);
	live_control(&mut doc, input);
	history_and_health(&mut doc, input.registry);
	structured_writes(&mut doc, input);
	clipboard(&mut doc, input.registry);
	introspection(&mut doc, input.registry);
	agent_runtime(&mut doc, input.registry);
	command_budget(&mut doc, input);
	safety(&mut doc, input);

	doc.finish()
}
