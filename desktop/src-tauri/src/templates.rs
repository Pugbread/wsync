//! What a Studio-triggered project looks like on disk (Design 4.3, 4.4, 7.0, 8.4).
//!
//! Deliberately the *smallest* thing `wsync serve` accepts: the eight standard
//! services mapped to `src/<ServiceName>`, the directories they point at, and a
//! `.gitignore` that covers the runtime dirs from Design 12. Nothing else.
//!
//! Design 7.0 pins both halves of that shape:
//!
//! * the **eight services** (`ReplicatedStorage`, `ServerScriptService`,
//!   `StarterPlayer`, `StarterGui`, `Workspace`, `ReplicatedFirst`,
//!   `ServerStorage`, `Lighting`) each map to `src/<Name>`, so a place's code
//!   lands where its service says it lives rather than in a Server/Client/Shared
//!   convention the DataModel does not have;
//! * **`scope: "code"`** — the default projection. Only `Folder`, `Script`,
//!   `LocalScript` and `ModuleScript` sync to disk; every other class stays
//!   Studio-authoritative. `"full"` re-enables the middleware projection, and is
//!   a project-file decision rather than a UI setting.
//!
//! Two omissions are on purpose, and both matter:
//!
//! * **No starter scripts.** `assets/templates/place` ships a `Main.server.luau`
//!   and friends, which is right for `wsync init` on an empty place and wrong
//!   here: a project created from Studio is created *against a place that
//!   already has content*. Empty source dirs mean the first connect applies
//!   Studio → disk (Design 7.0) instead of quietly pushing `print("Hello
//!   world")` into their game.
//! * **No `Packages` / `ServerPackages` nodes.** Wally is off for a fresh
//!   Studio-initiated project (`wallyEnabled: false` in the registry), and a
//!   `$path` node for a directory that does not exist would both warn on every
//!   start and create an empty Folder in the DataModel. The user can add Wally
//!   later; the project file is the place that decision belongs.
//!
//! Every write here is **no-follow**: directories are created one component at a
//! time with `create_dir` and files with `create_new`, so an existing entry —
//! symlink or not — fails the write rather than being followed (Design 11).

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde_json::{json, Map, Value};

use crate::error::{HostError, HostResult};

/// Design 7.0's eight standard services, in the order the ruling lists them.
/// Each one maps to `src/<Name>`; nothing else is mapped.
pub(crate) const SERVICES: &[&str] = &[
    "ReplicatedStorage",
    "ServerScriptService",
    "StarterPlayer",
    "StarterGui",
    "Workspace",
    "ReplicatedFirst",
    "ServerStorage",
    "Lighting",
];

/// Design 7.0's default projection: `Folder`, `Script`, `LocalScript` and
/// `ModuleScript` only. Written into every scaffolded project file, because the
/// scope is a property of the project and never of the app's settings.
pub(crate) const DEFAULT_SCOPE: &str = "code";

/// The project file every WSync project is discovered by (Design 12).
pub(crate) const PROJECT_FILE: &str = "default.project.json";

/// Design 12's runtime directories are generated, never authored, so they are
/// ignored from the first commit rather than after the first surprise.
const GITIGNORE: &str = "\
# WSync runtime (Design 12)
/.wsync-artifacts/
/.wsync-workflows/
/.wsync-backups/
/sourcemap.json

# Wally
/Packages
/ServerPackages
/DevPackages

# Build artifacts
/*.rbxl
/*.rbxlx
/*.rbxm
/*.rbxmx

# Misc
.DS_Store
";

/// The Roblox facts the broker was given, already validated.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectFacts {
    /// Human name for the project file's `name` field.
    pub(crate) name: String,
    pub(crate) game_id: u64,
    pub(crate) place_ids: Vec<u64>,
    /// Design 4.3's WSync extension: creator context, when Studio said the
    /// place belongs to a group.
    pub(crate) group_id: Option<u64>,
}

/// The place tree, with the Roblox ids filled in (Design 4.3).
///
/// Built through `serde_json` rather than string formatting so a project name
/// containing a quote, a backslash or a newline cannot produce a file the
/// engine will not parse.
pub(crate) fn project_document(facts: &ProjectFacts) -> Value {
    let mut document = Map::new();
    document.insert("name".into(), Value::String(facts.name.clone()));
    // Design 7.0: the projection is the project's, not the app's. Written even
    // though it is the engine's default, so the file states which scope it was
    // created under rather than inheriting whatever a later default becomes.
    document.insert("scope".into(), Value::String(DEFAULT_SCOPE.into()));

    let mut tree = Map::new();
    tree.insert("$className".into(), Value::String("DataModel".into()));
    for service in SERVICES {
        tree.insert(
            (*service).into(),
            json!({ "$path": format!("src/{service}") }),
        );
    }
    document.insert("tree".into(), Value::Object(tree));
    document.insert("gameId".into(), json!(facts.game_id));
    if let Some(group_id) = facts.group_id {
        document.insert("groupId".into(), json!(group_id));
    }
    document.insert("placeIds".into(), json!(facts.place_ids));
    Value::Object(document)
}

/// Write the whole scaffold into an **already-created, empty** directory.
///
/// The caller owns creating `directory` (that is where the one-direct-child and
/// escape rules live, Design 11); this function owns what goes inside it. On
/// any failure the caller is expected to remove the directory again — a
/// half-written project is worse than none.
pub(crate) fn write_scaffold(directory: &Path, facts: &ProjectFacts) -> HostResult<Vec<PathBuf>> {
    let mut written = Vec::new();

    // `src` first, then one directory per mapped service. One component at a
    // time, never `create_dir_all`: a pre-existing entry — including a symlink
    // someone slipped in mid-write — fails here instead of being followed out
    // of the project.
    let source_root = directory.join("src");
    fs::create_dir(&source_root).map_err(|error| {
        HostError::io(format!("could not create {}: {error}", source_root.display()))
    })?;
    written.push(source_root.clone());

    for service in SERVICES {
        let path = source_root.join(service);
        fs::create_dir(&path).map_err(|error| {
            HostError::io(format!("could not create {}: {error}", path.display()))
        })?;
        written.push(path);
    }

    let mut project = serde_json::to_vec_pretty(&project_document(facts))
        .map_err(|error| HostError::io(format!("could not serialize the project file: {error}")))?;
    project.push(b'\n');
    written.push(create_new(directory, PROJECT_FILE, &project)?);
    written.push(create_new(directory, ".gitignore", GITIGNORE.as_bytes())?);

    Ok(written)
}

/// Create a file that must not already exist. `create_new` is the no-follow
/// guard: it fails on a symlink rather than writing through it.
fn create_new(directory: &Path, name: &str, bytes: &[u8]) -> HostResult<PathBuf> {
    let path = directory.join(name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| HostError::io(format!("could not create {}: {error}", path.display())))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| HostError::io(format!("could not write {}: {error}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_document_carries_the_ids_and_the_place_tree() {
        let document = project_document(&ProjectFacts {
            name: "My \"Game\"".into(),
            game_id: 123,
            place_ids: vec![456, 789],
            group_id: Some(42),
        });

        assert_eq!(document["name"], "My \"Game\"");
        assert_eq!(document["gameId"], 123);
        assert_eq!(document["placeIds"], json!([456, 789]));
        assert_eq!(document["groupId"], 42);
        // Design 7.0: code scope, stated rather than inherited.
        assert_eq!(document["scope"], "code");

        // Design 7.0's eight services, each at `src/<Name>` — including
        // StarterPlayer itself rather than StarterPlayerScripts underneath it.
        assert_eq!(document["tree"]["$className"], "DataModel");
        for service in SERVICES {
            assert_eq!(
                document["tree"][service]["$path"],
                json!(format!("src/{service}")),
                "{service} is not mapped"
            );
        }
        assert_eq!(SERVICES.len(), 8);
        assert!(document["tree"]["StarterPlayer"].get("StarterPlayerScripts").is_none());
        // Nothing outside the eight is mapped.
        assert_eq!(document["tree"].as_object().unwrap().len(), SERVICES.len() + 1);

        // Wally slots are deliberately absent: nothing maps a directory the
        // scaffold does not create.
        assert!(document["tree"]["ReplicatedStorage"].get("Packages").is_none());

        // A name with a quote in it round-trips as JSON, not as a broken file.
        let encoded = serde_json::to_string(&document).unwrap();
        let reparsed: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reparsed["name"], "My \"Game\"");
    }

    #[test]
    fn no_group_id_means_no_group_id_field() {
        let document = project_document(&ProjectFacts {
            name: "solo".into(),
            game_id: 1,
            place_ids: vec![],
            group_id: None,
        });
        assert!(document.get("groupId").is_none());
        assert_eq!(document["placeIds"], json!([]));
    }

    #[test]
    fn the_scaffold_is_exactly_the_documented_layout() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("my-game");
        fs::create_dir(&project).unwrap();

        write_scaffold(
            &project,
            &ProjectFacts {
                name: "My Game".into(),
                game_id: 7,
                place_ids: vec![8],
                group_id: None,
            },
        )
        .unwrap();

        assert!(project.join("src").is_dir());
        for service in SERVICES {
            assert!(project.join("src").join(service).is_dir(), "missing {service}");
        }
        assert!(project.join(PROJECT_FILE).is_file());
        assert!(project.join(".gitignore").is_file());

        // No git, and no starter scripts: an empty source tree is what makes the
        // first connect a clean Studio → disk apply rather than a push of
        // scaffolding into someone's game.
        assert!(!project.join(".git").exists());
        for service in SERVICES {
            assert_eq!(
                fs::read_dir(project.join("src").join(service)).unwrap().count(),
                0,
                "src/{service} must start empty"
            );
        }
        // `src` holds the eight service directories and nothing else.
        assert_eq!(fs::read_dir(project.join("src")).unwrap().count(), SERVICES.len());

        let gitignore = fs::read_to_string(project.join(".gitignore")).unwrap();
        for runtime in [".wsync-artifacts", ".wsync-workflows", ".wsync-backups"] {
            assert!(gitignore.contains(runtime), "{runtime} must be ignored");
        }

        // The engine parses `default.project.json` with serde; a trailing
        // newline and pretty printing are what a human diffing it expects.
        let raw = fs::read_to_string(project.join(PROJECT_FILE)).unwrap();
        assert!(raw.ends_with("}\n"));
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["gameId"], 7);
    }

    #[test]
    fn writes_never_follow_an_existing_entry() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("occupied");
        fs::create_dir(&project).unwrap();
        // Someone got there first with the project file's name.
        fs::write(project.join(PROJECT_FILE), b"{}").unwrap();
        fs::create_dir(project.join("src")).unwrap();

        let error = write_scaffold(&project, &ProjectFacts::default()).unwrap_err();
        assert_eq!(error.code, crate::error::code::IO);
        // The pre-existing file is untouched.
        assert_eq!(fs::read_to_string(project.join(PROJECT_FILE)).unwrap(), "{}");
    }
}
