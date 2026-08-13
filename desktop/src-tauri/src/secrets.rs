//! Secret storage for the Open Cloud API key (Design 8.2 Settings, Design 11).
//!
//! Two stores, in order of preference:
//!
//! 1. **The OS keychain**, reached through `/usr/bin/security` on macOS. A CLI
//!    rather than a crate on purpose — see the note on argv below, and note
//!    that it also gets the ACL right for free: the item's trusted application
//!    is `security` itself, so the same binary reading it back never raises the
//!    "wants to use your confidential information" prompt a second process
//!    would.
//! 2. **A `0600` JSON file** at `<data dir>/secrets.json`, written atomically.
//!    Used when there is no keychain to use (every non-macOS platform, today)
//!    or when the keychain refused. The fallback is *reported*, never silent:
//!    `SecretStatus.store` says which one holds the value and `note` says why.
//!
//! ## The value never appears in argv
//!
//! `security add-generic-password -w <value>` would put an Open Cloud API key
//! in the process table, where every other user on the machine can read it with
//! `ps`. `security`'s own usage text says as much: *"Use of the -p or -w
//! options is insecure. Specify -w as the last option to be prompted."*
//!
//! So `-w` is passed **last and empty**, and the value is written to the
//! child's stdin instead — twice, because the prompt asks for the password and
//! then for a retype, and a single copy leaves the second read at EOF and
//! stores an empty secret. Reading back (`find-generic-password -w`) and
//! deleting never need the value at all. The only thing in argv is the service
//! name and the account name, neither of which is a secret.
//!
//! One consequence is worth writing down, because it looks like a missing
//! feature: `add-generic-password` takes an optional trailing *keychain* path,
//! and `security`'s parser stops reading options at the first positional — so
//! a trailing keychain and a prompt-mode `-w` are mutually exclusive. Passing
//! both makes `security` read the keychain path *as the password*. WSync
//! therefore always writes to the default keychain, and the test below proves
//! the argv/stdin contract against a stub binary rather than by pointing the
//! real one at a throwaway keychain it cannot use in prompt mode.
//!
//! The prompt cannot carry two shapes, and both would be *silently* truncated
//! rather than refused, so WSync detects them and routes to the file store
//! instead (see `keychain_can_carry`): a value containing a newline (the prompt
//! is line-based), and a value longer than 128 bytes (the prompt's read buffer
//! — measured, not documented). The second one is why this matters: a Roblox
//! Open Cloud API key is ~600 bytes, so the keychain path would store its first
//! 128 bytes and the server would reject the fragment with `401 Invalid API
//! Key`. A key that big goes to the 0600 file store, intact, and says so.
//!
//! ## What crosses to the webview
//!
//! Presence, a masked tail (`"…abcd"`), and which store holds it. Never the
//! value. `secret_export_env` is reserved for the daemon handoff (Design 8.2:
//! "synced into the CLI credential store via env-var handoff, never argv") and
//! is deliberately unimplemented rather than approximated.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    error::{HostError, HostResult},
    storage,
};

/// The secrets this app knows how to hold. A name outside this list is refused
/// before any store is touched.
pub(crate) const SECRET_NAMES: &[&str] = &["openCloudApiKey"];

/// The keychain service every WSync item lives under; the account is the secret
/// name, so one service holds the whole set without collisions.
const KEYCHAIN_SERVICE: &str = "com.wsync.desktop";

const SECURITY_BIN: &str = "/usr/bin/security";

/// Which keychain backend a call should use.
///
/// An explicit parameter rather than a platform `cfg!` at each call site, and
/// rather than an environment variable: tests run in parallel threads in one
/// process, so an env-var seam would let one test flip another test's backend
/// mid-run — and on macOS that other test would then be writing to the
/// developer's real login keychain. This cannot.
#[derive(Clone, Debug)]
pub(crate) enum Keychain {
    /// `/usr/bin/security`, against the default keychain. macOS only.
    System,
    /// No keychain: every other platform today, and every test that is not
    /// specifically about the keychain.
    None,
    /// A stand-in binary that records what it was handed. Tests only.
    #[cfg(test)]
    Stub(std::path::PathBuf),
}

impl Keychain {
    /// What the shipped app uses.
    pub(crate) fn platform() -> Self {
        if cfg!(target_os = "macos") {
            Self::System
        } else {
            Self::None
        }
    }

    fn binary(&self) -> Option<OsString> {
        match self {
            Self::System => Some(OsString::from(SECURITY_BIN)),
            Self::None => None,
            #[cfg(test)]
            Self::Stub(path) => Some(path.clone().into_os_string()),
        }
    }

    fn available(&self) -> bool {
        self.binary().is_some()
    }
}

/// `security`'s "the item is not in the keychain" exit status.
const SECURITY_NOT_FOUND: i32 = 44;

/// How much of the value the masked form reveals. Enough to tell two keys
/// apart, far too little to reconstruct one.
const MASK_TAIL: usize = 4;

/// A secret longer than this is not an API key, and is refused rather than
/// written into a store that has to hold it forever.
const MAX_SECRET_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Store {
    Keychain,
    File,
}

/// Everything a caller learns about a stored secret.
///
/// Deliberately not the value: the webview is the least trusted part of the app
/// (Design 8.1), and it has no use for the key beyond showing that one is set.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecretStatus {
    pub(crate) name: String,
    pub(crate) present: bool,
    /// `"…abcd"`, or `None` when nothing is stored.
    pub(crate) masked: Option<String>,
    pub(crate) store: Option<Store>,
    /// Why the answer is what it is, when that is not obvious — a keychain that
    /// refused, a fallback that was taken. Shown verbatim in Settings.
    pub(crate) note: Option<String>,
}

impl SecretStatus {
    fn absent(name: &str) -> Self {
        Self {
            name: name.to_string(),
            present: false,
            masked: None,
            store: None,
            note: None,
        }
    }

    fn found(name: &str, value: &str, store: Store) -> Self {
        Self {
            name: name.to_string(),
            present: true,
            masked: Some(mask(value)),
            store: Some(store),
            note: None,
        }
    }

    fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

pub(crate) fn validate_name(name: &str) -> HostResult<()> {
    if SECRET_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(HostError::invalid_argument(format!(
            "{name:?} is not a WSync secret"
        )))
    }
}

/// The display form: an ellipsis and the last few characters.
///
/// Two rules, and the second is the one that matters: at most `MASK_TAIL`
/// characters, and **never more than half** of them. A short value therefore
/// reveals proportionally less rather than all of itself — the mask can never
/// become the secret, whatever length it is handed.
pub(crate) fn mask(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    let tail = MASK_TAIL.min(characters.len() / 2);
    let shown: String = characters[characters.len() - tail..].iter().collect();
    format!("…{shown}")
}

/// The macOS `security` password *prompt* reads at most 128 bytes and silently
/// drops the rest — measured, not documented (127/128 store intact, 129+ come
/// back truncated to 128). A Roblox Open Cloud API key is ~600 bytes, so the
/// keychain path would store a fragment the server rejects with 401. Values
/// over this go to the 0600 file store intact instead.
const KEYCHAIN_PROMPT_MAX_BYTES: usize = 128;

/// A value that is safe to hand the keychain prompt: single-line, non-empty,
/// and within the prompt's 128-byte read. Anything else is stored in the file
/// tier, where none of these limits apply.
fn keychain_can_carry(value: &str) -> bool {
    !value.contains('\n')
        && !value.contains('\r')
        && !value.contains('\0')
        && value.len() <= KEYCHAIN_PROMPT_MAX_BYTES
}

// ------------------------------------------------------------------ the API --
//
// Each of the three takes the backend explicitly, and the `*_in` form is what
// the tests drive. The three-argument wrappers are what `commands.rs` calls.

pub(crate) fn set(secrets_file: &Path, name: &str, value: &str) -> HostResult<SecretStatus> {
    set_in(secrets_file, name, value, &Keychain::platform())
}

pub(crate) fn get(secrets_file: &Path, name: &str) -> HostResult<SecretStatus> {
    get_in(secrets_file, name, &Keychain::platform())
}

pub(crate) fn clear(secrets_file: &Path, name: &str) -> HostResult<SecretStatus> {
    clear_in(secrets_file, name, &Keychain::platform())
}

/// Store a secret, keychain first.
///
/// The two stores are kept from drifting: a value that lands in the keychain
/// clears any file copy, and vice versa. Two stores holding two different keys
/// with no way to tell which one the daemon would get is worse than either.
fn set_in(
    secrets_file: &Path,
    name: &str,
    value: &str,
    keychain: &Keychain,
) -> HostResult<SecretStatus> {
    validate_name(name)?;
    let value = value.trim();
    if value.is_empty() {
        return Err(HostError::invalid_argument("the secret value was empty"));
    }
    if value.len() > MAX_SECRET_BYTES {
        return Err(HostError::invalid_argument(format!(
            "a secret is capped at {MAX_SECRET_BYTES} bytes; this one was {}",
            value.len()
        )));
    }

    if keychain.available() {
        if !keychain_can_carry(value) {
            file_set(secrets_file, name, value)?;
            let _ = keychain_delete(keychain, name);
            let reason = if value.len() > KEYCHAIN_PROMPT_MAX_BYTES {
                format!(
                    "This key is {} bytes; the macOS keychain prompt reads only \
                     {KEYCHAIN_PROMPT_MAX_BYTES} and silently truncates the rest (which is why a \
                     long Open Cloud key would come back rejected).",
                    value.len()
                )
            } else {
                "This value spans more than one line, which the keychain prompt cannot carry \
                 without truncating it."
                    .to_string()
            };
            return Ok(SecretStatus::found(name, value, Store::File)
                .with_note(format!("{reason} It was written to the 0600 file store instead.")));
        }
        match keychain_set(keychain, name, value) {
            Ok(()) => {
                // Best effort: a leftover file copy is stale the moment the
                // keychain has the real one.
                let _ = file_clear(secrets_file, name);
                return Ok(SecretStatus::found(name, value, Store::Keychain));
            }
            Err(why) => {
                file_set(secrets_file, name, value)?;
                return Ok(SecretStatus::found(name, value, Store::File).with_note(format!(
                    "The macOS keychain refused ({why}), so the key was written to the 0600 \
                     file store at {} instead.",
                    secrets_file.display()
                )));
            }
        }
    }

    file_set(secrets_file, name, value)?;
    Ok(SecretStatus::found(name, value, Store::File).with_note(
        "This platform has no keychain integration yet, so the key is held in the 0600 file \
         store.",
    ))
}

/// Presence and a masked tail. The value is read only to mask it, and dropped.
fn get_in(
    secrets_file: &Path,
    name: &str,
    keychain: &Keychain,
) -> HostResult<SecretStatus> {
    validate_name(name)?;

    if keychain.available() {
        match keychain_get(keychain, name) {
            Ok(Some(value)) => return Ok(SecretStatus::found(name, &value, Store::Keychain)),
            Ok(None) => {}
            Err(why) => {
                // A keychain that will not answer is not the same as an empty
                // one: fall through to the file, but say what happened.
                if let Some(value) = file_get(secrets_file, name)? {
                    return Ok(SecretStatus::found(name, &value, Store::File)
                        .with_note(format!("The macOS keychain could not be read ({why}).")));
                }
                return Ok(SecretStatus::absent(name)
                    .with_note(format!("The macOS keychain could not be read ({why}).")));
            }
        }
    }

    match file_get(secrets_file, name)? {
        Some(value) => Ok(SecretStatus::found(name, &value, Store::File)),
        None => Ok(SecretStatus::absent(name)),
    }
}

/// Remove the secret from **both** stores, so nothing survives a "Clear".
fn clear_in(
    secrets_file: &Path,
    name: &str,
    keychain: &Keychain,
) -> HostResult<SecretStatus> {
    validate_name(name)?;

    let mut note = None;
    if keychain.available() {
        if let Err(why) = keychain_delete(keychain, name) {
            note = Some(format!(
                "The macOS keychain entry could not be removed ({why}); the file store was \
                 cleared."
            ));
        }
    }
    file_clear(secrets_file, name)?;

    let status = SecretStatus::absent(name);
    Ok(match note {
        Some(note) => status.with_note(note),
        None => status,
    })
}

// ------------------------------------------------------------- the keychain --

/// `security add-generic-password -U … -w`, with the value on stdin.
///
/// `-w` is last and takes no argument, which is what makes `security` prompt —
/// and the prompt reads the pipe. Two newline-terminated copies: one for
/// "password data for new item", one for "retype password for new item".
fn keychain_set(keychain: &Keychain, name: &str, value: &str) -> Result<(), String> {
    let binary = keychain
        .binary()
        .ok_or_else(|| "no keychain on this platform".to_string())?;
    let mut child = Command::new(&binary)
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            name,
            // Last, and with no argument: anything after it would be read as
            // the password, and anything *as* it would be in argv.
            "-w",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {SECURITY_BIN}: {error}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "security did not accept a stdin pipe".to_string())?;
        write!(stdin, "{value}\n{value}\n")
            .map_err(|error| format!("could not hand the value to security: {error}"))?;
        // Dropped here, closing the pipe: `security` waits on EOF.
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("security did not finish: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(security_failure(&output))
    }
}

/// `security find-generic-password -w` prints the value, and nothing else.
fn keychain_get(keychain: &Keychain, name: &str) -> Result<Option<String>, String> {
    let binary = keychain
        .binary()
        .ok_or_else(|| "no keychain on this platform".to_string())?;
    let output = Command::new(&binary)
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            name,
            "-w",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("could not run {SECURITY_BIN}: {error}"))?;

    if output.status.code() == Some(SECURITY_NOT_FOUND) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(security_failure(&output));
    }

    // One trailing newline is `security`'s, not the value's. Nothing else is
    // stripped: an API key's own whitespace was already trimmed on the way in.
    let value = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string();
    Ok(if value.is_empty() { None } else { Some(value) })
}

fn keychain_delete(keychain: &Keychain, name: &str) -> Result<(), String> {
    let binary = keychain
        .binary()
        .ok_or_else(|| "no keychain on this platform".to_string())?;
    let output = Command::new(&binary)
        .args(["delete-generic-password", "-s", KEYCHAIN_SERVICE, "-a", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("could not run {SECURITY_BIN}: {error}"))?;

    // Already gone is the outcome asked for.
    if output.status.success() || output.status.code() == Some(SECURITY_NOT_FOUND) {
        Ok(())
    } else {
        Err(security_failure(&output))
    }
}

/// `security`'s own words, bounded — never the value, which it does not echo.
fn security_failure(output: &std::process::Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr);
    let message = message.trim();
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    if message.is_empty() {
        format!("security exited {code}")
    } else {
        let mut short: String = message.chars().take(200).collect();
        if message.chars().count() > 200 {
            short.push('…');
        }
        format!("security exited {code}: {short}")
    }
}

// ----------------------------------------------------------- the file store --

/// Read `secrets.json` as a name → value map.
///
/// Unknown names are dropped on read, the same rule `storage.rs` applies to the
/// state document: the file is this app's, and a key it does not recognize is
/// not something to carry forward.
fn file_read(path: &Path) -> HostResult<BTreeMap<String, String>> {
    let raw = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(HostError::io(format!(
                "could not read {}: {error}",
                path.display()
            )))
        }
    };

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

    let mut secrets = BTreeMap::new();
    for (key, value) in stored {
        if !SECRET_NAMES.contains(&key.as_str()) {
            continue;
        }
        if let Value::String(value) = value {
            if !value.is_empty() {
                secrets.insert(key, value);
            }
        }
    }
    Ok(secrets)
}

fn file_write(path: &Path, secrets: &BTreeMap<String, String>) -> HostResult<()> {
    if secrets.is_empty() {
        // An empty file is not the same as no file, but for a secret store it
        // may as well be — and not leaving one behind is tidier.
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(HostError::io(format!(
                    "could not remove {}: {error}",
                    path.display()
                )))
            }
        }
    }

    let mut document = Map::new();
    for (key, value) in secrets {
        document.insert(key.clone(), Value::String(value.clone()));
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(document))
        .map_err(|error| HostError::io(format!("could not serialize the secret store: {error}")))?;
    bytes.push(b'\n');
    // Same atomic temp+rename as `state.json`, and `0600` rather than `0644`:
    // the temp file is created with the mode, so there is no window in which
    // the key is on disk world-readable.
    storage::atomic_write(path, &bytes, 0o600)
}

fn file_set(path: &Path, name: &str, value: &str) -> HostResult<()> {
    let mut secrets = file_read(path).or_else(|error| {
        // A corrupt store must not wedge the app: an explicit write rebuilds it.
        if error.code == crate::error::code::CORRUPT_STATE {
            Ok(BTreeMap::new())
        } else {
            Err(error)
        }
    })?;
    secrets.insert(name.to_string(), value.to_string());
    file_write(path, &secrets)
}

fn file_get(path: &Path, name: &str) -> HostResult<Option<String>> {
    Ok(file_read(path)?.get(name).cloned())
}

fn file_clear(path: &Path, name: &str) -> HostResult<()> {
    let mut secrets = match file_read(path) {
        Ok(secrets) => secrets,
        // Clearing a corrupt store is exactly what should replace it.
        Err(error) if error.code == crate::error::code::CORRUPT_STATE => BTreeMap::new(),
        Err(error) => return Err(error),
    };
    if secrets.remove(name).is_none() && !path.exists() {
        return Ok(());
    }
    file_write(path, &secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn store() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secrets.json");
        (directory, path)
    }

    #[test]
    fn unknown_names_are_refused_before_any_store_is_touched() {
        let (_directory, path) = store();
        assert_eq!(
            set_in(&path, "nope", "value", &Keychain::None).unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
        assert_eq!(
            get_in(&path, "nope", &Keychain::None).unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
        assert!(!path.exists(), "a refused name must not create a store");
    }

    #[test]
    fn an_empty_or_oversized_value_is_refused() {
        let (_directory, path) = store();
        assert_eq!(
            set_in(&path, "openCloudApiKey", "   ", &Keychain::None).unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
        let huge = "k".repeat(MAX_SECRET_BYTES + 1);
        assert_eq!(
            set_in(&path, "openCloudApiKey", &huge, &Keychain::None).unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
    }

    #[test]
    fn the_mask_never_reveals_a_short_value() {
        assert_eq!(mask("sk-live-0123456789"), "…6789");
        assert_eq!(mask("abcdefghij"), "…ghij");
        assert_eq!(mask("abcdef"), "…def");
        assert_eq!(mask("abcde"), "…de");
        assert_eq!(mask("abc"), "…c");
        assert_eq!(mask("a"), "…");

        // The property, stated once so it holds for every length: never the
        // value, and never more than half of it.
        for length in 1..40 {
            let value: String = "sk-live-0123456789abcdefghijklmnopqrstuv"
                .chars()
                .take(length)
                .collect();
            let masked = mask(&value);
            assert_ne!(masked, value);
            let revealed = masked.chars().count() - 1;
            assert!(revealed <= MASK_TAIL, "{masked} reveals {revealed} of {value}");
            assert!(
                revealed * 2 <= length,
                "{masked} reveals over half of a {length}-character value",
            );
        }
    }

    // --- the file fallback, which every platform can reach ------------------

    #[test]
    fn the_file_store_round_trips_and_masks() {
        let (_directory, path) = store();

        assert!(!file_get(&path, "openCloudApiKey").unwrap().is_some());
        file_set(&path, "openCloudApiKey", "sk-live-abcdefgh1234").unwrap();

        // Round-trips at full fidelity inside the host…
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap().as_deref(),
            Some("sk-live-abcdefgh1234")
        );

        // …and the status crossing to the webview carries only the tail.
        let status = SecretStatus::found("openCloudApiKey", "sk-live-abcdefgh1234", Store::File);
        assert!(status.present);
        assert_eq!(status.masked.as_deref(), Some("…1234"));
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("sk-live-abcdefgh1234"),
            "the serialized status must not carry the value: {json}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_store_is_0600_and_leaves_no_temporary_files() {
        use std::os::unix::fs::PermissionsExt as _;

        let (directory, path) = store();
        file_set(&path, "openCloudApiKey", "sk-live-abcdefgh1234").unwrap();
        file_set(&path, "openCloudApiKey", "sk-live-zyxwvu987654").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a secret store must not be group/world readable");

        let entries: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "left behind: {entries:?}");
    }

    #[test]
    fn clearing_removes_the_value_and_the_file() {
        let (_directory, path) = store();
        file_set(&path, "openCloudApiKey", "sk-live-abcdefgh1234").unwrap();
        assert!(path.exists());

        file_clear(&path, "openCloudApiKey").unwrap();
        assert!(file_get(&path, "openCloudApiKey").unwrap().is_none());
        assert!(!path.exists(), "an empty secret store should not linger");

        // Clearing what is already gone is the asked-for outcome, not an error.
        file_clear(&path, "openCloudApiKey").unwrap();
    }

    #[test]
    fn a_corrupt_store_is_replaced_by_a_write_not_by_a_read() {
        let (_directory, path) = store();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap_err().code,
            crate::error::code::CORRUPT_STATE
        );
        file_set(&path, "openCloudApiKey", "sk-live-abcdefgh1234").unwrap();
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap().as_deref(),
            Some("sk-live-abcdefgh1234")
        );
    }

    #[test]
    fn a_foreign_key_in_the_store_is_ignored() {
        let (_directory, path) = store();
        std::fs::write(&path, br#"{"openCloudApiKey":"sk-1234","somethingElse":"x"}"#).unwrap();
        let secrets = file_read(&path).unwrap();
        assert_eq!(secrets.len(), 1);
        assert!(secrets.contains_key("openCloudApiKey"));
    }

    #[test]
    fn a_multiline_value_never_reaches_the_keychain_prompt() {
        // The prompt is line-based, so a value with a newline would be stored
        // truncated. It has to take the file path instead — and it must reach
        // that decision *before* running `security`, which is why the backend
        // here is a stub that would fail loudly if it were invoked.
        assert!(!keychain_can_carry("sk-live-1234\nsk-more"));
        assert!(!keychain_can_carry("sk-live-1234\r"));
        assert!(keychain_can_carry("sk-live-1234"));

        let (_directory, path) = store();
        let keychain = Keychain::Stub(PathBuf::from("/nonexistent/security"));
        let status = set_in(&path, "openCloudApiKey", "line-one\nline-two", &keychain).unwrap();
        assert_eq!(status.store, Some(Store::File));
        assert!(status.note.is_some(), "the fallback must say why");
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap().as_deref(),
            Some("line-one\nline-two"),
            "the file store keeps the value whole",
        );
    }

    #[test]
    fn an_over_128_byte_key_takes_the_file_store_intact_not_a_truncated_keychain() {
        // The bug this guards: a ~600-byte Roblox Open Cloud key handed to the
        // macOS keychain prompt comes back as its first 128 bytes, and the
        // server rejects the fragment with 401. A single-line key that long has
        // to reach the file store whole — so it must be diverted *before*
        // `security` runs, which the failing stub backend below proves.
        assert!(keychain_can_carry(&"k".repeat(KEYCHAIN_PROMPT_MAX_BYTES)));
        assert!(!keychain_can_carry(&"k".repeat(KEYCHAIN_PROMPT_MAX_BYTES + 1)));

        let long_key: String = std::iter::repeat(['a', 'b', 'c', 'd']).flatten().take(600).collect();
        assert!(long_key.len() > KEYCHAIN_PROMPT_MAX_BYTES);

        let (_directory, path) = store();
        let keychain = Keychain::Stub(PathBuf::from("/nonexistent/security"));
        let status = set_in(&path, "openCloudApiKey", &long_key, &keychain).unwrap();
        assert_eq!(status.store, Some(Store::File));
        let note = status.note.expect("a diverted long key must explain itself");
        assert!(note.contains("128") && note.contains("truncat"), "{note}");
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap().as_deref(),
            Some(long_key.as_str()),
            "the file store keeps every byte of the key",
        );
    }

    #[test]
    fn a_keychain_that_refuses_falls_through_to_the_file_and_says_so() {
        // The branch a user actually hits when the keychain is locked: the key
        // must still be stored, the status must say where, and the reason must
        // survive to the UI rather than being swallowed into "saved".
        let (_directory, path) = store();
        let keychain = Keychain::Stub(PathBuf::from("/nonexistent/security"));

        let status = set_in(&path, "openCloudApiKey", "sk-live-fallback-5150", &keychain).unwrap();
        assert!(status.present);
        assert_eq!(status.store, Some(Store::File));
        assert!(status.note.unwrap().contains("keychain"));
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap().as_deref(),
            Some("sk-live-fallback-5150"),
        );

        // …and reading it back finds it, still reporting the file store and the
        // reason the keychain did not answer.
        let read = get_in(&path, "openCloudApiKey", &keychain).unwrap();
        assert!(read.present);
        assert_eq!(read.store, Some(Store::File));
        assert_eq!(read.masked.as_deref(), Some("…5150"));

        let cleared = clear_in(&path, "openCloudApiKey", &keychain).unwrap();
        assert!(!cleared.present);
        assert!(!get_in(&path, "openCloudApiKey", &keychain).unwrap().present);
    }

    #[test]
    fn the_file_backend_round_trips_through_the_public_api() {
        // `Keychain::None` is what every non-macOS build uses, so this is the
        // whole shipped path on those platforms.
        let (_directory, path) = store();
        assert!(!get_in(&path, "openCloudApiKey", &Keychain::None).unwrap().present);

        let written = set_in(&path, "openCloudApiKey", "  sk-live-trimmed-9876  ", &Keychain::None)
            .unwrap();
        assert_eq!(written.store, Some(Store::File));
        assert_eq!(written.masked.as_deref(), Some("…9876"));

        let read = get_in(&path, "openCloudApiKey", &Keychain::None).unwrap();
        assert_eq!(read.masked, written.masked);
        assert_eq!(read.store, written.store);
        assert_eq!(
            file_get(&path, "openCloudApiKey").unwrap().as_deref(),
            Some("sk-live-trimmed-9876"),
            "a pasted key keeps its value and loses its surrounding whitespace",
        );

        clear_in(&path, "openCloudApiKey", &Keychain::None).unwrap();
        assert!(!get_in(&path, "openCloudApiKey", &Keychain::None).unwrap().present);
    }

    // --- the keychain call's shape, checked without a keychain --------------

    /// The security property, tested directly: **the value is never in argv**.
    ///
    /// This is the one claim in this module that a user has to take on trust
    /// otherwise, and it is exactly the claim a refactor could quietly break by
    /// "simplifying" `-w` into `-w <value>`. A stub binary records what it was
    /// handed, so the assertion is on the real `Command` this code builds
    /// rather than on a description of it — no keychain, no macOS, no CI
    /// special case.
    #[cfg(unix)]
    #[test]
    fn the_keychain_call_puts_the_value_on_stdin_and_never_in_argv() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let stub = directory.path().join("security-stub");
        let argv_log = directory.path().join("argv");
        let stdin_log = directory.path().join("stdin");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\ncat > {}\n",
                argv_log.display(),
                stdin_log.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let value = "sk-live-never-in-argv-8080";
        keychain_set(&Keychain::Stub(stub), "openCloudApiKey", value).unwrap();

        let argv = std::fs::read_to_string(&argv_log).unwrap();
        assert!(
            !argv.contains(value),
            "the secret must never reach the process table; argv was:\n{argv}",
        );
        assert!(argv.contains("add-generic-password"), "argv was:\n{argv}");
        assert!(argv.contains(KEYCHAIN_SERVICE), "argv was:\n{argv}");
        assert!(argv.contains("openCloudApiKey"), "argv was:\n{argv}");
        assert!(
            argv.trim_end().ends_with("-w"),
            "`-w` must be last and bare, or security reads the next argument as the password; \
             argv was:\n{argv}",
        );

        // Twice, newline-terminated: `security` prompts for the password and
        // then for a retype, and one copy leaves the retype at EOF — which
        // stores an *empty* secret and reports success.
        assert_eq!(
            std::fs::read_to_string(&stdin_log).unwrap(),
            format!("{value}\n{value}\n"),
        );
    }

    /// A full round trip against the real macOS keychain.
    ///
    /// Opt-in (`WSYNC_KEYCHAIN_TESTS=1`) because it writes to the **default**
    /// keychain — `security` cannot be pointed at a throwaway one in prompt
    /// mode, see the module header — and a developer's login keychain is not a
    /// fixture to be scribbled on by a plain `cargo test`. It uses an account
    /// name no real secret can have, and deletes it again. Without the opt-in
    /// it prints how to run it rather than silently passing.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_keychain_round_trips_when_opted_in() {
        if std::env::var_os("WSYNC_KEYCHAIN_TESTS").is_none() {
            eprintln!(
                "note: skipping the live macOS keychain round-trip. It writes and deletes a \
                 `{KEYCHAIN_SERVICE}` item in your default keychain; run it with \
                 WSYNC_KEYCHAIN_TESTS=1 cargo test. The argv/stdin contract is covered without \
                 it, and so is the file fallback."
            );
            return;
        }

        // Not `openCloudApiKey`: this must be incapable of clobbering a real one.
        let system = Keychain::System;
        let account = "wsyncTestKey";
        let value = "sk-live-keychain-4321";
        let _ = keychain_delete(&system, account);

        keychain_set(&system, account, value).unwrap();
        assert_eq!(
            keychain_get(&system, account).unwrap(),
            Some(value.to_string()),
            "the value must survive the stdin prompt intact — one copy would store empty",
        );

        keychain_delete(&system, account).unwrap();
        assert_eq!(keychain_get(&system, account).unwrap(), None);
        // Deleting what is already gone is the outcome asked for, not a failure.
        keychain_delete(&system, account).unwrap();
    }
}
