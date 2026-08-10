use anyhow::{bail, Result};
use colored::Colorize;
use include_dir::{include_dir, Dir};
use log::trace;
use rbx_dom_weak::{types::Variant, ustr};
use std::{
	env, fs,
	path::{Path, PathBuf},
};

use crate::{
	ext::PathExt,
	updater,
	util::{self, get_plugin_path},
	wsync_info,
};

const PLACE_TEMPLATE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/templates/place");
const PLUGIN_TEMPLATE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/templates/plugin");
const PACKAGE_TEMPLATE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/templates/package");
const MODEL_TEMPLATE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/templates/model");
const QUICK_TEMPLATE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/templates/quick");
const EMPTY_TEMPLATE: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/templates/empty");

const WSYNC_PLUGIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/WSync.rbxm"));

pub fn is_managed() -> bool {
	let path = match env::current_exe() {
		Ok(path) => path,
		Err(_) => return false,
	};

	!path.contains(&[".wsync", "bin"]) && (path.contains(&["bin"]) || path.contains(&["tool-storage"]))
}

/// Startup verification: templates and (optionally) the Studio plugin only.
/// The Argon fork's first-run self installer — an automatic copy into
/// `~/.wsync/bin` plus a default-yes self-delete of the invoked binary — was
/// removed deliberately (Design §15 phase 2 checklist e): PATH installation
/// is now the explicit, prompted `wsync install` command, which never
/// deletes its source
pub fn verify(with_plugin: bool) -> Result<()> {
	install_templates(false)?;

	if with_plugin {
		let plugin_path = get_plugin_path()?;

		// Unlike the explicit `wsync plugin install` command, startup
		// verification skips silently when no plugin source exists yet
		if !plugin_path.exists() && find_plugin_source().is_some() {
			install_plugin(&plugin_path, false)?;
		}
	}

	Ok(())
}

/// Local sources the WSync plugin can be installed from, in order of
/// preference: `plugin/WSync.rbxm` relative to the current workspace,
/// `WSync.rbxm` next to the running binary, and finally the binary bundled
/// with the `plugin` cargo feature. WSync never downloads the plugin from
/// GitHub (the Argon fork base did)
enum PluginSource {
	Local(PathBuf),
	Bundled,
}

fn find_plugin_source() -> Option<PluginSource> {
	if let Ok(workspace_plugin) = Path::new("plugin").join("WSync.rbxm").resolve() {
		if workspace_plugin.exists() {
			return Some(PluginSource::Local(workspace_plugin));
		}
	}

	if let Ok(exe_path) = env::current_exe() {
		let exe_plugin = exe_path.get_parent().join("WSync.rbxm");

		if exe_plugin.exists() {
			return Some(PluginSource::Local(exe_plugin));
		}
	}

	#[allow(clippy::const_is_empty)]
	if !WSYNC_PLUGIN.is_empty() {
		return Some(PluginSource::Bundled);
	}

	None
}

pub fn install_plugin(path: &Path, _show_progress: bool) -> Result<()> {
	fs::create_dir_all(path.get_parent())?;

	let contents = match find_plugin_source() {
		Some(PluginSource::Local(source)) => {
			trace!("Installing WSync plugin from {source:?}");
			fs::read(source)?
		}
		Some(PluginSource::Bundled) => {
			trace!("Installing WSync plugin from bundled binary");
			WSYNC_PLUGIN.to_vec()
		}
		None => bail!(
			"No WSync plugin found! Place the built plugin at {} in your workspace or next to the {} binary and try again",
			Path::new("plugin").join("WSync.rbxm").to_string().bold(),
			"wsync".bold()
		),
	};

	let version = plugin_version_from_bytes(&contents);

	fs::write(path, contents)?;

	wsync_info!("Installed WSync plugin, version: {}", version.bold());

	if path.contains(&["Roblox", "Plugins"]) {
		let mut status = updater::get_status()?;
		status.plugin_version = version;

		updater::set_status(&status)?;
	}

	Ok(())
}

pub fn install_templates(update: bool) -> Result<()> {
	let templates_dir = util::get_wsync_dir()?.join("templates");

	let place_template = templates_dir.join("place");
	let plugin_template = templates_dir.join("plugin");
	let package_template = templates_dir.join("package");
	let model_template = templates_dir.join("model");
	let quick_template = templates_dir.join("quick");
	let empty_template = templates_dir.join("empty");

	if update || !place_template.exists() {
		fs::create_dir_all(&place_template)?;
		install_template(&PLACE_TEMPLATE, &place_template)?;
	}

	if update || !plugin_template.exists() {
		fs::create_dir_all(&plugin_template)?;
		install_template(&PLUGIN_TEMPLATE, &plugin_template)?;
	}

	if update || !package_template.exists() {
		fs::create_dir_all(&package_template)?;
		install_template(&PACKAGE_TEMPLATE, &package_template)?;
	}

	if update || !model_template.exists() {
		fs::create_dir_all(&model_template)?;
		install_template(&MODEL_TEMPLATE, &model_template)?;
	}

	if update || !quick_template.exists() {
		fs::create_dir_all(&quick_template)?;
		install_template(&QUICK_TEMPLATE, &quick_template)?;
	}

	if update || !empty_template.exists() {
		fs::create_dir_all(&empty_template)?;
		install_template(&EMPTY_TEMPLATE, &empty_template)?;
	}

	Ok(())
}

fn install_template(template: &Dir, path: &Path) -> Result<()> {
	for file in template.files() {
		if file.path().get_name() != ".gitkeep" {
			fs::write(path.join(file.path()), file.contents())?;
		}
	}

	for dir in template.dirs() {
		fs::create_dir_all(path.join(dir.path()))?;
		install_template(dir, path)?;
	}

	Ok(())
}

pub fn get_plugin_version() -> String {
	plugin_version_from_bytes(WSYNC_PLUGIN)
}

fn plugin_version_from_bytes(plugin: &[u8]) -> String {
	// May seem hacky, but this function will only be
	// called once for most users and is non-critical anyway
	if let Ok(dom) = rbx_binary::from_reader(plugin) {
		for (_, instance) in dom.into_raw().1 {
			if instance.name == "manifest" && instance.class == "ModuleScript" {
				if let Some(Variant::String(source)) = instance.properties.get(&ustr("Source")) {
					let source = &source[source.find(r#"["version"] = ""#).unwrap_or(0) + 15..];
					return source[..source.find(r#"","#).unwrap_or(6)].to_owned();
				}
			}
		}
	}

	String::from("0.0.0")
}
