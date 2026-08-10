use std::{env, fs, path::PathBuf};

fn main() {
    if let Ok(target) = env::var("TARGET") {
        println!("cargo:rustc-env=WSYNC_TARGET_TRIPLE={target}");
    }
    // The updater public key is compiled in from the environment at build time
    // (Design 8.5). Absent key => fail closed: the updater plugin is never
    // registered and `app_info.updaterConfigured` reports false.
    println!("cargo:rerun-if-env-changed=WSYNC_UPDATER_PUBLIC_KEY");

    ensure_sidecar_placeholders();
    stage_plugin_resources();
    tauri_build::build()
}

/// `bundle.externalBin` makes Tauri copy `binaries/wsync-<target-triple>` into
/// the build output, and the build fails outright when it is missing. The real
/// binary is produced by the workspace's own `cargo build -p wsync` and staged
/// here by the release script — it is never committed.
///
/// So that a fresh clone can `cargo check` and `cargo tauri dev` the shell
/// before the engine exists, a **debug** build stages an empty, clearly-labelled
/// placeholder and warns. A **release** build refuses — and a zero-byte file
/// left by an earlier debug build counts as missing, not staged: shipping an
/// installer around a fake sidecar is worse than failing the build.
fn ensure_sidecar_placeholders() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let config_path = manifest_dir.join("tauri.conf.json");
    println!("cargo:rerun-if-changed={}", config_path.display());

    let Ok(raw) = fs::read_to_string(&config_path) else {
        return;
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let Some(external) = config
        .get("bundle")
        .and_then(|bundle| bundle.get("externalBin"))
        .and_then(|value| value.as_array())
    else {
        return;
    };

    let target = env::var("TARGET").unwrap_or_default();
    let suffix = if target.contains("windows") { ".exe" } else { "" };
    let is_release = env::var("PROFILE").as_deref() == Ok("release");

    for entry in external {
        let Some(stem) = entry.as_str() else { continue };
        let expected = manifest_dir.join(format!("{stem}-{target}{suffix}"));
        println!("cargo:rerun-if-changed={}", expected.display());
        // A zero-byte file is the placeholder staged below. A debug build may
        // keep building around it; a release build must treat it as absent,
        // or a debug session's leftover ships inside the installer.
        let staged = fs::metadata(&expected).map(|meta| meta.len()).ok();
        let usable = if is_release { staged.unwrap_or(0) > 0 } else { staged.is_some() };
        if usable {
            continue;
        }

        let name = expected.file_name().unwrap_or_default().to_string_lossy();
        if is_release {
            let state = if staged.is_some() { "an empty placeholder" } else { "missing" };
            panic!(
                "the `{name}` sidecar is {state}.\n\
                 Build the engine and stage it before a release build:\n  \
                 cargo build --release -p wsync\n  \
                 cp target/release/wsync desktop/src-tauri/{stem}-{target}{suffix}"
            );
        }

        if let Some(parent) = expected.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&expected, b"").is_ok() {
            println!(
                "cargo:warning=staged an EMPTY placeholder for the `{name}` sidecar so the \
                 desktop shell can build; daemon lifecycle is not wired up in this wave. \
                 Stage the real binary before packaging."
            );
        }
    }
}

/// Where `bundle.resources` picks the Studio plugin up from.
const RESOURCE_DIR: &str = "resources";
/// The pair `plugin_install.rs` resolves: the artifact and the manifest that
/// says which artifact it is.
const PLUGIN_FILES: [&str; 2] = ["WSync.rbxm", "WSync.build.json"];

/// Stages the Studio plugin pair into `resources/` for `bundle.resources`.
///
/// `tauri.conf.json` maps the whole `resources/` **directory** to the root of
/// the app's resource directory, because that is the one form of the setting
/// that tolerates having nothing to ship: an empty directory is skipped, while a
/// named file that does not exist is a hard `ResourcePathNotFound` — and
/// `tauri_build::build()` resolves resources on every `cargo build` and `cargo
/// check`, not only when bundling. So the directory is always created, and what
/// goes in it depends on the profile:
///
/// **Release** must ship a verifiable plugin. Both files are required, the
/// manifest has to name this artifact and carry a usable sha256, and anything
/// else in the directory is refused rather than quietly bundled. A zero-byte or
/// missing manifest fails the build: `plugin_install.rs` installs a
/// manifest-less artifact in a `verified: false` state, which is a reasonable
/// thing for a hand-built dev tree and not a thing to ship inside an installer.
/// (The sha256 is *checked* against the bytes at install time by the app, and
/// again by `scripts/check-release-identity.mjs` before publishing; this is the
/// structural gate that stops a broken pair from getting that far.)
///
/// **Debug** stages nothing and clears anything a previous release build left
/// behind. Resources outrank the dev-tree lookup on purpose (module header in
/// `plugin_install.rs`), so a stale copy here would shadow the `plugin/`
/// artifact a developer just rebuilt — the checkout should always win in a
/// checkout.
fn stage_plugin_resources() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let resources = manifest_dir.join(RESOURCE_DIR);
    if let Err(error) = fs::create_dir_all(&resources) {
        panic!("could not create {}: {error}", resources.display());
    }

    // <repo>/desktop/src-tauri -> <repo>/plugin
    let source_dir = manifest_dir
        .parent()
        .and_then(|desktop| desktop.parent())
        .map(|root| root.join("plugin"))
        .expect("the desktop crate lives at <repo>/desktop/src-tauri");

    for file in PLUGIN_FILES {
        println!("cargo:rerun-if-changed={}", source_dir.join(file).display());
    }

    if env::var("PROFILE").as_deref() != Ok("release") {
        for file in PLUGIN_FILES {
            let _ = fs::remove_file(resources.join(file));
        }
        return;
    }

    let missing: Vec<&str> = PLUGIN_FILES
        .iter()
        .copied()
        .filter(|file| !source_dir.join(file).is_file())
        .collect();
    if !missing.is_empty() {
        panic!(
            "the Studio plugin is missing from plugin/ ({}).\n\
             A release build ships it as an app resource; build the pair first:\n  \
             node plugin/scripts/build.mjs",
            missing.join(", ")
        );
    }

    let artifact = source_dir.join(PLUGIN_FILES[0]);
    let manifest = source_dir.join(PLUGIN_FILES[1]);

    let artifact_bytes = fs::metadata(&artifact).map(|meta| meta.len()).unwrap_or(0);
    if artifact_bytes == 0 {
        panic!("{} is zero bytes — that is a placeholder, not a plugin", artifact.display());
    }

    let raw = fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("{} could not be read: {error}", manifest.display()));
    let document = serde_json::from_str::<serde_json::Value>(&raw)
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", manifest.display()));

    let named = document.get("artifact").and_then(|value| value.as_str());
    if named != Some(PLUGIN_FILES[0]) {
        panic!(
            "{} describes {:?}, not {:?} — refusing to ship an artifact its manifest does not name",
            manifest.display(),
            named.unwrap_or("<nothing>"),
            PLUGIN_FILES[0]
        );
    }

    let digest = document.get("sha256").and_then(|value| value.as_str()).unwrap_or_default();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        panic!(
            "{} carries no usable sha256, so the plugin would install unverified",
            manifest.display()
        );
    }

    for file in PLUGIN_FILES {
        let destination = resources.join(file);
        if let Err(error) = fs::copy(source_dir.join(file), &destination) {
            panic!("could not stage {}: {error}", destination.display());
        }
        // The staged copy is watched too (release only — in debug it is
        // deliberately absent, and cargo re-runs on a missing watched path).
        // Deleting or editing it must re-run this script: a fresh fingerprint
        // over an emptied directory would let `tauri build` bundle nothing.
        println!("cargo:rerun-if-changed={}", destination.display());
    }

    // Everything in the directory is bundled, so nothing unexpected may live in
    // it: a stray file here is a file inside the shipped app.
    if let Ok(entries) = fs::read_dir(&resources) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !PLUGIN_FILES.contains(&name.as_ref()) {
                panic!(
                    "{} would be bundled into the app: {}/ carries the Studio plugin pair and \
                     nothing else",
                    entry.path().display(),
                    RESOURCE_DIR
                );
            }
        }
    }
}
