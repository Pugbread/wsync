//! The app's persisted state (Design 8.3) and the paths it lives at (Design 12).
//!
//! One JSON document, one writer, atomic temp+rename replacement. There is no
//! partial-write path: either the previous document survives intact or the new
//! one lands whole.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};
use tauri::{AppHandle, Manager as _};

use crate::error::{HostError, HostResult};

/// Every key the webview is allowed to read or write, per Design 8.3.
///
/// This is the whole persisted surface. Adding a key means adding it here and
/// giving it a default below — a `state_set` for anything else is rejected, so
/// a compromised or buggy view cannot grow the state file into a scratch pad.
pub(crate) const STATE_KEYS: &[&str] = &[
    "projects",
    "projectsRoot",
    "activeProjectId",
    "servedProjectIds",
    "daemonSessions",
    "appearanceTheme",
    "lastView",
    // Design 7.3's last-edited ledger: `{projectId: {seenAt, paths:{path: ms}}}`,
    // fed by sanitized `sync-activity` names and bounded by the frontend store
    // (≤400 paths per project, ≤8 projects). Its own key rather than a field on
    // a project record, because it is an observation cache and not registry
    // truth — losing it costs a sort order, nothing else.
    "lastEdited",
];

/// The shape a fresh install starts from. Also fills gaps in an older document,
/// so a state file written by an earlier version never yields `undefined`.
pub(crate) fn default_document() -> Map<String, Value> {
    let mut document = Map::new();
    document.insert("projects".into(), Value::Array(Vec::new()));
    document.insert("projectsRoot".into(), Value::Null);
    document.insert("activeProjectId".into(), Value::Null);
    document.insert("servedProjectIds".into(), Value::Array(Vec::new()));
    document.insert("daemonSessions".into(), Value::Object(Map::new()));
    document.insert("appearanceTheme".into(), Value::String("system".into()));
    document.insert("lastView".into(), Value::String("projects".into()));
    document.insert("lastEdited".into(), Value::Object(Map::new()));
    document
}

pub(crate) fn is_known_key(key: &str) -> bool {
    STATE_KEYS.contains(&key)
}

#[derive(Clone, Debug)]
pub(crate) struct AppPaths {
    /// `<platform data dir>/WSync` — shared with the daemon (Design 12), which
    /// is why this is not Tauri's identifier-scoped `app_data_dir`.
    pub(crate) data_dir: PathBuf,
    pub(crate) state_file: PathBuf,
    pub(crate) secrets_file: PathBuf,
    pub(crate) daemons_dir: PathBuf,
}

impl AppPaths {
    pub(crate) fn initialize(app: &AppHandle) -> HostResult<Self> {
        let data_dir = app
            .path()
            .data_dir()
            .map_err(|error| {
                HostError::io(format!("could not resolve the platform data directory: {error}"))
            })?
            .join("WSync");
        create_private_dir(&data_dir)?;
        let daemons_dir = data_dir.join("daemons");
        create_private_dir(&daemons_dir)?;

        Ok(Self {
            state_file: data_dir.join("state.json"),
            secrets_file: data_dir.join("secrets.json"),
            daemons_dir,
            data_dir,
        })
    }
}

/// Read the whole document, defaults filled in.
pub(crate) fn read_document(path: &Path) -> HostResult<Map<String, Value>> {
    let raw = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(default_document())
        }
        Err(error) => {
            return Err(HostError::io(format!(
                "could not read {}: {error}",
                path.display()
            )))
        }
    };

    // A corrupt file is reported, never silently discarded: the frontend falls
    // back to in-memory defaults and says so, and the bad bytes stay on disk
    // for the user to inspect.
    let parsed: Value = serde_json::from_slice(&raw).map_err(|error| {
        HostError::corrupt_state(format!(
            "{} is not valid JSON ({error}); it was left untouched",
            path.display()
        ))
    })?;
    let Value::Object(stored) = parsed else {
        return Err(HostError::corrupt_state(format!(
            "{} does not contain a JSON object; it was left untouched",
            path.display()
        )));
    };

    let mut document = default_document();
    for (key, value) in stored {
        if is_known_key(&key) {
            document.insert(key, value);
        }
    }
    Ok(document)
}

/// Read one key, or the whole document when `key` is `None`.
pub(crate) fn read_value(path: &Path, key: Option<&str>) -> HostResult<Value> {
    let document = read_document(path)?;
    match key {
        None => Ok(Value::Object(document)),
        Some(key) if is_known_key(key) => {
            Ok(document.get(key).cloned().unwrap_or(Value::Null))
        }
        Some(key) => Err(HostError::invalid_argument(format!(
            "{key:?} is not part of the persisted app state"
        ))),
    }
}

/// Merge a patch of allowlisted keys and replace the file atomically.
/// Returns the document as it now exists on disk.
pub(crate) fn merge_document(path: &Path, patch: Map<String, Value>) -> HostResult<Value> {
    if patch.is_empty() {
        return Err(HostError::invalid_argument("the state patch was empty"));
    }
    if let Some(unknown) = patch.keys().find(|key| !is_known_key(key)) {
        return Err(HostError::invalid_argument(format!(
            "{unknown:?} is not part of the persisted app state"
        )));
    }

    let mut document = read_document(path).or_else(|error| {
        // A corrupt document must not wedge the app forever: an explicit write
        // rebuilds from defaults. Reads still report the corruption.
        if error.code == crate::error::code::CORRUPT_STATE {
            Ok(default_document())
        } else {
            Err(error)
        }
    })?;
    for (key, value) in patch {
        document.insert(key, value);
    }

    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| HostError::io(format!("could not serialize app state: {error}")))?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, 0o600)?;
    Ok(Value::Object(document))
}

/// Create a directory the current user alone can enter.
pub(crate) fn create_private_dir(path: &Path) -> HostResult<()> {
    fs::create_dir_all(path).map_err(|error| {
        HostError::io(format!("could not create {}: {error}", path.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            HostError::io(format!("could not secure {}: {error}", path.display()))
        })?;
    }
    Ok(())
}

/// Write `bytes` to `path` so that a reader sees either the old file or the new
/// one, never a truncated mix: same-directory temp file, flushed, then renamed
/// over the target. `mode` applies on unix only.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> HostResult<()> {
    let parent = path.parent().ok_or_else(|| {
        HostError::io(format!("{} has no parent directory", path.display()))
    })?;
    create_private_dir(parent)?;

    let temporary = parent.join(temporary_name(path));
    let write = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = fs::remove_file(&temporary);
        return Err(HostError::io(format!(
            "could not stage a replacement for {}: {error}",
            path.display()
        )));
    }

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(HostError::io(format!(
            "could not replace {}: {error}",
            path.display()
        )));
    }

    // Best effort: make the rename itself durable. Not supported everywhere,
    // and never a reason to report failure once the rename has succeeded.
    let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

fn temporary_name(path: &Path) -> String {
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(".{stem}.tmp-{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_reads_as_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let document = read_document(&path).unwrap();
        assert_eq!(document.get("appearanceTheme").unwrap(), "system");
        assert!(!path.exists(), "reading must not create the state file");
    }

    #[test]
    fn merge_persists_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");

        let mut patch = Map::new();
        patch.insert("appearanceTheme".into(), Value::String("black".into()));
        merge_document(&path, patch).unwrap();

        let mut second = Map::new();
        second.insert("lastView".into(), Value::String("settings".into()));
        merge_document(&path, second).unwrap();

        let document = read_document(&path).unwrap();
        assert_eq!(document.get("appearanceTheme").unwrap(), "black");
        assert_eq!(document.get("lastView").unwrap(), "settings");
        assert_eq!(read_value(&path, Some("lastView")).unwrap(), "settings");
    }

    #[test]
    fn unknown_keys_are_refused_on_both_sides() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");

        let mut patch = Map::new();
        patch.insert("scratch".into(), Value::Bool(true));
        assert_eq!(
            merge_document(&path, patch).unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
        assert_eq!(
            read_value(&path, Some("scratch")).unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
    }

    #[test]
    fn corrupt_state_is_reported_not_swallowed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            read_document(&path).unwrap_err().code,
            crate::error::code::CORRUPT_STATE
        );

        // ...but an explicit write still recovers.
        let mut patch = Map::new();
        patch.insert("lastView".into(), Value::String("docs".into()));
        merge_document(&path, patch).unwrap();
        assert_eq!(read_value(&path, Some("lastView")).unwrap(), "docs");
    }

    #[test]
    fn atomic_write_leaves_no_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        atomic_write(&path, b"{}\n", 0o600).unwrap();
        atomic_write(&path, b"{\"a\":1}\n", 0o600).unwrap();
        let entries: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "left behind: {entries:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
    }
}
