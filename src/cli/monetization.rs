//! `monetization` — Roblox game passes and developer products through Open
//! Cloud (monetization.json).
//!
//! Two kinds (`gamepass`, with the `gp`/`pass`/`gamepasses` aliases, and
//! `product`, with `dp`/`devproduct`) share one action surface: `discover`
//! (local-only reconnaissance — no credential, no network), `list`, `create`,
//! `edit`, `image`, and `images`. The cloud endpoints are the documented
//! game-passes v1 / developer-products v2 multipart surfaces; ids come back
//! as `gamePassId`/`productId` and a `PATCH` answering 200/204 is success.
//!
//! Universe discovery follows monetization.json: `--universe-id`, then the
//! `ROBLOX_UNIVERSE_ID`/`UNIVERSE_ID`/`GAMEID`/`GAME_ID` environment
//! variables, then the project env files, then the project file's `gameId`.
//! `discover` reports likely monetization config files by name but never
//! guesses project-specific code writes (the registry is explicit about
//! that), so it recommends — it does not edit.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use colored::Colorize;
use reqwest::blocking::multipart::{Form, Part};
use serde_json::{json, Value};
use std::{
	env, fs,
	path::{Path, PathBuf},
};

use crate::{
	cli::client::{clip, print_json},
	cli::cloud::{self, CloudClient},
	ext::PathExt,
	project::{self, Project},
	wsync_info, wsync_warn,
};

/// Filename tokens `discover` treats as monetization config candidates
const DISCOVER_EXTENSIONS: [&str; 6] = ["luau", "lua", "json", "toml", "yaml", "yml"];

/// Directory components the discover walk never descends into
const DISCOVER_SKIP: [&str; 8] = [
	"Packages",
	"_Index",
	"node_modules",
	".git",
	".vscode",
	".codex",
	"DevPackages",
	"ServerPackages",
];

/// Universe-id environment variables, in resolution order
const UNIVERSE_ENV_VARS: [&str; 4] = ["ROBLOX_UNIVERSE_ID", "UNIVERSE_ID", "GAMEID", "GAME_ID"];

/// Discover, list, create, edit, and upload images for game passes and
/// developer products through Open Cloud
#[derive(Parser)]
pub struct Monetization {
	#[command(subcommand)]
	kind: KindCommand,
}

#[derive(Subcommand)]
enum KindCommand {
	/// Game passes
	#[command(visible_aliases = ["gp", "pass", "gamepasses"])]
	Gamepass {
		#[command(subcommand)]
		action: Action,
	},
	/// Developer products
	#[command(visible_aliases = ["dp", "devproduct"])]
	Product {
		#[command(subcommand)]
		action: Action,
	},
}

#[derive(Subcommand)]
enum Action {
	/// Report the universe id, credential presence, and likely local
	/// monetization config files (local-only, no credential needed)
	Discover(Discover),
	/// List the universe's assets of this kind
	List(List),
	/// Create one or more assets, e.g. `"VIP 499 robux, Extra 99 robux"`
	Create(Create),
	/// Edit an existing asset's name, price, description, or sale state
	Edit(Edit),
	/// Upload one image for an existing asset
	Image(Image),
	/// Match a directory of images to assets by normalized filename and
	/// upload every match
	Images(Images),
}

impl Monetization {
	pub fn main(self) -> Result<()> {
		let (kind, action) = match self.kind {
			KindCommand::Gamepass { action } => (Kind::Gamepass, action),
			KindCommand::Product { action } => (Kind::Product, action),
		};

		match action {
			Action::Discover(command) => command.main(kind),
			Action::List(command) => command.main(kind),
			Action::Create(command) => command.main(kind),
			Action::Edit(command) => command.main(kind),
			Action::Image(command) => command.main(kind),
			Action::Images(command) => command.main(kind),
		}
	}
}

// ---------------------------------------------------------------------------
// Kind specifics — endpoints, field names, id keys
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq)]
enum Kind {
	Gamepass,
	Product,
}

impl Kind {
	fn noun(self) -> &'static str {
		match self {
			Kind::Gamepass => "game pass",
			Kind::Product => "developer product",
		}
	}

	fn key(self) -> &'static str {
		match self {
			Kind::Gamepass => "gamepass",
			Kind::Product => "product",
		}
	}

	fn collection_path(self, universe: u64) -> String {
		match self {
			Kind::Gamepass => format!("/game-passes/v1/universes/{universe}/game-passes"),
			Kind::Product => format!("/developer-products/v2/universes/{universe}/developer-products"),
		}
	}

	fn list_path(self, universe: u64) -> String {
		format!("{}/creator", self.collection_path(universe))
	}

	fn item_path(self, universe: u64, id: &str) -> String {
		format!("{}/{id}", self.collection_path(universe))
	}

	/// The multipart field carrying image bytes on create / on update —
	/// game passes use `imageFile` to create but `file` to update
	fn image_field(self, update: bool) -> &'static str {
		match (self, update) {
			(Kind::Gamepass, true) => "file",
			_ => "imageFile",
		}
	}

	fn id_keys(self) -> &'static [&'static str] {
		match self {
			Kind::Gamepass => &["gamePassId", "id"],
			Kind::Product => &["productId", "developerProductId", "id"],
		}
	}

	/// Filename tokens that mark a file as a likely config for this kind
	fn discover_tokens(self) -> &'static [&'static str] {
		match self {
			Kind::Gamepass => &["gamepass", "game-pass", "game_pass", "monetization"],
			Kind::Product => &[
				"devproduct",
				"developerproduct",
				"developer-product",
				"developer_product",
				"dev-product",
				"product",
				"monetization",
			],
		}
	}
}

// ---------------------------------------------------------------------------
// Shared targeting: project, universe, credential
// ---------------------------------------------------------------------------

#[derive(Args)]
struct Common {
	/// Project path (supplies the default universe id and env files)
	#[arg(long, value_name = "PATH")]
	project: Option<PathBuf>,

	/// Universe (game) id override
	#[arg(long = "universe-id", value_name = "ID")]
	universe_id: Option<u64>,

	/// Environment variable to read the API key from (after the auth store)
	#[arg(long = "api-key-env", value_name = "NAME")]
	api_key_env: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// The resolved cloud context for one action
struct Context_ {
	universe: u64,
	client: CloudClient,
}

impl Common {
	fn project_path(&self) -> Result<PathBuf> {
		project::resolve(self.project.clone().unwrap_or_default())
	}

	fn workspace(&self) -> Result<PathBuf> {
		Ok(self.project_path()?.get_parent().to_owned())
	}

	/// The universe id and where it came from (monetization.json order)
	fn resolve_universe(&self) -> Result<(u64, String)> {
		if let Some(universe) = self.universe_id {
			return Ok((universe, "--universe-id".to_owned()));
		}

		for name in UNIVERSE_ENV_VARS {
			if let Some(universe) = env::var(name).ok().and_then(|value| value.trim().parse::<u64>().ok()) {
				return Ok((universe, format!("environment variable {name}")));
			}
		}

		let workspace = self.workspace()?;

		if let Some((value, key, file)) = cloud::env_file_value(&workspace, &UNIVERSE_ENV_VARS) {
			if let Ok(universe) = value.trim().parse::<u64>() {
				return Ok((universe, format!("{key} in {}", file.display())));
			}
		}

		let project_path = self.project_path()?;

		if project_path.exists() {
			if let Some(game_id) = Project::load(&project_path).ok().and_then(|project| project.game_id) {
				return Ok((game_id, format!("gameId in {}", project_path.get_name())));
			}
		}

		bail!(
			"No universe id found — pass {}, set {}, or set `gameId` in the project file",
			"--universe-id <id>".bold(),
			UNIVERSE_ENV_VARS.join("/")
		)
	}

	fn connect(&self) -> Result<Context_> {
		let (universe, _) = self.resolve_universe()?;
		let credential = cloud::resolve_credential(self.api_key_env.as_deref(), Some(&self.workspace()?))?;

		Ok(Context_ {
			universe,
			client: CloudClient::new(credential, false)?,
		})
	}
}

// ---------------------------------------------------------------------------
// discover — local-only
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct Discover {
	#[command(flatten)]
	common: Common,
}

impl Discover {
	fn main(self, kind: Kind) -> Result<()> {
		let project_path = self.common.project_path()?;

		if !project_path.exists() {
			bail!(
				"No project file at {} — `discover` reads the project for its ids",
				project_path.to_string().bold()
			);
		}

		let workspace = project_path.get_parent().to_owned();
		let universe = self.common.resolve_universe().ok();
		let credential = cloud::find_credential(self.common.api_key_env.as_deref(), Some(&workspace))?;

		let mut files: Vec<PathBuf> = Vec::new();

		scan(&workspace, kind.discover_tokens(), &mut files, 0);
		files.sort();
		files.truncate(50);

		let relative: Vec<String> = files
			.iter()
			.map(|file| {
				file.strip_prefix(&workspace)
					.unwrap_or(file)
					.to_string_lossy()
					.into_owned()
			})
			.collect();

		if self.common.raw {
			print_json(&json!({
				"ok": true,
				"kind": kind.key(),
				"workspace": workspace.to_string(),
				"universe": {
					"id": universe.as_ref().map(|(id, _)| *id),
					"source": universe.as_ref().map(|(_, source)| source.clone()),
				},
				"credential": {
					"configured": credential.is_some(),
					"source": credential.as_ref().map(|credential| credential.source.clone()),
				},
				"files": relative,
			}));

			return Ok(());
		}

		match &universe {
			Some((id, source)) => println!("Universe    {} (from {source})", id.to_string().bold()),
			None => println!("Universe    unresolved — pass --universe-id or set gameId in the project file"),
		}

		match &credential {
			Some(credential) => println!("Credential  configured ({})", credential.source),
			None => println!("Credential  not configured — `wsync auth set` stores one"),
		}

		if relative.is_empty() {
			println!("Config      no likely {} config files found", kind.noun());
		} else {
			println!("Config      {} likely file(s):", relative.len());

			for file in &relative {
				println!("            {file}");
			}
		}

		wsync_info!(
			"Local reconnaissance only — `wsync monetization {} list` reads the cloud side",
			kind.key()
		);

		Ok(())
	}
}

/// Filename-based candidate scan, bounded to 6 directory levels and the
/// non-vendor tree. Content is never parsed: the registry is explicit that
/// `discover` does not guess project-specific schemas
fn scan(dir: &Path, tokens: &[&str], found: &mut Vec<PathBuf>, depth: u8) {
	if depth > 6 || found.len() >= 200 {
		return;
	}

	let Ok(entries) = fs::read_dir(dir) else { return };

	for entry in entries.filter_map(Result::ok) {
		let path = entry.path();
		let name = path.get_name().to_owned();

		if path.is_dir() {
			let vendor = DISCOVER_SKIP.iter().any(|skip| name.eq_ignore_ascii_case(skip))
				|| name.starts_with(".wsync-")
				|| name.starts_with('.');

			if !vendor {
				scan(&path, tokens, found, depth + 1);
			}

			continue;
		}

		if !DISCOVER_EXTENSIONS.contains(&path.get_ext().to_ascii_lowercase().as_str()) {
			continue;
		}

		let lower = name.to_ascii_lowercase();

		if tokens.iter().any(|token| lower.contains(token)) {
			found.push(path);
		}
	}
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct List {
	#[command(flatten)]
	common: Common,
}

impl List {
	fn main(self, kind: Kind) -> Result<()> {
		let context = self.common.connect()?;
		let items = fetch_items(&context, kind)?;

		if self.common.raw {
			print_json(&json!({
				"ok": true,
				"kind": kind.key(),
				"universeId": context.universe,
				"count": items.len(),
				"items": items,
			}));

			return Ok(());
		}

		if items.is_empty() {
			wsync_info!("Universe {} has no {}s", context.universe, kind.noun());

			return Ok(());
		}

		println!("{:<14} {:<32} {:>8}  FOR SALE", "ID", "NAME", "PRICE");

		for item in &items {
			println!(
				"{:<14} {:<32} {:>8}  {}",
				cloud::field_text(item, kind.id_keys()).unwrap_or_default(),
				clip(
					&cloud::field_text(item, &["name", "Name", "displayName"]).unwrap_or_default(),
					32
				),
				cloud::field_text(item, &["price", "priceInRobux", "PriceInRobux"]).unwrap_or_default(),
				item.get("isForSale")
					.or_else(|| item.get("IsForSale"))
					.map_or_else(|| "?".to_owned(), |sale| sale.to_string()),
			);
		}

		wsync_info!("{} {}(s) in universe {}", items.len(), kind.noun(), context.universe);

		Ok(())
	}
}

fn fetch_items(context: &Context_, kind: Kind) -> Result<Vec<Value>> {
	let response = context.client.get(&kind.list_path(context.universe))?;

	if !response.success() {
		bail!("Listing {}s failed ({})", kind.noun(), response.error_message());
	}

	Ok(cloud::extract_items(&response.value()))
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct Create {
	/// Entries like `"VIP 499 robux"`; commas separate multiple entries
	#[arg(value_name = "ENTRY")]
	entries: Vec<String>,

	/// Name for a single asset (with --price, instead of entries)
	#[arg(long, value_name = "NAME", conflicts_with = "entries")]
	name: Option<String>,

	/// Price in Robux for a single asset (with --name)
	#[arg(long, value_name = "ROBUX", requires = "name", conflicts_with = "entries")]
	price: Option<u64>,

	/// Description for every created asset
	#[arg(long, value_name = "TEXT")]
	description: Option<String>,

	/// Image uploaded with every created asset
	#[arg(long, value_name = "FILE")]
	image: Option<PathBuf>,

	/// Create off-sale (price becomes optional)
	#[arg(long = "not-for-sale")]
	not_for_sale: bool,

	#[command(flatten)]
	common: Common,
}

impl Create {
	fn main(self, kind: Kind) -> Result<()> {
		let entries = if let Some(name) = &self.name {
			vec![(name.clone(), self.price)]
		} else if self.entries.is_empty() {
			bail!(
				"Nothing to create — pass entries like {} or use --name with --price",
				"\"VIP 499 robux\"".bold()
			);
		} else {
			parse_entries(&self.entries, self.not_for_sale)?
		};

		let image = match &self.image {
			Some(image) => Some(read_image(image)?),
			None => None,
		};

		let context = self.common.connect()?;
		let mut failed = 0usize;

		for (name, price) in &entries {
			let record = match self.create_one(&context, kind, name, *price, image.as_ref()) {
				Ok(record) => record,
				Err(err) => {
					failed += 1;

					json!({ "ok": false, "name": name, "error": err.to_string() })
				}
			};

			if self.common.raw {
				print_json(&record);
			} else if record["ok"] == true {
				wsync_info!(
					"Created {} {} (id {}{})",
					kind.noun(),
					name.bold(),
					record["id"].as_str().unwrap_or_default().bold(),
					price.map_or_else(|| ", not for sale".to_owned(), |price| format!(", {price} Robux"))
				);
			} else {
				wsync_warn!("Failed to create {} — {}", name.bold(), record["error"]);
			}
		}

		if failed > 0 {
			bail!("{failed} of {} create(s) failed", entries.len());
		}

		Ok(())
	}

	fn create_one(
		&self,
		context: &Context_,
		kind: Kind,
		name: &str,
		price: Option<u64>,
		image: Option<&(Vec<u8>, String, &'static str)>,
	) -> Result<Value> {
		let mut form = Form::new()
			.text("name", name.to_owned())
			.text("isForSale", if self.not_for_sale { "false" } else { "true" });

		if let Some(price) = price {
			form = form.text("price", price.to_string());
		}

		if let Some(description) = &self.description {
			form = form.text("description", description.clone());
		}

		if let Some((bytes, file_name, content_type)) = image {
			form = form.part(
				kind.image_field(false),
				Part::bytes(bytes.clone())
					.file_name(file_name.clone())
					.mime_str(content_type)?,
			);
		}

		let response = context
			.client
			.post_multipart(&kind.collection_path(context.universe), form)?;

		if !response.success() {
			bail!("{}", response.error_message());
		}

		let value = response.value();
		let id = cloud::field_text(&value, kind.id_keys()).unwrap_or_default();

		Ok(json!({
			"ok": true,
			"kind": kind.key(),
			"universeId": context.universe,
			"name": name,
			"price": price,
			"forSale": !self.not_for_sale,
			"id": id,
			"response": value,
		}))
	}
}

/// Parses `"Name 499 robux"` entries, splitting each argument on commas.
/// The trailing `robux` token is optional; the price is the last numeric
/// token; everything before it is the name. Price-less entries are only
/// legal with `--not-for-sale`
fn parse_entries(args: &[String], not_for_sale: bool) -> Result<Vec<(String, Option<u64>)>> {
	let mut entries = Vec::new();

	for arg in args {
		for piece in arg.split(',') {
			let piece = piece.trim();

			if piece.is_empty() {
				continue;
			}

			let mut tokens: Vec<&str> = piece.split_whitespace().collect();

			if tokens
				.last()
				.is_some_and(|last| last.eq_ignore_ascii_case("robux") || last.eq_ignore_ascii_case("r$"))
			{
				tokens.pop();
			}

			let price = tokens.last().and_then(|last| last.parse::<u64>().ok());

			let name = match price {
				Some(_) => {
					tokens.pop();

					tokens.join(" ")
				}
				None => tokens.join(" "),
			};

			if name.is_empty() {
				bail!("Entry `{piece}` has no name — the format is `<name> <price> robux`");
			}

			if price.is_none() && !not_for_sale {
				bail!(
					"Entry `{piece}` has no price — the format is `<name> <price> robux` \
					 (price-less entries need --not-for-sale)"
				);
			}

			entries.push((name, price));
		}
	}

	if entries.is_empty() {
		bail!("No entries to create");
	}

	Ok(entries)
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct Edit {
	/// Numeric asset id
	#[arg(long, value_name = "ID", conflicts_with = "name")]
	id: Option<u64>,

	/// Existing asset name (resolved through the list endpoint)
	#[arg(long, value_name = "NAME")]
	name: Option<String>,

	/// New name
	#[arg(long = "new-name", value_name = "NAME")]
	new_name: Option<String>,

	/// New price in Robux
	#[arg(long, value_name = "ROBUX")]
	price: Option<u64>,

	/// New description
	#[arg(long, value_name = "TEXT")]
	description: Option<String>,

	/// Put the asset on or off sale
	#[arg(long = "for-sale", value_name = "true|false")]
	for_sale: Option<bool>,

	#[command(flatten)]
	common: Common,
}

impl Edit {
	fn main(self, kind: Kind) -> Result<()> {
		if self.new_name.is_none() && self.price.is_none() && self.description.is_none() && self.for_sale.is_none() {
			bail!("Nothing to edit — pass at least one of --new-name, --price, --description, --for-sale");
		}

		let context = self.common.connect()?;
		let id = resolve_id(&context, kind, self.id, self.name.as_deref())?;

		let mut form = Form::new();

		if let Some(name) = &self.new_name {
			form = form.text("name", name.clone());
		}

		if let Some(price) = self.price {
			form = form.text("price", price.to_string());
		}

		if let Some(description) = &self.description {
			form = form.text("description", description.clone());
		}

		if let Some(for_sale) = self.for_sale {
			form = form.text("isForSale", for_sale.to_string());
		}

		let response = context
			.client
			.patch_multipart(&kind.item_path(context.universe, &id), form)?;

		if !response.success() {
			bail!("Editing {} {id} failed ({})", kind.noun(), response.error_message());
		}

		if self.common.raw {
			print_json(&json!({
				"ok": true,
				"kind": kind.key(),
				"universeId": context.universe,
				"id": id,
				"changed": {
					"name": self.new_name,
					"price": self.price,
					"description": self.description.is_some(),
					"forSale": self.for_sale,
				},
			}));

			return Ok(());
		}

		wsync_info!("Edited {} {}", kind.noun(), id.bold());

		Ok(())
	}
}

/// `--id` verbatim, or `--name` resolved through the list endpoint. An
/// ambiguous name is refused with the candidate ids, never guessed
fn resolve_id(context: &Context_, kind: Kind, id: Option<u64>, name: Option<&str>) -> Result<String> {
	if let Some(id) = id {
		return Ok(id.to_string());
	}

	let Some(name) = name else {
		bail!("Pass --id <id> or --name <existing-name> to select the {}", kind.noun());
	};

	let items = fetch_items(context, kind)?;

	let matches: Vec<&Value> = items
		.iter()
		.filter(|item| {
			cloud::field_text(item, &["name", "Name", "displayName"]).is_some_and(|candidate| candidate == name)
		})
		.collect();

	match matches.len() {
		0 => bail!(
			"No {} named {} in universe {} ({} listed)",
			kind.noun(),
			name.bold(),
			context.universe,
			items.len()
		),
		1 => cloud::field_text(matches[0], kind.id_keys())
			.with_context(|| format!("The listed {} has no id field", kind.noun())),
		_ => {
			let ids: Vec<String> = matches
				.iter()
				.filter_map(|item| cloud::field_text(item, kind.id_keys()))
				.collect();

			bail!(
				"{} {}s are named {} — pass --id ({})",
				matches.len(),
				kind.noun(),
				name.bold(),
				ids.join(", ")
			)
		}
	}
}

// ---------------------------------------------------------------------------
// image / images
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct Image {
	/// Image file to upload
	#[arg(value_name = "FILE")]
	file: PathBuf,

	/// Numeric asset id
	#[arg(long, value_name = "ID", conflicts_with = "name")]
	id: Option<u64>,

	/// Existing asset name (resolved through the list endpoint)
	#[arg(long, value_name = "NAME")]
	name: Option<String>,

	#[command(flatten)]
	common: Common,
}

impl Image {
	fn main(self, kind: Kind) -> Result<()> {
		let image = read_image(&self.file)?;
		let context = self.common.connect()?;
		let id = resolve_id(&context, kind, self.id, self.name.as_deref())?;

		upload_image(&context, kind, &id, &image)?;

		if self.common.raw {
			print_json(&json!({
				"ok": true,
				"kind": kind.key(),
				"universeId": context.universe,
				"id": id,
				"file": self.file.resolve()?.to_string(),
			}));

			return Ok(());
		}

		wsync_info!("Uploaded {} for {} {}", image.1.bold(), kind.noun(), id.bold());

		Ok(())
	}
}

#[derive(Parser)]
struct Images {
	/// Directory of images matched to assets by normalized filename
	#[arg(value_name = "DIR")]
	dir: PathBuf,

	#[command(flatten)]
	common: Common,
}

impl Images {
	fn main(self, kind: Kind) -> Result<()> {
		let dir = self.dir.resolve()?;

		if !dir.is_dir() {
			bail!("{} is not a directory", dir.to_string().bold());
		}

		let mut files: Vec<PathBuf> = fs::read_dir(&dir)
			.with_context(|| format!("Failed to read {}", dir.to_string()))?
			.filter_map(Result::ok)
			.map(|entry| entry.path())
			.filter(|path| path.is_file() && image_content_type(path).is_some())
			.collect();

		files.sort();

		if files.is_empty() {
			bail!("No image files (png/jpg/jpeg/bmp/tga) in {}", dir.to_string().bold());
		}

		let context = self.common.connect()?;
		let items = fetch_items(&context, kind)?;

		let mut matched = 0usize;
		let mut failed = 0usize;
		let mut records: Vec<Value> = Vec::new();

		for file in &files {
			let normalized = normalize(file.get_stem());

			let target = items.iter().find(|item| {
				cloud::field_text(item, &["name", "Name", "displayName"])
					.is_some_and(|name| normalize(&name) == normalized)
			});

			let record = match target {
				Some(item) => {
					let id = cloud::field_text(item, kind.id_keys()).unwrap_or_default();
					let name = cloud::field_text(item, &["name", "Name", "displayName"]).unwrap_or_default();

					match read_image(file).and_then(|image| upload_image(&context, kind, &id, &image)) {
						Ok(()) => {
							matched += 1;

							json!({ "ok": true, "file": file.to_string(), "id": id, "name": name })
						}
						Err(err) => {
							failed += 1;

							json!({ "ok": false, "file": file.to_string(), "id": id, "error": err.to_string() })
						}
					}
				}
				None => json!({ "ok": true, "file": file.to_string(), "status": "unmatched" }),
			};

			if self.common.raw {
				print_json(&record);
			} else if record["status"] == "unmatched" {
				wsync_warn!("No {} matches {}", kind.noun(), file.get_name());
			} else if record["ok"] == true {
				wsync_info!("Uploaded {} → {} {}", file.get_name().bold(), kind.noun(), record["id"]);
			} else {
				wsync_warn!("Failed {} — {}", file.get_name().bold(), record["error"]);
			}

			records.push(record);
		}

		if !self.common.raw {
			wsync_info!(
				"{matched} uploaded, {failed} failed, {} unmatched of {} image(s)",
				files.len() - matched - failed,
				files.len()
			);
		}

		if failed > 0 {
			bail!("{failed} image upload(s) failed");
		}

		if matched == 0 {
			bail!(
				"No image matched any {} name — files match by normalized name, e.g. coins-small.png → \"Coins Small\"",
				kind.noun()
			);
		}

		Ok(())
	}
}

fn upload_image(context: &Context_, kind: Kind, id: &str, image: &(Vec<u8>, String, &'static str)) -> Result<()> {
	let (bytes, file_name, content_type) = image;

	let form = Form::new().part(
		kind.image_field(true),
		Part::bytes(bytes.clone())
			.file_name(file_name.clone())
			.mime_str(content_type)?,
	);

	let response = context
		.client
		.patch_multipart(&kind.item_path(context.universe, id), form)?;

	if !response.success() {
		bail!("{}", response.error_message());
	}

	Ok(())
}

fn image_content_type(path: &Path) -> Option<&'static str> {
	match path.get_ext().to_ascii_lowercase().as_str() {
		"png" => Some("image/png"),
		"jpg" | "jpeg" => Some("image/jpeg"),
		"bmp" => Some("image/bmp"),
		"tga" => Some("image/tga"),
		_ => None,
	}
}

/// Image bytes plus the filename and content type the part is tagged with
fn read_image(path: &Path) -> Result<(Vec<u8>, String, &'static str)> {
	let path = path.resolve()?;

	let content_type = image_content_type(&path)
		.with_context(|| format!("{} is not an image file (png/jpg/jpeg/bmp/tga)", path.to_string()))?;

	let bytes = fs::read(&path).with_context(|| format!("Failed to read {}", path.to_string()))?;

	Ok((bytes, path.get_name().to_owned(), content_type))
}

/// The filename↔asset-name normalization: lowercase alphanumerics only, so
/// `coins-small.png` matches `Coins Small`
fn normalize(text: &str) -> String {
	text.chars()
		.filter(|character| character.is_ascii_alphanumeric())
		.map(|character| character.to_ascii_lowercase())
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn entries_parse_names_prices_and_commas() {
		let entries = parse_entries(&["VIP 499 robux".to_owned()], false).unwrap();

		assert_eq!(entries, vec![("VIP".to_owned(), Some(499))]);

		let entries = parse_entries(&["Coins Small 49 robux, Coins Large 399 robux".to_owned()], false).unwrap();

		assert_eq!(
			entries,
			vec![
				("Coins Small".to_owned(), Some(49)),
				("Coins Large".to_owned(), Some(399)),
			]
		);

		// `robux` is optional; the price is the last numeric token
		assert_eq!(
			parse_entries(&["Mega Pack 1000".to_owned()], false).unwrap(),
			vec![("Mega Pack".to_owned(), Some(1000))]
		);

		// Price-less entries need --not-for-sale
		assert!(parse_entries(&["Just A Name".to_owned()], false).is_err());
		assert_eq!(
			parse_entries(&["Just A Name".to_owned()], true).unwrap(),
			vec![("Just A Name".to_owned(), None)]
		);

		assert!(parse_entries(&["499 robux".to_owned()], false).is_err());
		assert!(parse_entries(&[",".to_owned()], false).is_err());
	}

	#[test]
	fn normalization_matches_files_to_names() {
		assert_eq!(normalize("coins-small"), normalize("Coins Small"));
		assert_eq!(normalize("VIP_Pass!"), "vippass");
		assert_ne!(normalize("coins-small"), normalize("Coins Large"));
	}

	#[test]
	fn image_fields_differ_between_create_and_update() {
		assert_eq!(Kind::Gamepass.image_field(false), "imageFile");
		assert_eq!(Kind::Gamepass.image_field(true), "file");
		assert_eq!(Kind::Product.image_field(false), "imageFile");
		assert_eq!(Kind::Product.image_field(true), "imageFile");
	}
}
