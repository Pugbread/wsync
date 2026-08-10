//! Generated agent docs (Design §10.6).

use serde_json::json;
use std::{fs, path::Path};
use tempfile::TempDir;

use wsync::{
	config::Config,
	docsgen::{
		self, markers, CodexSkip, DocFile, DocsInput, EnvFacts, GeneratedDocs, Preserve, ProjectFacts, RegistryFacts,
		Safety, Shipped, WriteStatus, AGENTS_INCLUDE, AGENT_CONTEXT, PROJECT_MEMORY,
	},
	project::Project,
};

////////////////////////////////////////////////////////////////////////////////
// Fixtures
////////////////////////////////////////////////////////////////////////////////

/// Trimmed stand-in for `docs/client-commands.generated.json`, built from a
/// compact table so the fixture stays readable
fn fixture_registry() -> RegistryFacts {
	let commands: [(&str, &str, &str, &[&str]); 33] = [
		(
			"context",
			"Command registry",
			"Prints a compact LLM-oriented project context snapshot as JSON.",
			&["wsync context --project ."],
		),
		(
			"commands",
			"Command registry",
			"Prints generated command docs as JSON.",
			&["wsync commands --compact"],
		),
		(
			"status",
			"Live diagnostics",
			"Summarizes daemon reachability, plugin handshake, and project config.",
			&["wsync status --project . --raw"],
		),
		(
			"doctor",
			"Live diagnostics",
			"Runs a broader health check over project files, daemon, and plugin.",
			&["wsync doctor --project ."],
		),
		(
			"ping",
			"Live diagnostics",
			"Round-trips a lightweight request to the Studio plugin.",
			&["wsync ping --project ."],
		),
		(
			"version",
			"Live diagnostics",
			"Prints the daemon version and build identity.",
			&["wsync version --project ."],
		),
		(
			"logs",
			"Live diagnostics",
			"Reads recent Studio output from the plugin log buffer.",
			&["wsync logs --project . --limit 50"],
		),
		(
			"tail",
			"Live diagnostics",
			"Streams Studio output until interrupted.",
			&["wsync tail --project ."],
		),
		(
			"get",
			"Live inspection",
			"Reads an instance view or one property from the live Studio session.",
			&["wsync get --project . --path Workspace/Camera --prop FieldOfView"],
		),
		(
			"ls",
			"Live inspection",
			"Lists the direct children of a live Studio instance.",
			&["wsync ls --project . --path ReplicatedStorage"],
		),
		(
			"tree",
			"Live inspection",
			"Prints a class and name tree below a live Studio instance.",
			&["wsync tree --project . --path Workspace --depth 3"],
		),
		(
			"props",
			"Live inspection",
			"Prints inspectable live Studio properties for one instance.",
			&["wsync props --project . --path Workspace/Part"],
		),
		(
			"query",
			"Live inspection",
			"Matches a selector inside Studio and projects selected properties.",
			&[
				"wsync query --project . 'Workspace/**/Camera'",
				"wsync query --project . 'ReplicatedStorage/Shared/*' --format paths",
			],
		),
		(
			"find",
			"Live inspection",
			"Finds live Studio instances by class name and/or name substring.",
			&["wsync find --project . --class RemoteEvent"],
		),
		(
			"find-attr",
			"Live inspection",
			"Finds live Studio instances that have a named attribute.",
			&["wsync find-attr --project . --name Health --under Workspace"],
		),
		(
			"classinfo",
			"Live inspection",
			"Lists properties and methods for a Roblox class.",
			&["wsync classinfo --project . --class BasePart"],
		),
		(
			"enums",
			"Live inspection",
			"Lists every Enum type name exposed by Studio.",
			&["wsync enums --project ."],
		),
		(
			"enum",
			"Live inspection",
			"Lists the items for one Roblox Enum type.",
			&["wsync enum --project . --name Material"],
		),
		(
			"source",
			"Live inspection",
			"Prints an instance's script source from live Studio.",
			&["wsync source --project . --path ReplicatedStorage/Shared/Hello"],
		),
		(
			"diff",
			"Live inspection",
			"Compares the project's on-disk tree against the live Studio DataModel.",
			&["wsync diff --project ."],
		),
		(
			"changes",
			"Live inspection",
			"Alias for `wsync diff`, intended for reviewing what a resync would change.",
			&["wsync changes --project ."],
		),
		(
			"services",
			"Live inspection",
			"Lists the project's sync roots and whether each exists.",
			&["wsync services --project . --raw"],
		),
		(
			"snapshot",
			"Live inspection",
			"Exports the live Studio tree plus inspectable properties to a file.",
			&["wsync snapshot --project ."],
		),
		(
			"path",
			"Path tools",
			"Translates between Studio instance paths and syncable filesystem paths.",
			&["wsync path --project . Workspace/Camera"],
		),
		(
			"meta",
			"Path tools",
			"Shows the Studio path, class, and filesystem path for a syncable instance.",
			&["wsync meta --project . ReplicatedStorage/Shared"],
		),
		(
			"where",
			"Path tools",
			"Finds live Studio instances by name substring and resolves a target.",
			&["wsync where --project . Hello"],
		),
		(
			"conflicts",
			"Conflict resolution",
			"Lists parked conflicts waiting for a Keep Disk or Keep Studio decision.",
			&["wsync conflicts --project ."],
		),
		(
			"resolve",
			"Conflict resolution",
			"Resolves one parked conflict by keeping either disk or Studio content.",
			&["wsync resolve --project . --path src/Shared/Hello.luau --disk"],
		),
		(
			"set",
			"Live writes",
			"Sets one property or applies a batch of property writes.",
			&["wsync set --project . --path Workspace/Camera --prop FieldOfView --value 90"],
		),
		(
			"new",
			"Live writes",
			"Creates a new live Studio instance under a parent path.",
			&["wsync new --project . --path Workspace --class Part --name Box"],
		),
		(
			"mv",
			"Live writes",
			"Reparents a live Studio instance to a destination parent path.",
			&["wsync mv --project . --from Workspace/Box --to Workspace/Folder"],
		),
		(
			"eval",
			"Live writes",
			"Executes Luau source inside Studio through the plugin sandbox.",
			&["wsync eval --project . --source 'return 1'"],
		),
		(
			"refresh",
			"Project docs",
			"Refreshes generated WSync agent docs without starting the daemon.",
			&["wsync refresh --project ."],
		),
	];

	let commands: Vec<serde_json::Value> = commands
		.iter()
		.map(|(name, category, description, examples)| {
			json!({
				"name": name,
				"title": format!("wsync {name}"),
				"category": category,
				"description": description,
				"usage": format!("wsync {name} [--project <path>]"),
				"examples": examples,
				"notes": [],
			})
		})
		.collect();

	let registry = json!({
		"schemaVersion": 1,
		"source": "docs/commands/*.json",
		"categories": ["Command registry", "Live diagnostics", "Live inspection", "Path tools",
			"Conflict resolution", "Live writes", "Project docs"],
		"commands": commands,
	});

	RegistryFacts::from_generated_json(&registry.to_string()).expect("fixture registry parses")
}

fn env_facts() -> EnvFacts {
	EnvFacts::new("0.1.0", 1, Path::new("/state/WSync")).with_plugin(Some(String::from("0.1.0")), true)
}

/// Loads a project from JSON written into a scratch directory, exercising the
/// real `Project` parser rather than a hand-built fact struct
fn project_facts(dir: &TempDir, project_json: serde_json::Value) -> ProjectFacts {
	let path = dir.path().join("default.project.json");
	fs::write(&path, serde_json::to_string_pretty(&project_json).unwrap()).unwrap();

	let project = Project::load(&path).expect("project loads");

	ProjectFacts::from_project(&project, &Config::new())
}

/// The stock `place` template tree
fn place_project() -> serde_json::Value {
	json!({
		"name": "demo",
		"tree": {
			"$className": "DataModel",
			"ReplicatedStorage": {
				"$path": "src/Shared",
				"Packages": { "$path": "Packages" },
			},
			"ServerScriptService": { "$path": "src/Server" },
			"StarterPlayer": {
				"StarterPlayerScripts": { "$path": "src/Client" },
			},
		},
	})
}

/// A project whose `syncRules` replace the built-in set
fn custom_rules_project() -> serde_json::Value {
	json!({
		"name": "custom",
		"tree": {
			"$className": "DataModel",
			"ReplicatedStorage": { "$path": "source" },
		},
		"globIgnorePaths": ["**/_vendor/**"],
		"syncRules": [
			{ "type": "ModuleScript", "pattern": "*.mod.luau", "child_pattern": "init.mod.luau" },
			{ "type": "ServerScript", "pattern": "*.srv.luau", "child_pattern": "init.srv.luau", "suffix": ".srv.luau" },
			{ "type": "StringValue", "pattern": "*.text" }
		],
	})
}

fn render(project: &ProjectFacts, registry: &RegistryFacts, env: &EnvFacts) -> GeneratedDocs {
	docsgen::render(&DocsInput::new(project, registry, env))
}

/// Text from one `## ` heading up to the next one
fn section<'doc>(doc: &'doc str, heading: &str) -> &'doc str {
	let start = doc
		.find(heading)
		.unwrap_or_else(|| panic!("missing section {heading} in:\n{doc}"));
	let rest = &doc[start..];

	match rest[heading.len()..].find("\n## ") {
		Some(end) => rest[..heading.len() + end].trim_end(),
		None => rest.trim_end(),
	}
}

fn headings(doc: &str) -> Vec<&str> {
	doc.lines().filter(|line| line.starts_with("## ")).collect()
}

/// Whitespace-collapsed view. The renderer re-wraps prose to a fixed column,
/// so a sentence assertion must never depend on where a line happens to break
fn flat(text: &str) -> String {
	text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

////////////////////////////////////////////////////////////////////////////////
// Golden snapshots
////////////////////////////////////////////////////////////////////////////////

const PLACE_HEADINGS: &str = r#"## 0. Agent bootstrap
## 0b. Refreshing agent docs
## 1. What syncs, what doesn't
## 1b. Playtesting is a separate environment
## 2. Filesystem conventions
## 3. Synced roots
## 4. Generated files (do not edit inside the markers)
## 5. Querying the live tree
## 5b. Linting Luau
## 5c. Asset uploads and monetization
## 6. Agent usage — live Studio control
## 6b. Handshake, health, logs, and change history
## 6c. Structured writes beyond `set` and `eval`
## 6e. Introspection — class info, enums, attribute-scoped search
## 6i. LLM-first command budget
## 7. Safety note"#;

const PLACE_CONVENTIONS: &str = r#"## 2. Filesystem conventions

These tables are generated from the sync rules actually in effect (the
built-in defaults; this project overrides none of them). First match wins.

**Directories**

| On disk | Roblox instance |
| --- | --- |
| `Foo/` | `Folder` named `Foo`, unless it holds an `init` file below |

**Scripts**

| On disk | Roblox instance |
| --- | --- |
| `Foo.server.luau` | `Script` named `Foo` (`RunContext = Legacy`) |
| `Foo.client.luau` | `Script` named `Foo` (`RunContext = Client`) |
| `Foo.local.luau` | `LocalScript` named `Foo` |
| `Foo.runserver.luau` | `Script` named `Foo` (`RunContext = Server`) |
| `Foo.luau` | `ModuleScript` named `Foo` |
| `Foo.server.lua` | `Script` named `Foo` (`RunContext = Legacy`) |
| `Foo.client.lua` | `Script` named `Foo` (`RunContext = Client`) |
| `Foo.local.lua` | `LocalScript` named `Foo` |
| `Foo.runserver.lua` | `Script` named `Foo` (`RunContext = Server`) |
| `Foo.lua` | `ModuleScript` named `Foo` |

**Values and localization**

| On disk | Roblox instance |
| --- | --- |
| `Foo.txt` | `StringValue` named `Foo`, `Value` is the file text |
| `Foo.md` | `StringValue` named `Foo`, `Value` is the Markdown converted to rich text |
| `Foo.csv` | `LocalizationTable` named `Foo` |

**Data modules**

| On disk | Roblox instance |
| --- | --- |
| `Foo.json` | `ModuleScript` named `Foo` that returns the decoded table |
| `Foo.toml` | `ModuleScript` named `Foo` that returns the decoded table |
| `Foo.yaml` | `ModuleScript` named `Foo` that returns the decoded table |
| `Foo.yml` | `ModuleScript` named `Foo` that returns the decoded table |
| `Foo.msgpack` | `ModuleScript` named `Foo` that returns the decoded table |

**Model files**

| On disk | Roblox instance |
| --- | --- |
| `Foo.model.json` | the instance tree the model file describes, rooted at `Foo` |
| `Foo.rbxm` | the serialized instance tree in the file, rooted at `Foo` |
| `Foo.rbxmx` | the serialized instance tree in the file, rooted at `Foo` |

**Project and metadata files**

| On disk | Roblox instance |
| --- | --- |
| `Foo.project.json` | a nested project; `$path` resolves through that project file |
| `Foo.meta.json` | metadata for the sibling instance: `$className`, properties, attributes, tags, `originalName` |

An instance that has both source and children is a directory plus one matching
`init` file. Edit the init file for the instance's own source; edit the
sibling entries for its children:

- `Foo/init.server.luau` — `Script` named `Foo` (`RunContext = Legacy`),
  with the directory's other entries as its children
- `Foo/init.client.luau` — `Script` named `Foo` (`RunContext = Client`),
  with the directory's other entries as its children
- `Foo/init.local.luau` — `LocalScript` named `Foo`, with the directory's
  other entries as its children
- `Foo/init.runserver.luau` — `Script` named `Foo` (`RunContext = Server`),
  with the directory's other entries as its children
- `Foo/init.luau` — `ModuleScript` named `Foo`, with the directory's other
  entries as its children
- `Foo/init.server.lua` — `Script` named `Foo` (`RunContext = Legacy`), with
  the directory's other entries as its children
- `Foo/init.client.lua` — `Script` named `Foo` (`RunContext = Client`), with
  the directory's other entries as its children
- `Foo/init.local.lua` — `LocalScript` named `Foo`, with the directory's
  other entries as its children
- `Foo/init.runserver.lua` — `Script` named `Foo` (`RunContext = Server`),
  with the directory's other entries as its children
- `Foo/init.lua` — `ModuleScript` named `Foo`, with the directory's other
  entries as its children

Script class and RunContext are encoded in the file suffix: `.server` is a
`Script` with `RunContext = Legacy`, `.runserver` a `Script` with
`RunContext = Server`, `.client` a `Script` with `RunContext = Client`, and
`.local` a `LocalScript`.

`rojoMode` is on, so syncback writes the Rojo-shaped forms: `init.*` files and
`.meta.json` sidecars. The Argon-legacy `.src.*` and `.data.json` forms are
still *read* where they exist, they are just never written.

New script files use the `.luau` form; `.lua` files are read as well.

Additional rules in force here:

- The synced roots in §3 are the only valid entry points. Arbitrary folders
  in the project root are not children of `game`; they are simply unmapped.
- Empty plain directories are ignored until they contain something syncable,
  so a placeholder folder cannot shadow a same-named script.
- File and directory renames/moves sync as Roblox renames and reparents while
  they stay under a mapped root.
- `*.meta.json` sidecars carry `$className`, properties, attributes, tags and
  `originalName` for the instance they sit next to. Property changes made
  there do reach Studio — but a property on an instance with no file form
  cannot be set from disk at all.
- Set a boolean Studio attribute `AvoidSync = true` on any instance to exclude
  that subtree from sync in both directions, including divergence comparison.
  It is the Studio-side counterpart to `ignoreGlobs` and needs no file edit.
  Use `wsync tree` or `wsync meta` to find the boundaries.
- The runtime directories `.wsync-backups/`, `.wsync-artifacts/` and
  `.wsync-workflows/` are never synced.

Studio names a path cannot express are **not** percent-encoded.
`renameInstances` is on, so WSync writes a safe file name and records the real
Studio name as `originalName` in the instance's sidecar, which is what makes
the name round-trip. `/` and control characters are rejected everywhere; on
Windows `< > : " / \ | ? *` and the reserved device names are rejected too.

`keepDuplicates` is off: when two siblings would land on the same path the
second is skipped with an error rather than silently overwriting the first."#;

const PLACE_ROOTS: &str = r#"## 3. Synced roots

The project root mirrors the `game` DataModel. Every entry below is a `$path`
mapping in `default.project.json` — this list is the whole projection, and
it is generated from the project tree rather than assumed:

- `ReplicatedStorage` → `src/Shared`
- `ReplicatedStorage/Packages` → `Packages`
- `ServerScriptService` → `src/Server`
- `StarterPlayer/StarterPlayerScripts` → `src/Client`

Anything outside these roots is Studio-only. That is not drift and not a bug:
it is the projection working as configured."#;

const CUSTOM_CONVENTIONS: &str = r#"## 2. Filesystem conventions

**This project defines its own `syncRules`.** Project rules replace the
built-in set entirely, so the tables below are the complete file-form
vocabulary for this project — a file shape that is not listed has no
instance form here. The tables are generated from `default.project.json`.

**Directories**

| On disk | Roblox instance |
| --- | --- |
| `Foo/` | `Folder` named `Foo`, unless it holds an `init` file below |

**Scripts**

| On disk | Roblox instance |
| --- | --- |
| `Foo.mod.luau` | `ModuleScript` named `Foo` |
| `Foo.srv.luau` | `Script` named `Foo` (`RunContext = Legacy`) |

**Values and localization**

| On disk | Roblox instance |
| --- | --- |
| `Foo.text` | `StringValue` named `Foo`, `Value` is the file text |

An instance that has both source and children is a directory plus one matching
`init` file. Edit the init file for the instance's own source; edit the
sibling entries for its children:

- `Foo/init.mod.luau` — `ModuleScript` named `Foo`, with the directory's
  other entries as its children
- `Foo/init.srv.luau` — `Script` named `Foo` (`RunContext = Legacy`), with
  the directory's other entries as its children

Script class and RunContext are encoded in the file suffix: `.server` is a
`Script` with `RunContext = Legacy`, `.runserver` a `Script` with
`RunContext = Server`, `.client` a `Script` with `RunContext = Client`, and
`.local` a `LocalScript`.

`rojoMode` is on. The rules in effect define no Argon-legacy `.src.*` or
`.data.json` forms, so syncback writes exactly the shapes in the tables above.

Additional rules in force here:

- The synced roots in §3 are the only valid entry points. Arbitrary folders
  in the project root are not children of `game`; they are simply unmapped.
- Empty plain directories are ignored until they contain something syncable,
  so a placeholder folder cannot shadow a same-named script.
- File and directory renames/moves sync as Roblox renames and reparents while
  they stay under a mapped root.
- The rules in effect define no metadata sidecar, so properties, attributes
  and tags have no file form in this project at all. Change them in Studio, or
  with `wsync set` and the user's consent.
- Set a boolean Studio attribute `AvoidSync = true` on any instance to exclude
  that subtree from sync in both directions, including divergence comparison.
  It is the Studio-side counterpart to `ignoreGlobs` and needs no file edit.
  Use `wsync tree` or `wsync meta` to find the boundaries.
- The runtime directories `.wsync-backups/`, `.wsync-artifacts/` and
  `.wsync-workflows/` are never synced.

Studio names a path cannot express are **not** percent-encoded.
`renameInstances` is on, so WSync writes a safe file name and records the real
Studio name as `originalName` in the instance's sidecar, which is what makes
the name round-trip. `/` and control characters are rejected everywhere; on
Windows `< > : " / \ | ? *` and the reserved device names are rejected too.

`keepDuplicates` is off: when two siblings would land on the same path the
second is skipped with an error rather than silently overwriting the first."#;

const CUSTOM_ROOTS: &str = r#"## 3. Synced roots

The project root mirrors the `game` DataModel. Every entry below is a `$path`
mapping in `default.project.json` — this list is the whole projection, and
it is generated from the project tree rather than assumed:

- `ReplicatedStorage` → `source`

Anything outside these roots is Studio-only. That is not drift and not a bug:
it is the projection working as configured."#;

#[test]
fn place_project_section_arc_is_stable() {
	let dir = TempDir::new().unwrap();
	let docs = render(&project_facts(&dir, place_project()), &fixture_registry(), &env_facts());

	assert_eq!(headings(&docs.project_memory).join("\n"), PLACE_HEADINGS);
}

#[test]
fn place_project_conventions_and_roots_are_golden() {
	let dir = TempDir::new().unwrap();
	let docs = render(&project_facts(&dir, place_project()), &fixture_registry(), &env_facts());

	assert_eq!(section(&docs.project_memory, "## 2."), PLACE_CONVENTIONS);
	assert_eq!(section(&docs.project_memory, "## 3."), PLACE_ROOTS);
}

#[test]
fn custom_sync_rules_drive_the_conventions_table() {
	let dir = TempDir::new().unwrap();
	let docs = render(
		&project_facts(&dir, custom_rules_project()),
		&fixture_registry(),
		&env_facts(),
	);

	let conventions = section(&docs.project_memory, "## 2.");

	assert_eq!(conventions, CUSTOM_CONVENTIONS);
	assert_eq!(section(&docs.project_memory, "## 3."), CUSTOM_ROOTS);

	// The override replaced the defaults, so no default file form may appear
	for default_form in [
		"`Foo.server.luau`",
		"`Foo.luau`",
		"`Foo.rbxm`",
		"`Foo.json`",
		"`Foo.meta.json`",
	] {
		assert!(
			!conventions.contains(default_form),
			"custom syncRules project still advertises the default form {default_form}"
		);
	}

	for custom_form in ["`Foo.mod.luau`", "`Foo.srv.luau`", "`Foo.text`", "`Foo/init.mod.luau`"] {
		assert!(
			conventions.contains(custom_form),
			"custom syncRules project is missing {custom_form}"
		);
	}

	// With no InstanceData rule there is no sidecar, and the doc must say so
	assert!(flat(conventions).contains("no metadata sidecar"));

	// Configured exclusions are reported, not assumed
	assert!(flat(section(&docs.project_memory, "## 1.")).contains("`ignoreGlobs`: `**/_vendor/**`"));
}

#[test]
fn synced_roots_come_from_the_project_tree_only() {
	let dir = TempDir::new().unwrap();
	let facts = project_facts(
		&dir,
		json!({
			"name": "exotic",
			"tree": {
				"$className": "DataModel",
				"TestService": { "$path": "src/Tests" },
			},
		}),
	);

	let docs = render(&facts, &fixture_registry(), &env_facts());
	let roots = section(&docs.project_memory, "## 3.");

	assert!(flat(roots).contains("- `TestService` → `src/Tests`"));
	assert_eq!(roots.lines().filter(|line| line.starts_with("- `")).count(), 1);

	// Nothing may leak in from a hardcoded service list
	for service in [
		"Workspace",
		"ReplicatedStorage",
		"ReplicatedFirst",
		"ServerScriptService",
		"ServerStorage",
		"StarterGui",
		"StarterPack",
		"StarterPlayer",
		"Lighting",
	] {
		assert!(
			!roots.contains(service),
			"§3 mentions {service}, which this project never maps"
		);
	}
}

#[test]
fn optional_paths_and_pinned_classes_are_reported() {
	let dir = TempDir::new().unwrap();
	let facts = project_facts(
		&dir,
		json!({
			"name": "optional",
			"tree": {
				"$className": "DataModel",
				"ReplicatedStorage": {
					"Packages": { "$path": { "optional": "Packages" } },
				},
				"Rig": { "$className": "Model", "$path": "src/Rig" },
			},
		}),
	);

	let docs = render(&facts, &fixture_registry(), &env_facts());
	let roots = section(&docs.project_memory, "## 3.");

	assert!(flat(roots).contains("- `ReplicatedStorage/Packages` → `Packages` — optional"));
	assert!(flat(roots).contains("- `Rig` → `src/Rig` (`$className: Model`)"));
}

#[test]
fn model_project_root_path_is_rendered() {
	let dir = TempDir::new().unwrap();
	let facts = project_facts(&dir, json!({ "name": "model", "tree": { "$path": "src" } }));
	let docs = render(&facts, &fixture_registry(), &env_facts());
	let roots = section(&docs.project_memory, "## 3.");

	assert!(flat(roots).contains("This project is not a place"));
	assert!(flat(roots).contains("- `<project root>` → `src`"));
}

////////////////////////////////////////////////////////////////////////////////
// Determinism
////////////////////////////////////////////////////////////////////////////////

#[test]
fn render_is_deterministic() {
	let dir = TempDir::new().unwrap();
	let facts = project_facts(&dir, place_project());
	let registry = fixture_registry();

	let first = render(&facts, &registry, &env_facts());
	let second = render(&facts, &registry, &env_facts());

	assert_eq!(first, second);

	// A timestamp would break every idempotency guarantee below
	for year in ["202", "203"] {
		assert!(!first.project_memory.contains(year), "the generated docs carry a date");
	}
}

#[test]
fn merging_a_rendered_file_again_changes_nothing() {
	let dir = TempDir::new().unwrap();
	let docs = render(&project_facts(&dir, place_project()), &fixture_registry(), &env_facts());

	let once = docsgen::wsync_md(None, &docs);
	assert_eq!(docsgen::wsync_md(Some(&once), &docs), once);

	let agents = docsgen::agents_md(None, &docs);
	assert_eq!(docsgen::agents_md(Some(&agents), &docs), agents);

	let claude = docsgen::claude_md(None);
	assert_eq!(docsgen::claude_md(Some(&claude)), claude);

	let codex = docsgen::codex_config(None).unwrap();
	assert_eq!(docsgen::codex_config(Some(&codex)).unwrap(), codex);
}

////////////////////////////////////////////////////////////////////////////////
// Marker engine
////////////////////////////////////////////////////////////////////////////////

fn demo_docs() -> (TempDir, GeneratedDocs) {
	let dir = TempDir::new().unwrap();
	let docs = render(&project_facts(&dir, place_project()), &fixture_registry(), &env_facts());

	(dir, docs)
}

#[test]
fn user_notes_around_the_block_survive_byte_for_byte() {
	let (_dir, docs) = demo_docs();

	let above = "# My project\n\nSome notes that WSync must never touch.\t \n\n";
	let below = "\n\n## My own section\n\nMore notes.\n";

	let seeded = format!(
		"{above}<!-- wsync:project-memory:start -->\nstale generated text\n<!-- wsync:project-memory:end -->{below}"
	);

	let merged = docsgen::wsync_md(Some(&seeded), &docs);

	assert!(merged.starts_with(above), "text above the block changed");
	assert!(merged.ends_with(below), "text below the block changed");
	assert!(!merged.contains("stale generated text"));
	assert_eq!(
		markers::extract(&merged, PROJECT_MEMORY).as_deref(),
		Some(docs.project_memory.trim_end())
	);

	// And it is stable from there on
	assert_eq!(docsgen::wsync_md(Some(&merged), &docs), merged);
}

#[test]
fn foreign_marker_blocks_are_left_alone() {
	let (_dir, docs) = demo_docs();

	let foreign = "<!-- t64:image-tools:start -->\nImageMagick lives here.\n<!-- t64:image-tools:end -->\n";
	let merged = docsgen::agents_md(Some(foreign), &docs);

	assert!(merged.starts_with(foreign), "another tool's block was rewritten");
	assert!(merged.contains(&format!("<!-- {AGENT_CONTEXT}:start -->")));
	assert_eq!(docsgen::agents_md(Some(&merged), &docs), merged);
}

#[test]
fn a_duplicated_block_collapses_to_one() {
	let (_dir, docs) = demo_docs();

	let block = format!("<!-- {PROJECT_MEMORY}:start -->\nold\n<!-- {PROJECT_MEMORY}:end -->\n");
	let seeded = format!("# Notes\n\n{block}\n{block}\ntrailing note\n");

	let merged = docsgen::wsync_md(Some(&seeded), &docs);

	assert_eq!(merged.matches(&format!("<!-- {PROJECT_MEMORY}:start -->")).count(), 1);
	assert_eq!(merged.matches(&format!("<!-- {PROJECT_MEMORY}:end -->")).count(), 1);
	assert!(merged.contains("trailing note"));
	assert!(!merged.contains("\nold\n"));
	assert_eq!(docsgen::wsync_md(Some(&merged), &docs), merged);
}

#[test]
fn an_unterminated_start_marker_is_repaired_without_losing_text() {
	let (_dir, docs) = demo_docs();

	let seeded = format!("# Notes\n\n<!-- {PROJECT_MEMORY}:start -->\nhalf-written doc\n\nimportant user note\n");
	let merged = docsgen::wsync_md(Some(&seeded), &docs);

	// The orphan marker is gone, the user's text is not
	assert_eq!(merged.matches(&format!("<!-- {PROJECT_MEMORY}:start -->")).count(), 1);
	assert!(merged.contains("important user note"));
	assert!(merged.contains("# Notes"));

	// Repaired once, stable thereafter — the block is never appended twice
	let again = docsgen::wsync_md(Some(&merged), &docs);
	assert_eq!(again, merged);
	assert_eq!(again.matches(&format!("<!-- {PROJECT_MEMORY}:start -->")).count(), 1);
}

#[test]
fn a_file_without_markers_gains_a_block_and_keeps_its_text() {
	let merged = docsgen::claude_md(Some("My Claude notes, no trailing newline"));

	assert!(merged.starts_with("My Claude notes, no trailing newline\n"));
	assert!(merged.contains(&format!(
		"<!-- {AGENTS_INCLUDE}:start -->\n@AGENTS.md\n<!-- {AGENTS_INCLUDE}:end -->"
	)));
	assert!(merged.ends_with('\n'));
	assert_eq!(docsgen::claude_md(Some(&merged)), merged);
}

////////////////////////////////////////////////////////////////////////////////
// File structures
////////////////////////////////////////////////////////////////////////////////

#[test]
fn agents_md_embeds_the_whole_reference() {
	let (_dir, docs) = demo_docs();
	let agents = docsgen::agents_md(None, &docs);

	assert!(agents.starts_with("# Agent notes"), "missing preamble on a new file");

	let embedded = markers::extract(&agents, AGENT_CONTEXT).expect("agent-context block");

	assert!(embedded.contains("# WSync agent context"));
	assert!(embedded.contains("### wsync.md"));
	// The embedded copy keeps wsync.md's own markers, exactly as the file has them
	assert!(embedded.contains(&format!("<!-- {PROJECT_MEMORY}:start -->")));
	assert!(embedded.contains(&format!("<!-- {PROJECT_MEMORY}:end -->")));
	assert!(embedded.contains(docs.project_memory.trim_end()));
}

#[test]
fn claude_md_is_only_the_import_when_created() {
	assert_eq!(
		docsgen::claude_md(None),
		format!("<!-- {AGENTS_INCLUDE}:start -->\n@AGENTS.md\n<!-- {AGENTS_INCLUDE}:end -->\n")
	);
}

////////////////////////////////////////////////////////////////////////////////
// Codex configuration
////////////////////////////////////////////////////////////////////////////////

fn fallbacks(config: &str) -> Vec<String> {
	let table: toml::Table = toml::from_str(config).expect("valid toml");

	table["project_doc_fallback_filenames"]
		.as_array()
		.unwrap()
		.iter()
		.map(|value| value.as_str().unwrap().to_owned())
		.collect()
}

#[test]
fn codex_config_is_created_with_the_wsync_docs() {
	let created = docsgen::codex_config(None).unwrap();

	assert_eq!(fallbacks(&created), vec!["wsync.md", "AGENTS.md", "CLAUDE.md"]);
	assert!(created.ends_with('\n'));
}

#[test]
fn codex_config_preserves_every_other_key_and_its_comments() {
	let existing = "\
# my own comment
mcp_servers = { demo = { command = \"node\", args = [\"server.mjs\"], enabled = true } }
model = \"gpt-5\"

[profiles.fast]
approval_policy = \"never\"
";

	let updated = docsgen::codex_config(Some(existing)).unwrap();

	assert!(updated.contains("# my own comment"), "comments were dropped");
	assert!(updated.contains("mcp_servers = { demo = "), "MCP config was rewritten");
	assert!(updated.contains("[profiles.fast]"));
	assert!(updated.contains("approval_policy = \"never\""));
	assert_eq!(fallbacks(&updated), vec!["wsync.md", "AGENTS.md", "CLAUDE.md"]);

	// The key is inserted above the first table header, where TOML requires it
	let key_at = updated.find("project_doc_fallback_filenames").unwrap();
	assert!(key_at < updated.find("[profiles.fast]").unwrap());

	assert_eq!(docsgen::codex_config(Some(&updated)).unwrap(), updated);
}

#[test]
fn codex_config_merges_extra_filenames_and_replaces_in_place() {
	let existing = "\
# leading comment
project_doc_fallback_filenames = [\"ro-sync.md\", \"CLAUDE.md\"]
model = \"gpt-5\"
";

	let updated = docsgen::codex_config(Some(existing)).unwrap();

	assert_eq!(
		fallbacks(&updated),
		vec!["wsync.md", "AGENTS.md", "CLAUDE.md", "ro-sync.md"]
	);
	assert!(updated.contains("# leading comment"));
	assert!(updated.contains("model = \"gpt-5\""));
	assert_eq!(updated.matches("project_doc_fallback_filenames").count(), 1);
	assert_eq!(docsgen::codex_config(Some(&updated)).unwrap(), updated);
}

#[test]
fn codex_config_already_correct_is_returned_untouched() {
	let existing = "project_doc_fallback_filenames = [\"wsync.md\", \"AGENTS.md\", \"CLAUDE.md\"]\nmodel = \"x\"\n";

	assert_eq!(docsgen::codex_config(Some(existing)).unwrap(), existing);
}

#[test]
fn codex_config_multiline_array_is_replaced_whole() {
	let existing = "\
project_doc_fallback_filenames = [
  \"ro-sync.md\", # legacy
  \"CLAUDE.md\",
]
model = \"gpt-5\"
";

	let updated = docsgen::codex_config(Some(existing)).unwrap();

	assert_eq!(
		fallbacks(&updated),
		vec!["wsync.md", "AGENTS.md", "CLAUDE.md", "ro-sync.md"]
	);
	assert!(updated.contains("model = \"gpt-5\""));
}

#[test]
fn codex_config_refuses_to_rewrite_broken_toml() {
	assert_eq!(
		docsgen::codex_config(Some("this is [not = toml")),
		Err(CodexSkip::Unparseable)
	);
}

////////////////////////////////////////////////////////////////////////////////
// write_all
////////////////////////////////////////////////////////////////////////////////

fn read(dir: &Path, name: &str) -> String {
	fs::read_to_string(dir.join(name)).unwrap_or_else(|_| panic!("missing {name}"))
}

#[test]
fn write_all_creates_then_reports_unchanged() {
	let (_project_dir, docs) = demo_docs();
	let workspace = TempDir::new().unwrap();

	let first = docsgen::write_all(workspace.path(), &docs, Preserve::UserNotes).unwrap();

	assert_eq!(first.len(), 4);
	assert!(first.iter().all(|outcome| outcome.status == WriteStatus::Created));

	let snapshot: Vec<String> = DocFile::ALL
		.iter()
		.map(|file| read(workspace.path(), file.relative_path()))
		.collect();

	let second = docsgen::write_all(workspace.path(), &docs, Preserve::UserNotes).unwrap();

	assert!(
		second.iter().all(|outcome| outcome.status == WriteStatus::Unchanged),
		"a second refresh rewrote files: {second:?}"
	);

	for (file, before) in DocFile::ALL.iter().zip(snapshot) {
		assert_eq!(read(workspace.path(), file.relative_path()), before);
	}

	// The four managed files, at their documented paths
	assert!(workspace.path().join("wsync.md").is_file());
	assert!(workspace.path().join("AGENTS.md").is_file());
	assert!(workspace.path().join("CLAUDE.md").is_file());
	assert!(workspace.path().join(".codex/config.toml").is_file());
}

#[test]
fn write_all_preserves_user_notes_and_foreign_blocks() {
	let (_project_dir, docs) = demo_docs();
	let workspace = TempDir::new().unwrap();

	fs::write(
		workspace.path().join("AGENTS.md"),
		"# House rules\n\nAlways ask first.\n",
	)
	.unwrap();
	fs::write(workspace.path().join("CLAUDE.md"), "Claude-only note.\n").unwrap();
	fs::create_dir_all(workspace.path().join(".codex")).unwrap();
	fs::write(workspace.path().join(".codex/config.toml"), "model = \"gpt-5\"\n").unwrap();

	docsgen::write_all(workspace.path(), &docs, Preserve::UserNotes).unwrap();

	let agents = read(workspace.path(), "AGENTS.md");
	assert!(agents.starts_with("# House rules\n\nAlways ask first.\n"));
	assert!(agents.contains("### wsync.md"));

	assert!(read(workspace.path(), "CLAUDE.md").starts_with("Claude-only note.\n"));
	assert!(read(workspace.path(), ".codex/config.toml").contains("model = \"gpt-5\""));

	let again = docsgen::write_all(workspace.path(), &docs, Preserve::UserNotes).unwrap();
	assert!(again.iter().all(|outcome| outcome.status == WriteStatus::Unchanged));
}

#[test]
fn regenerate_drops_notes_from_markdown_but_never_from_codex_config() {
	let (_project_dir, docs) = demo_docs();
	let workspace = TempDir::new().unwrap();

	fs::write(
		workspace.path().join("AGENTS.md"),
		"# House rules\n\nAlways ask first.\n",
	)
	.unwrap();
	fs::create_dir_all(workspace.path().join(".codex")).unwrap();
	fs::write(workspace.path().join(".codex/config.toml"), "model = \"gpt-5\"\n").unwrap();

	docsgen::write_all(workspace.path(), &docs, Preserve::Regenerate).unwrap();

	assert!(!read(workspace.path(), "AGENTS.md").contains("House rules"));
	assert!(read(workspace.path(), ".codex/config.toml").contains("model = \"gpt-5\""));
}

#[test]
fn write_all_skips_a_broken_codex_config() {
	let (_project_dir, docs) = demo_docs();
	let workspace = TempDir::new().unwrap();

	fs::create_dir_all(workspace.path().join(".codex")).unwrap();
	fs::write(workspace.path().join(".codex/config.toml"), "this is [not = toml").unwrap();

	let outcomes = docsgen::write_all(workspace.path(), &docs, Preserve::UserNotes).unwrap();
	let codex = outcomes
		.iter()
		.find(|outcome| outcome.file == DocFile::CodexConfig)
		.unwrap();

	assert!(matches!(codex.status, WriteStatus::Skipped(_)));
	assert_eq!(read(workspace.path(), ".codex/config.toml"), "this is [not = toml");
}

////////////////////////////////////////////////////////////////////////////////
// Registry-driven content
////////////////////////////////////////////////////////////////////////////////

/// Every `- `wsync <name>` — ` tier entry in the document
fn tier_entries(doc: &str) -> Vec<String> {
	doc.lines()
		.filter_map(|line| line.strip_prefix("- `wsync "))
		.filter_map(|rest| rest.split_once("` — "))
		.map(|(name, _)| name.to_owned())
		.collect()
}

/// Every `wsync <name>` invocation inside a fenced code block
fn code_invocations(doc: &str) -> Vec<String> {
	let mut inside = false;
	let mut names = Vec::new();

	for line in doc.lines() {
		if line.starts_with("```") {
			inside = !inside;
			continue;
		}

		if inside {
			if let Some(rest) = line.trim().strip_prefix("wsync ") {
				let name = rest.split_whitespace().next().unwrap_or_default();

				if !name.starts_with("--") {
					names.push(name.to_owned());
				}
			}
		}
	}

	names
}

/// Every `` `wsync <name>` `` mention in the document, paired with the blank-line
/// separated block it appears in
fn command_mentions(doc: &str) -> Vec<(String, String)> {
	let mut mentions = Vec::new();

	for block in doc.split("\n\n") {
		let mut rest = block;

		while let Some(at) = rest.find("`wsync ") {
			let after = &rest[at + "`wsync ".len()..];

			let Some(close) = after.find('`') else {
				break;
			};

			let name = after[..close]
				.split_whitespace()
				.next()
				.unwrap_or_default()
				.trim_end_matches(',')
				.to_owned();

			if !name.is_empty() && !name.starts_with("--") && !name.starts_with('<') {
				mentions.push((name, block.to_owned()));
			}

			rest = &after[close + 1..];
		}
	}

	mentions
}

/// A block may only name a command this build lacks if it says so in the same
/// breath
fn is_honest_about(block: &str) -> bool {
	let block = flat(block);

	block.contains("not in this build yet")
		|| block.contains("lands in a later build")
		|| block.contains("land in a later build")
		|| block.contains("not available here yet")
}

#[test]
fn every_named_command_is_a_real_shipped_command() {
	let dir = TempDir::new().unwrap();
	let registry = fixture_registry();
	let docs = render(&project_facts(&dir, place_project()), &registry, &env_facts());

	let named: Vec<String> = tier_entries(&docs.project_memory)
		.into_iter()
		.chain(code_invocations(&docs.project_memory))
		.collect();

	assert!(!named.is_empty());

	for name in named {
		assert!(
			registry.is_shipped(&name),
			"the docs tell an agent to run `wsync {name}`, which this build does not ship"
		);
	}
}

#[test]
fn tiers_split_read_only_from_mutating() {
	let dir = TempDir::new().unwrap();
	let registry = fixture_registry();
	let docs = render(&project_facts(&dir, place_project()), &registry, &env_facts());
	let live = section(&docs.project_memory, "## 6. ");

	let read_only = live.split("**Mutating").next().unwrap();
	let mutating = live.split("**Mutating").nth(1).unwrap();

	for name in tier_entries(read_only) {
		assert_eq!(registry.get(&name).unwrap().safety(), Safety::ReadOnly, "{name}");
	}

	for name in tier_entries(mutating) {
		assert_eq!(registry.get(&name).unwrap().safety(), Safety::Mutating, "{name}");
	}

	// The tiers are the registry's live commands, not a list typed into the renderer
	assert_eq!(
		tier_entries(read_only).len(),
		registry.live_tier(Safety::ReadOnly).len()
	);
	assert_eq!(tier_entries(mutating).len(), registry.live_tier(Safety::Mutating).len());
	assert!(tier_entries(mutating).contains(&String::from("set")));
	assert!(tier_entries(read_only).contains(&String::from("get")));
}

#[test]
fn unshipped_commands_are_reported_as_pending_not_promised() {
	let dir = TempDir::new().unwrap();
	let registry = fixture_registry().with_shipped(Shipped::only(["status", "tree", "ls", "get"]));
	let docs = render(&project_facts(&dir, place_project()), &registry, &env_facts());
	let doc = &docs.project_memory;

	// lint and upload are not even documented in the fixture, so they must read
	// as future work rather than as available tooling
	assert!(flat(section(doc, "## 5b.")).contains("lands in a later build"));
	assert!(flat(section(doc, "## 5c.")).contains("land in a later build"));

	// Nothing may be offered as runnable
	for name in tier_entries(doc).into_iter().chain(code_invocations(doc)) {
		assert!(registry.is_shipped(&name), "`wsync {name}` is offered but not shipped");
	}

	// The budget flow degrades honestly instead of naming a missing command
	let budget = flat(section(doc, "## 6i."));
	assert!(!budget.contains("`wsync commands --compact`"));
	assert!(!budget.contains("`wsync plan`"));
	assert!(budget.contains("`wsync context` is not in this build yet"));

	// And the mutating tier is empty, so §7 still has to say something sane
	assert!(flat(section(doc, "## 7.")).contains("escape hatches"));
}

#[test]
fn the_real_command_registry_parses_and_drives_the_docs() {
	let json = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/client-commands.generated.json"))
		.expect("generated registry is checked in");

	let registry = RegistryFacts::from_generated_json(&json).expect("real registry parses");

	assert_eq!(registry.schema_version, 1);
	assert!(registry.commands().count() > 50);

	// Categories the tier split depends on really exist in the registry
	assert!(registry.categories.iter().any(|category| category == "Live writes"));
	assert!(registry.categories.iter().any(|category| category == "Live inspection"));

	let read_only = registry.live_tier(Safety::ReadOnly);
	let mutating = registry.live_tier(Safety::Mutating);

	assert!(read_only.iter().any(|command| command.name == "tree"));
	assert!(mutating.iter().any(|command| command.name == "set"));
	assert!(!read_only.iter().any(|command| command.name == "set"));
	assert!(!mutating.iter().any(|command| command.name == "get"));

	// `refresh` is a project-docs command, never a live tier entry
	assert!(!read_only
		.iter()
		.chain(mutating.iter())
		.any(|command| command.name == "refresh"));

	let dir = TempDir::new().unwrap();
	let docs = render(&project_facts(&dir, place_project()), &registry, &env_facts());

	for name in tier_entries(&docs.project_memory)
		.into_iter()
		.chain(code_invocations(&docs.project_memory))
	{
		assert!(registry.is_documented(&name), "`wsync {name}` is not in the registry");
	}
}

#[test]
fn env_facts_appear_where_an_agent_needs_them() {
	let dir = TempDir::new().unwrap();
	let docs = render(&project_facts(&dir, place_project()), &fixture_registry(), &env_facts());
	let doc = &docs.project_memory;

	assert!(doc.contains("/state/WSync/writes.log"));
	assert!(flat(doc).contains("Generated for WSync 0.1.0 (protocol 1, Studio plugin 0.1.0)."));
	assert!(flat(doc).contains("A Studio plugin was connected when these docs were generated"));

	let offline = EnvFacts::new("0.1.0", 1, Path::new("/state/WSync"));
	let offline = render(&project_facts(&dir, place_project()), &fixture_registry(), &offline);

	assert!(flat(&offline.project_memory).contains("No Studio plugin was connected when these docs were generated"));
}

#[test]
fn conflict_engine_off_is_stated_rather_than_implied() {
	// The rendered text follows the resolved config, which defaults to on
	let dir = TempDir::new().unwrap();
	let mut facts = project_facts(&dir, place_project());

	assert!(facts.conflict_engine);
	facts.conflict_engine = false;

	let docs = render(&facts, &fixture_registry(), &env_facts());

	assert!(flat(section(&docs.project_memory, "## 1. ")).contains("`conflictEngine = false`"));
}

#[test]
fn prose_never_presents_an_unshipped_command_as_available() {
	let dir = TempDir::new().unwrap();
	// The real registry documents far more than the fixture ships, which is
	// exactly the state the CLI is in today
	let registry = fixture_registry().with_shipped(Shipped::only([
		"status", "tree", "ls", "get", "props", "find", "set", "new", "mv", "eval", "refresh",
	]));

	let docs = render(&project_facts(&dir, place_project()), &registry, &env_facts());
	let mentions = command_mentions(&docs.project_memory);

	assert!(mentions.len() > 20, "the document barely mentions any command");

	for (name, block) in mentions {
		if registry.is_shipped(&name) {
			continue;
		}

		assert!(
			is_honest_about(&block),
			"`wsync {name}` is named as if it worked, but this build does not ship it:\n{block}"
		);
	}
}
