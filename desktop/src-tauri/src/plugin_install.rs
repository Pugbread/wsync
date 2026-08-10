//! The Studio plugin artifact: where it comes from, whether it is the one this
//! build ships, and how it lands in Roblox's plugins folder (Design 8.2,
//! Appendix A `plugin`).
//!
//! Two things make this more than a file copy.
//!
//! **It is verified before it is installed.** The build writes
//! `WSync.build.json` beside `WSync.rbxm` — `{schemaVersion, artifact, sha256,
//! pluginVersion, protocolVersion, builtAt, buildCommit, buildDirty}` — and the
//! sha256 in it is checked against the bytes about to be written into Studio's
//! plugins directory. A mismatch is refused outright rather than installed with a
//! caveat: the whole point of shipping a manifest is that "probably the right
//! plugin" is not a state anyone can act on. An artifact with **no** manifest
//! beside it still installs, because a hand-built `rojo build` in a dev tree is
//! a legitimate thing to want, but it installs into a reported `verified:
//! false` state and says so in the UI.
//!
//! A manifest that exists but cannot be read is a refusal too, not a fallback
//! to "unverified". Otherwise corrupting one byte of the manifest would be a
//! way to switch verification off.
//!
//! **Resolution order is fail-closed, not override-first.** In order:
//!
//! 1. the app's own resources — the artifact packaged beside the binary;
//! 2. `WSYNC_PLUGIN_ARTIFACT`;
//! 3. `plugin/WSync.rbxm` in a dev tree, found by walking up from the cwd;
//! 4. a file the user picked in the native dialog (`commands.rs` drives this
//!    one; it is never reached without an explicit click).
//!
//! This is deliberately the opposite of `WSYNC_DAEMON_PATH`, which wins over
//! the bundled sidecar. The difference is what an override can do: pointing the
//! daemon at another binary affects this app's own child process, while
//! pointing this at another `.rbxm` writes it into Roblox Studio, where it
//! outlives the app and runs with Studio's permissions. A stray environment
//! variable inherited from a shell should not be able to do that to a packaged
//! install, so the packaged artifact wins where one exists — which is also the
//! only case where an override is being ignored, since a dev build has no
//! resources to prefer.
//!
//! **A dev-tree artifact is rebuilt when its sources are newer.** Installing
//! from a checkout must mean installing what the checkout *says*, not what a
//! forgotten `build.mjs` run left behind — that gap is exactly how a fixed
//! plugin fails to reach Studio while everyone believes it has. `dev_tree_stale`
//! detects the gap and `rebuild_dev_tree` closes it by running the plugin's own
//! build script; `commands.rs` wires the two in front of `install`. Resources
//! and env/picked artifacts are installed as-is: a packaged app's resources are
//! its version, and an explicit override means the user knows better.
//!
//! Every path in here that touches the plugins directory takes it as an
//! argument. There is no test in this file that can reach the real one.

use std::{
    ffi::OsString,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::error::{HostError, HostResult};

// -------------------------------------------------------------- constants --

/// What the build produces, and the name Studio loads it under. The install is
/// always this name, whatever the source file was called.
pub(crate) const ARTIFACT_NAME: &str = "WSync.rbxm";

/// The identity document the build writes beside the artifact.
pub(crate) const MANIFEST_NAME: &str = "WSync.build.json";

/// Names an older single-file plugin used. Studio would load one of these
/// *alongside* the model and run two copies of WSync, so installing removes
/// them.
const STALE_NAMES: &[&str] = &["WSync.lua", "WSync.luau"];

/// Points at a `WSync.rbxm` to install. Consulted after the app's own
/// resources — see the module header for why that way round.
pub(crate) const ARTIFACT_PATH_ENV: &str = "WSYNC_PLUGIN_ARTIFACT";

/// Printed verbatim in the "no artifact" error, so the fix is in the message
/// rather than in a doc the user has to go and find.
pub(crate) const BUILD_COMMAND: &str = "node plugin/scripts/build.mjs";

/// The manifest shape this build was written against. A newer one is read for
/// the fields it still has and noted, not refused: the document is additive.
const SCHEMA_VERSION: u64 = 1;

/// A sanity bound on what may be read and hashed. The real artifact is a few
/// hundred KiB; this exists so a mis-picked file cannot be pulled into memory.
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// How far up from the working directory to look for a `plugin/` sibling.
/// Enough to cover `desktop/src-tauri/target/debug` in a dev tree.
const DEV_TREE_DEPTH: usize = 6;

// ------------------------------------------------------------------ origin --

/// Where a resolved artifact came from. Reported so the UI can say "from the
/// app bundle" rather than leaving the user to guess which copy was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Packaged beside the binary.
    Resources,
    /// `WSYNC_PLUGIN_ARTIFACT`.
    Env,
    /// `plugin/WSync.rbxm` in a checkout.
    DevTree,
    /// The user picked it in the native dialog.
    Picked,
}

impl Origin {
    /// The stable wire value. Written out rather than derived, so renaming a
    /// variant cannot silently change what the frontend reads.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Resources => "resources",
            Self::Env => "env",
            Self::DevTree => "dev-tree",
            Self::Picked => "picked",
        }
    }
}

/// A file that exists and is the plugin artifact, plus where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Artifact {
    pub(crate) path: PathBuf,
    pub(crate) origin: Origin,
}

// ----------------------------------------------------------------- sources --

/// The places resolution may look, passed in rather than read from the process.
///
/// `commands.rs` fills this from the `AppHandle` and the environment; tests
/// fill it from a temp directory. Nothing in here reaches out on its own.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sources {
    /// Directories Tauri considers app resources. Both the resource dir and
    /// the executable's own directory, because "bundled beside the binary" is
    /// one place on Windows and another inside a `.app`.
    pub(crate) resource_dirs: Vec<PathBuf>,
    /// The raw `WSYNC_PLUGIN_ARTIFACT` value, if set.
    pub(crate) env_override: Option<OsString>,
    /// Where to start walking up from, looking for `plugin/WSync.rbxm`.
    pub(crate) cwd: Option<PathBuf>,
}

/// Find the artifact to install, in the order the module header states.
///
/// The env override is validated only when it is actually consulted: a typo in
/// it is an error rather than a silent fall-through to the dev tree, for the
/// same reason `WSYNC_DAEMON_PATH` reports one — a wrong artifact installed
/// quietly is worse than a refusal that names the variable.
pub(crate) fn resolve(sources: &Sources) -> HostResult<Artifact> {
    for directory in &sources.resource_dirs {
        let candidate = directory.join(ARTIFACT_NAME);
        if candidate.is_file() {
            return Ok(Artifact {
                path: candidate,
                origin: Origin::Resources,
            });
        }
    }

    if let Some(raw) = &sources.env_override {
        let path = PathBuf::from(raw);
        if !path.is_file() {
            return Err(HostError::invalid_argument(format!(
                "{ARTIFACT_PATH_ENV} points at {}, which is not a file",
                path.display()
            )));
        }
        return Ok(Artifact {
            path,
            origin: Origin::Env,
        });
    }

    if let Some(path) = dev_tree_artifact(sources.cwd.as_deref()) {
        return Ok(Artifact {
            path,
            origin: Origin::DevTree,
        });
    }

    Err(HostError::unavailable(format!(
        "no {ARTIFACT_NAME} is available to install.\n\
         Release builds carry it as an app resource. In a checkout, build it:\n  \
         {BUILD_COMMAND}\n\
         or point {ARTIFACT_PATH_ENV} at one, or choose the file yourself."
    )))
}

/// Walk up from `cwd` looking for `plugin/WSync.rbxm`.
///
/// Bounded rather than unbounded: a dev build's working directory is a few
/// levels inside the checkout, and climbing to `/` would let a stray `plugin/`
/// directory anywhere above the user's home become the source.
fn dev_tree_artifact(cwd: Option<&Path>) -> Option<PathBuf> {
    let mut directory = cwd?;
    for _ in 0..DEV_TREE_DEPTH {
        let candidate = directory.join("plugin").join(ARTIFACT_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = directory.parent()?;
    }
    None
}

/// Where Roblox Studio loads plugins from.
///
/// Linux has no Studio, so there is no honest answer to give — and inventing a
/// directory there would mean writing a file nothing will ever read.
pub(crate) fn plugins_dir() -> HostResult<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| HostError::io("HOME is not set, so the Roblox plugins folder cannot be found"))?;
        Ok(home.join("Documents").join("Roblox").join("Plugins"))
    }
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or_else(|| {
            HostError::io("LOCALAPPDATA is not set, so the Roblox plugins folder cannot be found")
        })?;
        Ok(local.join("Roblox").join("Plugins"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(HostError::unavailable(
            "Roblox Studio does not run on this platform, so there is no plugins folder to install into",
        ))
    }
}

// ---------------------------------------------------------------- manifest --

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawManifest {
    schema_version: Option<u64>,
    artifact: Option<String>,
    sha256: Option<String>,
    plugin_version: Option<String>,
    protocol_version: Option<Value>,
    built_at: Option<String>,
    build_commit: Option<String>,
    #[serde(default)]
    build_dirty: bool,
}

/// The build's own statement about the artifact sitting next to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    /// The file the manifest describes, when it names one.
    pub(crate) artifact: Option<String>,
    /// Lowercase hex, 64 characters. Validated on parse, so nothing downstream
    /// has to wonder whether a comparison is case-sensitive.
    pub(crate) sha256: String,
    pub(crate) plugin_version: Option<String>,
    pub(crate) protocol_version: Option<u64>,
    /// ISO 8601, verbatim from the build. Not identity — the sha256 is — but
    /// the one field that tells two same-versioned builds apart at a glance.
    pub(crate) built_at: Option<String>,
    pub(crate) build_commit: Option<String>,
    pub(crate) build_dirty: bool,
    /// Set when the document declares a schema this build predates.
    pub(crate) schema_note: Option<String>,
}

/// Parse `WSync.build.json`.
///
/// Strict about the one field verification depends on and relaxed about the
/// rest: a missing `pluginVersion` costs a line in the UI, while a missing or
/// malformed `sha256` would silently turn the check off.
pub(crate) fn parse_manifest(bytes: &[u8]) -> HostResult<Manifest> {
    let raw: RawManifest = serde_json::from_slice(bytes).map_err(|error| {
        HostError::corrupt_state(format!("{MANIFEST_NAME} is not valid JSON ({error})"))
    })?;

    let sha256 = raw
        .sha256
        .map(|digest| digest.trim().to_ascii_lowercase())
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| {
            HostError::corrupt_state(format!(
                "{MANIFEST_NAME} carries no usable sha256, so the artifact beside it cannot be verified"
            ))
        })?;

    let schema_note = match raw.schema_version {
        Some(version) if version > SCHEMA_VERSION => Some(format!(
            "{MANIFEST_NAME} declares schema {version}; this build understands {SCHEMA_VERSION} \
             and read the fields it knows."
        )),
        _ => None,
    };

    Ok(Manifest {
        artifact: raw.artifact.filter(|name| !name.trim().is_empty()),
        sha256,
        plugin_version: raw.plugin_version.filter(|version| !version.trim().is_empty()),
        protocol_version: raw.protocol_version.as_ref().and_then(protocol_number),
        built_at: raw.built_at.filter(|timestamp| !timestamp.trim().is_empty()),
        build_commit: raw.build_commit.filter(|commit| !commit.trim().is_empty()),
        build_dirty: raw.build_dirty,
        schema_note,
    })
}

/// Read the manifest beside `artifact`, if there is one.
///
/// `Ok(None)` means no manifest exists. A manifest that exists and cannot be
/// understood is an error — see the module header.
pub(crate) fn manifest_beside(artifact: &Path) -> HostResult<Option<Manifest>> {
    let Some(directory) = artifact.parent() else {
        return Ok(None);
    };
    let path = directory.join(MANIFEST_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(HostError::io(format!(
                "could not read {}: {error}",
                path.display()
            )))
        }
    };
    parse_manifest(&bytes).map(Some)
}

/// The protocol as a number, from either shape the ecosystem uses: the
/// daemon's `/hello` reports `1`, a manifest may write `1` or `"wsync/1"`.
fn protocol_number(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => {
            let tail = text.rsplit('/').next().unwrap_or(text);
            tail.trim().parse().ok()
        }
        _ => None,
    }
}

/// The protocol a `/hello` document reports, in the same normalized form.
pub(crate) fn hello_protocol(hello: &Value) -> Option<u64> {
    hello.get("protocol").and_then(protocol_number)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// ----------------------------------------------------------------- install --

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallReport {
    pub(crate) installed: bool,
    pub(crate) path: String,
    pub(crate) plugin_version: Option<String>,
    pub(crate) protocol_version: Option<u64>,
    /// sha256 of the bytes actually written — the installed file's identity,
    /// manifest or no manifest.
    pub(crate) sha256: String,
    /// When the artifact was built, from its manifest.
    pub(crate) built_at: Option<String>,
    /// True only when a manifest was found *and* its sha256 matched the bytes
    /// written. Never a default, never inferred.
    pub(crate) verified: bool,
    /// `resources` · `env` · `dev-tree` · `picked`.
    pub(crate) source: &'static str,
    /// Everything the user should know that "installed" does not say: no
    /// manifest, a stale file removed, a schema this build predates.
    pub(crate) note: Option<String>,
}

/// Verify and install one artifact into `plugins_dir`.
///
/// Blocking: the caller runs it off the async runtime.
pub(crate) fn install(artifact: &Artifact, plugins_dir: &Path) -> HostResult<InstallReport> {
    let bytes = read_artifact(&artifact.path)?;
    let manifest = manifest_beside(&artifact.path)?;
    let digest = sha256_hex(&bytes);

    let mut notes: Vec<String> = Vec::new();
    let verified = match &manifest {
        Some(manifest) => {
            if let Some(named) = &manifest.artifact {
                let actual = artifact
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !named.eq_ignore_ascii_case(&actual) {
                    return Err(HostError::integrity(format!(
                        "the {MANIFEST_NAME} beside {actual} describes {named}, not this file. \
                         Rebuild the pair with `{BUILD_COMMAND}`."
                    )));
                }
            }

            if digest != manifest.sha256 {
                return Err(HostError::integrity(format!(
                    "{} does not match the {MANIFEST_NAME} beside it — the manifest expects \
                     sha256 {}, the file is {}. Nothing was installed. Rebuild the pair with \
                     `{BUILD_COMMAND}`.",
                    artifact.path.display(),
                    short_digest(&manifest.sha256),
                    short_digest(&digest),
                )));
            }
            if let Some(note) = &manifest.schema_note {
                notes.push(note.clone());
            }
            true
        }
        None => {
            notes.push(format!(
                "No {MANIFEST_NAME} was found beside this artifact, so its contents could not be \
                 verified. It was installed as-is."
            ));
            false
        }
    };

    let target = plugins_dir.join(ARTIFACT_NAME);
    atomic_place(&target, &bytes)?;

    let removed = remove_stale(plugins_dir);
    if !removed.is_empty() {
        notes.push(format!(
            "Removed an older single-file plugin ({}) that Studio would have loaded alongside \
             this one.",
            removed.join(", ")
        ));
    }

    Ok(InstallReport {
        installed: true,
        path: target.display().to_string(),
        plugin_version: manifest.as_ref().and_then(|m| m.plugin_version.clone()),
        protocol_version: manifest.as_ref().and_then(|m| m.protocol_version),
        sha256: digest,
        built_at: manifest.as_ref().and_then(|m| m.built_at.clone()),
        verified,
        source: artifact.origin.as_str(),
        note: join_notes(notes),
    })
}

fn read_artifact(path: &Path) -> HostResult<Vec<u8>> {
    let metadata = fs::metadata(path)
        .map_err(|error| HostError::io(format!("could not read {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(HostError::invalid_argument(format!(
            "{} is not a file",
            path.display()
        )));
    }
    if metadata.len() == 0 {
        return Err(HostError::invalid_argument(format!(
            "{} is empty, so it is not a plugin",
            path.display()
        )));
    }
    if metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(HostError::invalid_argument(format!(
            "{} is {} bytes; a plugin artifact is capped at {MAX_ARTIFACT_BYTES}",
            path.display(),
            metadata.len()
        )));
    }
    fs::read(path).map_err(|error| HostError::io(format!("could not read {}: {error}", path.display())))
}

/// Temp file in the target directory, then rename over the target — the same
/// shape as `storage::atomic_write`, minus its `0700` clamp on the parent.
/// That clamp is right for WSync's own data directory and wrong here: this is
/// Roblox's plugins folder, and tightening its permissions as a side effect of
/// installing would be a change nobody asked for.
///
/// Studio watches this directory, so a partially written `WSync.rbxm` is a
/// plugin it will try to load. The rename is what makes that impossible.
fn atomic_place(target: &Path, bytes: &[u8]) -> HostResult<()> {
    let parent = target.parent().ok_or_else(|| {
        HostError::io(format!("{} has no parent directory", target.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        HostError::io(format!("could not create {}: {error}", parent.display()))
    })?;

    let temporary = parent.join(temporary_name());
    let write = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = fs::remove_file(&temporary);
        return Err(HostError::io(format!(
            "could not stage a replacement for {}: {error}",
            target.display()
        )));
    }

    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(HostError::io(format!(
            "could not replace {}: {error}",
            target.display()
        )));
    }
    Ok(())
}

fn temporary_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(".{ARTIFACT_NAME}.tmp-{}-{nanos}", std::process::id())
}

/// Delete the single-file plugins an older WSync installed. Best effort: a file
/// that will not go away is worth mentioning, not worth failing an install that
/// otherwise succeeded — and the caller reports what was actually removed.
fn remove_stale(plugins_dir: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    for name in STALE_NAMES {
        let path = plugins_dir.join(name);
        if path.is_file() && fs::remove_file(&path).is_ok() {
            removed.push((*name).to_string());
        }
    }
    removed
}

fn short_digest(digest: &str) -> String {
    digest.chars().take(12).collect()
}

fn join_notes(notes: Vec<String>) -> Option<String> {
    if notes.is_empty() {
        None
    } else {
        Some(notes.join(" "))
    }
}

// --------------------------------------------------------- dev-tree rebuild --

/// Everything the plugin build reads, relative to the plugin directory. A file
/// newer than the artifact in any of these means the artifact no longer says
/// what the checkout says. `sourcemap.json` and the artifact pair itself are
/// build *outputs* and deliberately absent.
const DEV_TREE_INPUTS: &[&str] = &[
    "src",
    "Packages",
    "default.project.json",
    "wally.toml",
    "wally.lock",
    "aftman.toml",
    "scripts/build.mjs",
];

/// A dev-tree artifact older than the sources that produce it.
#[derive(Debug)]
pub(crate) struct Stale {
    /// The newest input — named in the UI so "why did install rebuild" has an
    /// answer.
    pub(crate) source: PathBuf,
}

/// Whether the checkout's sources changed after `artifact` was built.
///
/// Mtime-based, so it can miss a same-second edit; the price of a false "fresh"
/// is installing a build the very next click repairs, and the price of hashing
/// every source on every status poll would be paid always.
pub(crate) fn dev_tree_stale(artifact: &Path) -> Option<Stale> {
    let plugin_dir = artifact.parent()?;
    let artifact_modified = fs::metadata(artifact).ok()?.modified().ok()?;

    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for input in DEV_TREE_INPUTS {
        scan_newest(&plugin_dir.join(input), &mut newest);
    }

    let (source, source_modified) = newest?;
    (source_modified > artifact_modified).then_some(Stale { source })
}

fn scan_newest(path: &Path, newest: &mut Option<(PathBuf, SystemTime)>) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };

    if metadata.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                scan_newest(&entry.path(), newest);
            }
        }
        return;
    }

    if let Ok(modified) = metadata.modified() {
        if newest.as_ref().map_or(true, |(_, time)| modified > *time) {
            *newest = Some((path.to_path_buf(), modified));
        }
    }
}

/// Run the plugin's own build script so the artifact matches the checkout.
///
/// The script resolves rojo/wally itself (its `TOOL_DIRS` probing), so the one
/// thing this has to find is `node` — and a GUI-launched app's PATH is the
/// minimal system one, so the well-known install locations are searched too.
/// Blocking, and slow when wally refreshes its index: callers run it off the
/// async runtime, behind the same lock as the install itself.
pub(crate) fn rebuild_dev_tree(plugin_dir: &Path) -> HostResult<()> {
    let script = plugin_dir.join("scripts").join("build.mjs");
    if !script.is_file() {
        return Err(HostError::unavailable(format!(
            "{} does not exist, so the stale plugin cannot be rebuilt here",
            script.display()
        )));
    }

    let path_value = augmented_path();
    let Some(node) = find_node(&path_value) else {
        return Err(HostError::unavailable(format!(
            "the plugin sources changed after {ARTIFACT_NAME} was built, and no `node` was found \
             to rebuild it. Install Node 18+, or run `{BUILD_COMMAND}` in the checkout and \
             Install again."
        )));
    };

    let output = std::process::Command::new(&node)
        .arg(&script)
        .current_dir(plugin_dir)
        .env("PATH", &path_value)
        .output()
        .map_err(|error| {
            HostError::io(format!("could not run {} {}: {error}", node.display(), script.display()))
        })?;

    if !output.status.success() {
        let mut lines: Vec<String> = Vec::new();
        for stream in [&output.stdout, &output.stderr] {
            lines.extend(
                String::from_utf8_lossy(stream)
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_owned),
            );
        }
        let tail = lines
            .iter()
            .rev()
            .take(8)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(HostError::unavailable(format!(
            "rebuilding the Studio plugin failed. Nothing was installed.\n{tail}\n\
             Run `{BUILD_COMMAND}` in the checkout for the full story."
        )));
    }

    Ok(())
}

/// The process PATH plus the places node, rojo and wally are actually
/// installed on developer machines. Appended, never prepended: an explicit
/// PATH entry keeps winning.
fn augmented_path() -> OsString {
    let mut directories: Vec<PathBuf> =
        std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for suffix in [".local/bin", ".aftman/bin", ".rokit/bin", ".cargo/bin"] {
            directories.push(home.join(suffix));
        }
    }
    directories.push(PathBuf::from("/opt/homebrew/bin"));
    directories.push(PathBuf::from("/usr/local/bin"));

    std::env::join_paths(directories.iter().filter(|directory| !directory.as_os_str().is_empty()))
        .unwrap_or_default()
}

fn find_node(path_value: &OsString) -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(path_value)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

// ------------------------------------------------------------------ status --

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStatus {
    /// Whether `WSync.rbxm` is in the plugins folder right now.
    pub(crate) installed: bool,
    /// `None` only when this platform has no plugins folder at all.
    pub(crate) plugins_dir: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) size: Option<u64>,
    /// Epoch milliseconds.
    pub(crate) modified_at: Option<u64>,
    /// sha256 of the installed bytes — what is actually in the folder, known
    /// whenever the file could be read, matched manifest or not.
    pub(crate) sha256: Option<String>,
    /// Identity of the installed file, when a manifest could be matched to it.
    pub(crate) plugin_version: Option<String>,
    pub(crate) protocol_version: Option<u64>,
    pub(crate) built_at: Option<String>,
    pub(crate) build_commit: Option<String>,
    pub(crate) build_dirty: Option<bool>,
    /// True when the installed bytes hash to the sha256 of a manifest this
    /// host could resolve — i.e. the installed plugin *is* the artifact this
    /// build ships, byte for byte.
    pub(crate) verified: bool,
    /// Older single-file plugins still sitting in the folder.
    pub(crate) stale: Vec<String>,
    /// What the served daemon's `/hello` reported, when one was reachable.
    pub(crate) daemon_protocol: Option<u64>,
    /// `Some(false)` only when both numbers are known and differ.
    pub(crate) protocol_match: Option<bool>,
    /// The one line to show in red when something needs doing.
    pub(crate) warning: Option<String>,
    pub(crate) note: Option<String>,
}

impl PluginStatus {
    fn nowhere(note: String) -> Self {
        Self {
            installed: false,
            plugins_dir: None,
            path: None,
            size: None,
            modified_at: None,
            sha256: None,
            plugin_version: None,
            protocol_version: None,
            built_at: None,
            build_commit: None,
            build_dirty: None,
            verified: false,
            stale: Vec::new(),
            daemon_protocol: None,
            protocol_match: None,
            warning: None,
            note: Some(note),
        }
    }
}

/// What is installed, whether it is the artifact this build ships, and whether
/// it agrees with a running daemon.
///
/// Never fails: this is what a settings panel renders, and a status line that
/// throws is less useful than one that says it does not know. `sources` is
/// consulted only to find a *manifest* — the identity of the installed bytes is
/// established by hashing them, not by trusting the file's presence.
pub(crate) fn status(
    plugins_dir: Option<&Path>,
    sources: &Sources,
    daemon_protocol: Option<u64>,
) -> PluginStatus {
    let Some(plugins_dir) = plugins_dir else {
        return PluginStatus::nowhere(
            "Roblox Studio does not run on this platform, so there is no plugins folder."
                .to_string(),
        );
    };

    let path = plugins_dir.join(ARTIFACT_NAME);
    let metadata = fs::metadata(&path).ok().filter(|meta| meta.is_file());
    let stale: Vec<String> = STALE_NAMES
        .iter()
        .filter(|name| plugins_dir.join(name).is_file())
        .map(|name| (*name).to_string())
        .collect();

    let mut status = PluginStatus {
        installed: metadata.is_some(),
        plugins_dir: Some(plugins_dir.display().to_string()),
        path: metadata.as_ref().map(|_| path.display().to_string()),
        size: metadata.as_ref().map(|meta| meta.len()),
        modified_at: metadata.as_ref().and_then(|meta| meta.modified().ok()).map(epoch_millis),
        sha256: None,
        plugin_version: None,
        protocol_version: None,
        built_at: None,
        build_commit: None,
        build_dirty: None,
        verified: false,
        stale,
        daemon_protocol,
        protocol_match: None,
        warning: None,
        note: None,
    };

    let mut notes: Vec<String> = Vec::new();

    // Resolved once: it locates both the manifest that identifies the installed
    // bytes and — when it points into a checkout — the sources whose freshness
    // the next install depends on.
    let resolved = resolve(sources).ok();

    if status.installed {
        // Identity comes from hashing what is actually installed against the
        // manifest this build can reach. The install copies only the artifact,
        // so there is no manifest in the plugins folder to read — and trusting
        // one there would mean trusting a file anything could have written.
        let source_manifest = match &resolved {
            Some(artifact) => manifest_beside(&artifact.path),
            None => Ok(None),
        };

        match (fs::read(&path), source_manifest) {
            (Ok(bytes), manifest) => {
                let digest = sha256_hex(&bytes);
                status.sha256 = Some(digest.clone());

                match manifest {
                    Ok(Some(manifest)) => {
                        if digest == manifest.sha256 {
                            status.verified = true;
                            status.plugin_version = manifest.plugin_version;
                            status.protocol_version = manifest.protocol_version;
                            status.built_at = manifest.built_at;
                            status.build_commit = manifest.build_commit;
                            status.build_dirty = Some(manifest.build_dirty);
                        } else {
                            notes.push(format!(
                                "The installed plugin is not the {ARTIFACT_NAME} this build ships. \
                                 Reinstall to replace it."
                            ));
                        }
                    }
                    Ok(None) => notes.push(format!(
                        "No {MANIFEST_NAME} is reachable, so the installed plugin's version cannot \
                         be confirmed."
                    )),
                    Err(error) => notes.push(error.message),
                }
            }
            (Err(error), _) => notes.push(format!("{} could not be read: {error}", path.display())),
        }
    }

    // A checkout whose sources moved past its artifact: the next Install
    // rebuilds first, and saying so here is what makes that unsurprising.
    if let Some(artifact) = &resolved {
        if artifact.origin == Origin::DevTree {
            if let Some(stale_tree) = dev_tree_stale(&artifact.path) {
                notes.push(format!(
                    "The checkout's plugin sources ({}) changed after {ARTIFACT_NAME} was built; \
                     Install will rebuild it first.",
                    stale_tree
                        .source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| stale_tree.source.display().to_string())
                ));
            }
        }
    }

    if !status.stale.is_empty() {
        notes.push(format!(
            "An older single-file plugin ({}) is still in the folder; installing removes it.",
            status.stale.join(", ")
        ));
    }

    // The mismatch that actually breaks a session: the plugin speaks one
    // protocol and the daemon another, so the WS hello is rejected and Studio
    // looks connected to nothing.
    if let (Some(plugin), Some(daemon)) = (status.protocol_version, daemon_protocol) {
        status.protocol_match = Some(plugin == daemon);
        if plugin != daemon {
            status.warning = Some(format!(
                "The installed plugin speaks protocol {plugin} and the running daemon speaks \
                 {daemon}. Rebuild the plugin and restart Studio."
            ));
        }
    }

    status.note = join_notes(notes);
    status
}

fn epoch_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

// ------------------------------------------------------------------- tests --

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree with an artifact and (optionally) the manifest that describes it.
    fn artifact_at(directory: &Path, bytes: &[u8], manifest: Option<&str>) -> PathBuf {
        fs::create_dir_all(directory).unwrap();
        let path = directory.join(ARTIFACT_NAME);
        fs::write(&path, bytes).unwrap();
        if let Some(manifest) = manifest {
            fs::write(directory.join(MANIFEST_NAME), manifest).unwrap();
        }
        path
    }

    fn manifest_for(bytes: &[u8]) -> String {
        format!(
            r#"{{"schemaVersion":1,"artifact":"{ARTIFACT_NAME}","sha256":"{}",
                "pluginVersion":"0.1.0","protocolVersion":1,
                "builtAt":"2026-08-10T11:14:00Z",
                "buildCommit":"a1b2c3d4e5f6","buildDirty":false}}"#,
            sha256_hex(bytes)
        )
    }

    // --- resolution order ---------------------------------------------------

    #[test]
    fn resources_win_over_everything_and_the_env_wins_over_the_dev_tree() {
        let root = tempfile::tempdir().unwrap();

        let resources = root.path().join("Resources");
        artifact_at(&resources, b"resources", None);
        let elsewhere = root.path().join("elsewhere");
        let env_artifact = artifact_at(&elsewhere, b"env", None);
        // A dev tree two levels below the working directory's start point.
        let checkout = root.path().join("checkout");
        artifact_at(&checkout.join("plugin"), b"dev-tree", None);
        let cwd = checkout.join("desktop").join("src-tauri");
        fs::create_dir_all(&cwd).unwrap();

        let all = Sources {
            resource_dirs: vec![resources.clone()],
            env_override: Some(env_artifact.clone().into_os_string()),
            cwd: Some(cwd.clone()),
        };
        let resolved = resolve(&all).unwrap();
        assert_eq!(resolved.origin, Origin::Resources);
        assert_eq!(resolved.path, resources.join(ARTIFACT_NAME));

        // No resources — the packaged case is the only one that outranks an
        // explicit override, so a dev build's override is honoured.
        let without_resources = Sources {
            resource_dirs: Vec::new(),
            ..all.clone()
        };
        let resolved = resolve(&without_resources).unwrap();
        assert_eq!(resolved.origin, Origin::Env);
        assert_eq!(resolved.path, env_artifact);

        // …and the dev tree is the last automatic source.
        let dev_only = Sources {
            resource_dirs: Vec::new(),
            env_override: None,
            cwd: Some(cwd),
        };
        let resolved = resolve(&dev_only).unwrap();
        assert_eq!(resolved.origin, Origin::DevTree);
        assert_eq!(resolved.path, checkout.join("plugin").join(ARTIFACT_NAME));
    }

    #[test]
    fn a_broken_override_is_reported_and_nothing_at_all_says_how_to_build_one() {
        let root = tempfile::tempdir().unwrap();

        let error = resolve(&Sources {
            env_override: Some(root.path().join("nope.rbxm").into_os_string()),
            ..Sources::default()
        })
        .unwrap_err();
        assert_eq!(error.code, crate::error::code::INVALID_ARGUMENT);
        assert!(error.message.contains(ARTIFACT_PATH_ENV), "{}", error.message);

        let error = resolve(&Sources {
            cwd: Some(root.path().to_path_buf()),
            ..Sources::default()
        })
        .unwrap_err();
        assert_eq!(error.code, crate::error::code::UNAVAILABLE);
        assert!(
            error.message.contains(BUILD_COMMAND),
            "the error must carry the command that fixes it: {}",
            error.message
        );
    }

    #[test]
    fn the_dev_tree_walk_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        artifact_at(&root.path().join("plugin"), b"dev-tree", None);

        // Just inside the bound.
        let near = root.path().join("a/b/c");
        fs::create_dir_all(&near).unwrap();
        assert!(dev_tree_artifact(Some(&near)).is_some());

        // Far enough below that climbing to it would mean climbing forever.
        let far = root.path().join("a/b/c/d/e/f/g/h");
        fs::create_dir_all(&far).unwrap();
        assert!(dev_tree_artifact(Some(&far)).is_none());
    }

    // --- the manifest -------------------------------------------------------

    #[test]
    fn a_manifest_parses_its_identity_and_normalizes_the_protocol() {
        let manifest = parse_manifest(manifest_for(b"bytes").as_bytes()).unwrap();
        assert_eq!(manifest.plugin_version.as_deref(), Some("0.1.0"));
        assert_eq!(manifest.protocol_version, Some(1));
        assert_eq!(manifest.built_at.as_deref(), Some("2026-08-10T11:14:00Z"));
        assert_eq!(manifest.build_commit.as_deref(), Some("a1b2c3d4e5f6"));
        assert!(!manifest.build_dirty);
        assert!(manifest.schema_note.is_none());

        // Both shapes the ecosystem writes a protocol in.
        for raw in [r#""protocolVersion":1"#, r#""protocolVersion":"wsync/1""#, r#""protocolVersion":"1""#] {
            let document = format!(
                r#"{{"sha256":"{}",{raw}}}"#,
                "a".repeat(64)
            );
            assert_eq!(
                parse_manifest(document.as_bytes()).unwrap().protocol_version,
                Some(1),
                "{raw}",
            );
        }

        // A newer schema is read, not refused: the document is additive.
        let newer = format!(r#"{{"schemaVersion":9,"sha256":"{}"}}"#, "b".repeat(64));
        let manifest = parse_manifest(newer.as_bytes()).unwrap();
        assert!(manifest.schema_note.unwrap().contains("schema 9"));
    }

    #[test]
    fn a_manifest_without_a_usable_digest_is_a_refusal_not_an_unverified_install() {
        // Every one of these would otherwise be a way to switch verification
        // off by damaging the manifest instead of the artifact.
        for document in [
            "{ not json",
            r#"{"pluginVersion":"0.1.0"}"#,
            r#"{"sha256":""}"#,
            r#"{"sha256":"abc"}"#,
            r#"{"sha256":"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"}"#,
        ] {
            let error = parse_manifest(document.as_bytes()).unwrap_err();
            assert_eq!(
                error.code,
                crate::error::code::CORRUPT_STATE,
                "{document} should be refused",
            );
        }

        // Case and surrounding space in an otherwise good digest are the
        // author's, not a reason to refuse.
        let manifest =
            parse_manifest(format!(r#"{{"sha256":" {} "}}"#, "AB".repeat(32)).as_bytes()).unwrap();
        assert_eq!(manifest.sha256, "ab".repeat(32));
    }

    // --- install ------------------------------------------------------------

    /// Every install test writes into a temp directory. There is no path in
    /// this file that can reach `~/Documents/Roblox/Plugins`.
    fn plugins() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_verified_artifact_installs_and_carries_its_identity() {
        let source = tempfile::tempdir().unwrap();
        let bytes = b"<roblox></roblox>".as_slice();
        let path = artifact_at(source.path(), bytes, Some(&manifest_for(bytes)));
        let plugins = plugins();

        let report = install(
            &Artifact {
                path,
                origin: Origin::DevTree,
            },
            plugins.path(),
        )
        .unwrap();

        assert!(report.installed);
        assert!(report.verified, "a matching sha256 is what verified means");
        assert_eq!(report.plugin_version.as_deref(), Some("0.1.0"));
        assert_eq!(report.protocol_version, Some(1));
        assert_eq!(report.sha256, sha256_hex(bytes), "the report carries the installed identity");
        assert_eq!(report.built_at.as_deref(), Some("2026-08-10T11:14:00Z"));
        assert_eq!(report.source, "dev-tree");
        assert!(report.note.is_none(), "nothing to warn about: {:?}", report.note);
        assert_eq!(fs::read(plugins.path().join(ARTIFACT_NAME)).unwrap(), bytes);
    }

    #[test]
    fn a_mismatched_sha_refuses_and_leaves_the_folder_untouched() {
        let source = tempfile::tempdir().unwrap();
        // A manifest that describes *different* bytes: exactly the shape of a
        // stale manifest beside a rebuilt artifact, or a tampered download.
        let path = artifact_at(source.path(), b"actual bytes", Some(&manifest_for(b"other bytes")));
        let plugins = plugins();

        let error = install(
            &Artifact {
                path,
                origin: Origin::Resources,
            },
            plugins.path(),
        )
        .unwrap_err();

        assert_eq!(error.code, crate::error::code::INTEGRITY);
        assert!(error.message.contains(BUILD_COMMAND), "{}", error.message);
        assert!(
            !plugins.path().join(ARTIFACT_NAME).exists(),
            "a refused install must not write anything",
        );
        // And nothing is left staged, either.
        assert_eq!(fs::read_dir(plugins.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_manifest_that_names_another_artifact_is_refused() {
        let source = tempfile::tempdir().unwrap();
        let bytes = b"bytes".as_slice();
        let path = artifact_at(
            source.path(),
            bytes,
            Some(&format!(
                r#"{{"artifact":"Other.rbxm","sha256":"{}"}}"#,
                sha256_hex(bytes)
            )),
        );
        let plugins = plugins();

        let error = install(
            &Artifact {
                path,
                origin: Origin::Picked,
            },
            plugins.path(),
        )
        .unwrap_err();
        assert_eq!(error.code, crate::error::code::INTEGRITY);
        assert!(!plugins.path().join(ARTIFACT_NAME).exists());
    }

    #[test]
    fn a_manifestless_artifact_installs_unverified_and_says_so() {
        let source = tempfile::tempdir().unwrap();
        let path = artifact_at(source.path(), b"hand built", None);
        let plugins = plugins();

        let report = install(
            &Artifact {
                path,
                origin: Origin::Picked,
            },
            plugins.path(),
        )
        .unwrap();

        assert!(report.installed);
        assert!(!report.verified);
        assert!(report.plugin_version.is_none());
        let note = report.note.expect("an unverified install must explain itself");
        assert!(note.contains(MANIFEST_NAME), "{note}");
    }

    #[test]
    fn a_corrupt_manifest_blocks_the_install_rather_than_downgrading_it() {
        let source = tempfile::tempdir().unwrap();
        let path = artifact_at(source.path(), b"bytes", Some("{ not json"));
        let plugins = plugins();

        let error = install(
            &Artifact {
                path,
                origin: Origin::DevTree,
            },
            plugins.path(),
        )
        .unwrap_err();
        assert_eq!(error.code, crate::error::code::CORRUPT_STATE);
        assert!(!plugins.path().join(ARTIFACT_NAME).exists());
    }

    #[test]
    fn installing_replaces_the_old_copy_atomically_and_removes_stale_single_files() {
        let source = tempfile::tempdir().unwrap();
        let bytes = b"new build".as_slice();
        let path = artifact_at(source.path(), bytes, Some(&manifest_for(bytes)));

        let plugins = plugins();
        fs::write(plugins.path().join(ARTIFACT_NAME), b"an older model").unwrap();
        fs::write(plugins.path().join("WSync.lua"), b"-- WSync 0.0.1").unwrap();
        fs::write(plugins.path().join("WSync.luau"), b"-- WSync 0.0.2").unwrap();
        // A neighbour that is none of WSync's business.
        fs::write(plugins.path().join("SomeoneElse.rbxm"), b"theirs").unwrap();

        let report = install(
            &Artifact {
                path,
                origin: Origin::Resources,
            },
            plugins.path(),
        )
        .unwrap();

        assert_eq!(fs::read(plugins.path().join(ARTIFACT_NAME)).unwrap(), bytes);
        assert!(!plugins.path().join("WSync.lua").exists());
        assert!(!plugins.path().join("WSync.luau").exists());
        assert!(plugins.path().join("SomeoneElse.rbxm").exists(), "only WSync's own files go");
        let note = report.note.expect("removing a stale plugin is worth reporting");
        assert!(note.contains("WSync.lua"), "{note}");

        // Temp+rename, so no half-written model and no leftovers for Studio to
        // find: the folder holds exactly the two files it should.
        let names: Vec<String> = fs::read_dir(plugins.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "left behind: {names:?}");
    }

    #[test]
    fn the_plugins_folder_is_created_when_it_does_not_exist_yet() {
        let source = tempfile::tempdir().unwrap();
        let bytes = b"first ever install".as_slice();
        let path = artifact_at(source.path(), bytes, Some(&manifest_for(bytes)));

        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("Documents/Roblox/Plugins");
        let report = install(
            &Artifact {
                path,
                origin: Origin::Resources,
            },
            &plugins,
        )
        .unwrap();
        assert!(report.installed);
        assert_eq!(report.path, plugins.join(ARTIFACT_NAME).display().to_string());
    }

    #[test]
    fn an_empty_or_absent_artifact_is_refused_before_anything_is_hashed() {
        let source = tempfile::tempdir().unwrap();
        let plugins = plugins();

        let empty = source.path().join(ARTIFACT_NAME);
        fs::write(&empty, b"").unwrap();
        let error = install(
            &Artifact {
                path: empty,
                origin: Origin::Picked,
            },
            plugins.path(),
        )
        .unwrap_err();
        assert_eq!(error.code, crate::error::code::INVALID_ARGUMENT);

        let error = install(
            &Artifact {
                path: source.path().join("gone.rbxm"),
                origin: Origin::Picked,
            },
            plugins.path(),
        )
        .unwrap_err();
        assert_eq!(error.code, crate::error::code::IO);
    }

    // --- status -------------------------------------------------------------

    #[test]
    fn status_reports_an_empty_folder_without_inventing_an_identity() {
        let plugins = plugins();
        let status = status(Some(plugins.path()), &Sources::default(), None);
        assert!(!status.installed);
        assert!(!status.verified);
        assert!(status.path.is_none());
        assert!(status.plugin_version.is_none());
        assert_eq!(status.plugins_dir.as_deref(), Some(plugins.path().display().to_string().as_str()));
    }

    #[test]
    fn status_identifies_an_installed_plugin_by_hashing_it_against_the_source_manifest() {
        let source = tempfile::tempdir().unwrap();
        let bytes = b"<roblox/>".as_slice();
        let path = artifact_at(source.path(), bytes, Some(&manifest_for(bytes)));
        let plugins = plugins();
        install(
            &Artifact {
                path,
                origin: Origin::DevTree,
            },
            plugins.path(),
        )
        .unwrap();

        let sources = Sources {
            resource_dirs: vec![source.path().to_path_buf()],
            ..Sources::default()
        };
        let status = status(Some(plugins.path()), &sources, None);
        assert!(status.installed);
        assert!(status.verified);
        assert_eq!(status.plugin_version.as_deref(), Some("0.1.0"));
        assert_eq!(status.protocol_version, Some(1));
        assert_eq!(status.sha256.as_deref(), Some(sha256_hex(bytes).as_str()));
        assert_eq!(status.built_at.as_deref(), Some("2026-08-10T11:14:00Z"));
        assert_eq!(status.build_commit.as_deref(), Some("a1b2c3d4e5f6"));
        assert_eq!(status.build_dirty, Some(false));
        assert_eq!(status.size, Some(bytes.len() as u64));
        assert!(status.modified_at.is_some());
        assert!(status.warning.is_none());
        assert!(status.note.is_none(), "{:?}", status.note);
    }

    #[test]
    fn status_says_when_the_installed_plugin_is_not_the_one_this_build_ships() {
        let plugins = plugins();
        fs::write(plugins.path().join(ARTIFACT_NAME), b"somebody else's build").unwrap();

        let source = tempfile::tempdir().unwrap();
        let bytes = b"our build".as_slice();
        artifact_at(source.path(), bytes, Some(&manifest_for(bytes)));
        let sources = Sources {
            resource_dirs: vec![source.path().to_path_buf()],
            ..Sources::default()
        };

        let status = status(Some(plugins.path()), &sources, None);
        assert!(status.installed);
        assert!(!status.verified, "different bytes are a different plugin");
        assert!(status.plugin_version.is_none(), "an unmatched file has no known version");
        assert!(status.note.unwrap().contains("Reinstall"));
    }

    #[test]
    fn status_flags_a_protocol_mismatch_against_a_running_daemon() {
        let source = tempfile::tempdir().unwrap();
        let bytes = b"<roblox/>".as_slice();
        let path = artifact_at(source.path(), bytes, Some(&manifest_for(bytes)));
        let plugins = plugins();
        install(&Artifact { path, origin: Origin::Resources }, plugins.path()).unwrap();
        let sources = Sources {
            resource_dirs: vec![source.path().to_path_buf()],
            ..Sources::default()
        };

        // Agreement is silent.
        let agreed = status(Some(plugins.path()), &sources, Some(1));
        assert_eq!(agreed.protocol_match, Some(true));
        assert!(agreed.warning.is_none());

        // Disagreement is the one thing worth shouting about: the plugin will
        // connect and be rejected, which looks like nothing happening.
        let mismatched = status(Some(plugins.path()), &sources, Some(2));
        assert_eq!(mismatched.protocol_match, Some(false));
        let warning = mismatched.warning.expect("a mismatch must warn");
        assert!(warning.contains("restart Studio"), "{warning}");
        assert!(warning.contains('1') && warning.contains('2'), "{warning}");

        // No daemon, or no known plugin protocol, is *unknown* rather than a
        // mismatch — a warning nobody can act on is noise.
        assert_eq!(status(Some(plugins.path()), &sources, None).protocol_match, None);
        assert_eq!(
            status(Some(plugins.path()), &Sources::default(), Some(2)).protocol_match,
            None,
        );
    }

    #[test]
    fn status_reports_stale_single_file_plugins_and_a_platform_with_no_folder() {
        let plugins = plugins();
        fs::write(plugins.path().join("WSync.luau"), b"-- old").unwrap();
        let installed = status(Some(plugins.path()), &Sources::default(), None);
        assert!(!installed.installed);
        assert_eq!(installed.stale, vec!["WSync.luau".to_string()]);
        assert!(installed.note.unwrap().contains("WSync.luau"));

        let nowhere = status(None, &Sources::default(), None);
        assert!(!nowhere.installed);
        assert!(nowhere.plugins_dir.is_none());
        assert!(nowhere.note.is_some());
    }

    // --- dev-tree staleness -------------------------------------------------

    fn set_modified(path: &Path, time: SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn a_dev_tree_artifact_is_stale_only_when_a_build_input_is_newer() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("plugin");
        fs::create_dir_all(plugin.join("src")).unwrap();
        let artifact = plugin.join(ARTIFACT_NAME);
        fs::write(&artifact, b"built").unwrap();
        let source = plugin.join("src").join("init.server.luau");
        fs::write(&source, b"-- source").unwrap();

        let hour = std::time::Duration::from_secs(3600);
        let base = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

        set_modified(&source, base);
        set_modified(&artifact, base + hour);
        assert!(
            dev_tree_stale(&artifact).is_none(),
            "an artifact newer than every input is fresh"
        );

        set_modified(&source, base + hour * 2);
        let stale = dev_tree_stale(&artifact).expect("an input newer than the artifact is stale");
        assert!(stale.source.ends_with("init.server.luau"), "{:?}", stale.source);

        // Build *outputs* do not count — a regenerated sourcemap beside the
        // artifact must not report the artifact stale forever.
        set_modified(&source, base);
        let sourcemap = plugin.join("sourcemap.json");
        fs::write(&sourcemap, b"{}").unwrap();
        set_modified(&sourcemap, base + hour * 3);
        assert!(dev_tree_stale(&artifact).is_none());
    }

    #[test]
    fn status_says_when_the_dev_tree_artifact_needs_a_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        let plugin_dir = checkout.join("plugin");
        fs::create_dir_all(plugin_dir.join("src")).unwrap();

        let bytes = b"<roblox/>".as_slice();
        let artifact = artifact_at(&plugin_dir, bytes, Some(&manifest_for(bytes)));
        let source = plugin_dir.join("src").join("init.server.luau");
        fs::write(&source, b"-- newer").unwrap();

        let hour = std::time::Duration::from_secs(3600);
        let base = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        set_modified(&artifact, base);
        set_modified(&source, base + hour);

        let plugins = plugins();
        install(
            &Artifact {
                path: artifact,
                origin: Origin::DevTree,
            },
            plugins.path(),
        )
        .unwrap();

        let sources = Sources {
            cwd: Some(checkout),
            ..Sources::default()
        };
        let status = status(Some(plugins.path()), &sources, None);
        assert!(status.verified, "staleness is about the *next* install, not this one");
        let note = status.note.expect("a stale checkout is worth a note");
        assert!(note.contains("rebuild"), "{note}");
        assert!(note.contains("init.server.luau"), "{note}");
    }

    // --- shared shapes ------------------------------------------------------

    #[test]
    fn the_protocol_from_a_hello_document_is_read_the_same_way() {
        assert_eq!(hello_protocol(&serde_json::json!({ "protocol": 1 })), Some(1));
        assert_eq!(hello_protocol(&serde_json::json!({ "protocol": "wsync/1" })), Some(1));
        assert_eq!(hello_protocol(&serde_json::json!({})), None);
        assert_eq!(hello_protocol(&serde_json::json!({ "protocol": null })), None);
    }

    #[test]
    fn the_digest_is_the_one_sha256sum_prints() {
        // The empty-string vector, so a swapped hash function would be caught.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn the_platform_plugins_folder_is_the_one_studio_reads() {
        // Shape only — this never creates it, and no other test in this file
        // so much as names it.
        let path = plugins_dir().unwrap();
        assert!(path.ends_with("Roblox/Plugins") || path.ends_with("Roblox\\Plugins"), "{path:?}");
        #[cfg(target_os = "macos")]
        assert!(path.to_string_lossy().contains("Documents"), "{path:?}");
    }
}
