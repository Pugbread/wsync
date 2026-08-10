use anyhow::Result;
use std::{
	env, fs,
	fs::File,
	path::PathBuf,
	process::{Command, Stdio},
};

/// Runs a git command in the manifest directory, returning trimmed stdout on
/// success and `None` when git is absent, the directory is not a repository,
/// or the command fails (e.g. a repository with no commits yet)
fn git_output(manifest_dir: &str, args: &[&str]) -> Option<String> {
	let output = Command::new("git")
		.args(args)
		.current_dir(manifest_dir)
		.stderr(Stdio::null())
		.output()
		.ok()?;

	if !output.status.success() {
		return None;
	}

	Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Stamps the build identity as compile-time environment for the version
/// surfaces (`/hello` reports `commit` and `dirty` from these)
fn stamp_build_identity(manifest_dir: &str) {
	let commit = git_output(manifest_dir, &["rev-parse", "--short=12", "HEAD"])
		.filter(|commit| !commit.is_empty())
		.unwrap_or_else(|| String::from("unknown"));

	// A repository with pending changes is a dirty build; outside git (or in
	// a repository git cannot answer for) the tree is reported clean
	let dirty = match git_output(manifest_dir, &["status", "--porcelain"]) {
		Some(status) => !status.is_empty(),
		None => false,
	};

	println!("cargo:rustc-env=WSYNC_BUILD_COMMIT={commit}");
	println!("cargo:rustc-env=WSYNC_BUILD_DIRTY={dirty}");

	// Re-stamp when the checked-out commit moves; dirtiness inherently lags
	// until the next rebuild, which is acceptable for a diagnostic surface
	let git_head = PathBuf::from(manifest_dir).join(".git").join("HEAD");

	if git_head.exists() {
		println!("cargo:rerun-if-changed={}", git_head.display());
	}
}

fn main() -> Result<()> {
	let manifest_dir = env::var("CARGO_MANIFEST_DIR")?;
	let out_path = PathBuf::from(env::var("OUT_DIR")?).join("WSync.rbxm");

	stamp_build_identity(&manifest_dir);

	// WSync never downloads the plugin at compile time (the Argon fork did).
	// With the `plugin` feature enabled the locally built plugin is bundled
	// into the binary when present, otherwise an empty placeholder is used
	// so that offline builds always succeed
	let local_plugin = PathBuf::from(&manifest_dir).join("plugin").join("WSync.rbxm");

	println!("cargo:rerun-if-changed=build.rs");

	if cfg!(feature = "plugin") {
		println!("cargo:rerun-if-changed={}", local_plugin.display());

		if local_plugin.exists() {
			fs::copy(local_plugin, out_path)?;
			return Ok(());
		}

		println!("cargo:warning=plugin/WSync.rbxm not found, building without a bundled plugin!");
	}

	File::create(out_path)?;

	Ok(())
}
