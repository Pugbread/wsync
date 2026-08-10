//! `capture` — transparent UI, model/scene, and viewport captures through the
//! plugin's permission-free internal renderer (capture.json).
//!
//! The plugin renders and serves tightly packed RGBA8; everything else is the
//! CLI's job: every option is validated here before a socket is opened (axis
//! and total-pixel limits, padding/fov ranges, camera-CFrame arity and
//! finiteness, per-kind option applicability via the per-kind argument
//! structs), the pixel pump re-checks length and SHA-256, the PNG is encoded
//! locally and then *decoded back* from the written file before it is moved
//! into place — so the artifact on disk is proven readable, and pixels never
//! touch stdout. `capture_close` runs even when the pump or the encode fails,
//! so the plugin never holds a dead render.
//!
//! Prepare waits out the render with a 60 s default deadline (`--timeout`
//! overrides); each chunk read keeps the surface-wide 5 s default.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use colored::Colorize;
use serde_json::{json, Map, Value};
use std::{fs, io::BufWriter, path::PathBuf, process};

use crate::{
	cli::client::{print_json, Client, Targeting},
	cli::live::transfer::{self, human_size, sha256_hex},
	ext::PathExt,
	wsync_info,
};

/// Per-axis output limit (capture.json)
const MAX_AXIS: u32 = 4096;

/// Total-pixel output limit (capture.json)
const MAX_PIXELS: u64 = 16_777_216;

/// How long `capture_prepare` may render before the daemon times it out —
/// deliberately far above the 5 s op default, because a 4096² scene render
/// legitimately takes a while
const PREPARE_TIMEOUT_MS: u64 = 60_000;

/// Capture transparent UI, a model or scene, or the current 3D viewport as a
/// locally verified PNG
#[derive(Parser)]
pub struct Capture {
	#[command(subcommand)]
	command: CaptureCommand,
}

#[derive(Subcommand)]
enum CaptureCommand {
	/// Capture edit-mode ScreenGui pixels on a transparent background
	Ui(CaptureUi),
	/// Capture a model or scene through an isolated focused render
	Model(CaptureModel),
	/// Capture the complete current 3D viewport
	Viewport(CaptureViewport),
}

impl Capture {
	pub fn main(self) -> Result<()> {
		match self.command {
			CaptureCommand::Ui(command) => command.main(),
			CaptureCommand::Model(command) => command.main(),
			CaptureCommand::Viewport(command) => command.main(),
		}
	}
}

/// The flags every capture kind shares
#[derive(Args)]
struct Common {
	#[command(flatten)]
	targeting: Targeting,

	/// Exact output dimensions as WIDTHxHEIGHT (each axis at most 4096)
	#[arg(long, value_name = "WxH")]
	size: Option<String>,

	/// Output file (defaults to wsync-<kind>.png in the current directory)
	#[arg(short, long, value_name = "FILE")]
	output: Option<PathBuf>,

	/// Seconds to wait for the render to prepare (default 60)
	#[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: Option<u64>,

	/// Print machine-readable JSON (never pixel data)
	#[arg(long)]
	raw: bool,
}

#[derive(Parser)]
struct CaptureUi {
	/// ScreenGui or GuiObject path to isolate and tight-crop (defaults to
	/// every visible edit-mode ScreenGui)
	#[arg(value_name = "TARGET", conflicts_with = "region")]
	target: Option<String>,

	/// Explicit rectangle in native viewport pixels, as x,y,width,height
	#[arg(long, value_name = "X,Y,W,H")]
	region: Option<String>,

	#[command(flatten)]
	common: Common,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum View {
	Isometric,
	Front,
	Back,
	Left,
	Right,
	Top,
	Bottom,
}

impl View {
	fn as_str(self) -> &'static str {
		match self {
			View::Isometric => "isometric",
			View::Front => "front",
			View::Back => "back",
			View::Left => "left",
			View::Right => "right",
			View::Top => "top",
			View::Bottom => "bottom",
		}
	}
}

#[derive(Parser)]
struct CaptureModel {
	/// Studio path of the model or instance to capture
	#[arg(value_name = "TARGET")]
	target: String,

	/// Camera view preset
	#[arg(long, value_enum, value_name = "PRESET", conflicts_with = "camera_cframe")]
	view: Option<View>,

	/// Camera direction vector as x,y,z (overrides --view)
	#[arg(long, value_name = "X,Y,Z", conflicts_with = "camera_cframe")]
	direction: Option<String>,

	/// Authored camera: exactly the 12 comma-separated values of
	/// CFrame:GetComponents(), replacing view, direction, and padding framing
	#[arg(long = "camera-cframe", value_name = "12-VALUES")]
	camera_cframe: Option<String>,

	/// Framing head-room multiplier, 1.0-4.0
	#[arg(long, value_name = "FACTOR", conflicts_with = "camera_cframe")]
	padding: Option<f64>,

	/// Camera field of view in degrees, 1-120
	#[arg(long, value_name = "DEGREES")]
	fov: Option<f64>,

	/// Frame the original target in place instead of an isolated clone
	#[arg(long)]
	world: bool,

	/// Keep the rendered sky and world background (opaque scene capture)
	#[arg(long)]
	skybox: bool,

	/// Keep the complete camera-framed canvas instead of the subject-alpha
	/// crop
	#[arg(long)]
	framed: bool,

	#[command(flatten)]
	common: Common,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum UiMode {
	None,
	Overlay,
}

impl UiMode {
	fn as_str(self) -> &'static str {
		match self {
			UiMode::None => "none",
			UiMode::Overlay => "overlay",
		}
	}
}

#[derive(Parser)]
struct CaptureViewport {
	/// Explicit rectangle in native viewport pixels, as x,y,width,height
	#[arg(long, value_name = "X,Y,W,H")]
	region: Option<String>,

	/// Keep the rendered sky and world background (opaque capture)
	#[arg(long)]
	skybox: bool,

	/// Include visible ScreenGui layers over the scene
	#[arg(long, value_enum, value_name = "MODE", default_value = "none")]
	ui: UiMode,

	#[command(flatten)]
	common: Common,
}

impl CaptureUi {
	fn main(self) -> Result<()> {
		let mut options = Map::new();

		insert_size(&mut options, self.common.size.as_deref())?;
		insert_region(&mut options, self.region.as_deref())?;

		run("ui", self.target, options, self.common)
	}
}

impl CaptureModel {
	fn main(self) -> Result<()> {
		let mut options = Map::new();

		insert_size(&mut options, self.common.size.as_deref())?;

		if let Some(view) = self.view {
			options.insert("view".to_owned(), json!(view.as_str()));
		}

		if let Some(direction) = &self.direction {
			let axes = parse_floats(direction, 3, "--direction", "x,y,z like `1,-0.65,1`")?;

			if axes.iter().all(|axis| *axis == 0.0) {
				bail!("--direction cannot be the zero vector — it has no direction to look along");
			}

			options.insert(
				"direction".to_owned(),
				json!({ "x": axes[0], "y": axes[1], "z": axes[2] }),
			);
		}

		if let Some(cframe) = &self.camera_cframe {
			let components = parse_floats(
				cframe,
				12,
				"--camera-cframe",
				"the 12 comma-separated values of CFrame:GetComponents()",
			)?;

			options.insert("cameraCFrame".to_owned(), json!(components));
		}

		if let Some(padding) = self.padding {
			if !padding.is_finite() || !(1.0..=4.0).contains(&padding) {
				bail!("--padding must be between 1.0 and 4.0, not {padding}");
			}

			options.insert("padding".to_owned(), json!(padding));
		}

		if let Some(fov) = self.fov {
			if !fov.is_finite() || !(1.0..=120.0).contains(&fov) {
				bail!("--fov must be between 1 and 120 degrees, not {fov}");
			}

			options.insert("fov".to_owned(), json!(fov));
		}

		for (flag, on) in [("world", self.world), ("skybox", self.skybox), ("framed", self.framed)] {
			if on {
				options.insert(flag.to_owned(), json!(true));
			}
		}

		run("model", Some(self.target), options, self.common)
	}
}

impl CaptureViewport {
	fn main(self) -> Result<()> {
		let mut options = Map::new();

		insert_size(&mut options, self.common.size.as_deref())?;
		insert_region(&mut options, self.region.as_deref())?;

		if self.skybox {
			options.insert("skybox".to_owned(), json!(true));
		}

		options.insert("ui".to_owned(), json!(self.ui.as_str()));

		run("viewport", None, options, self.common)
	}
}

// ---------------------------------------------------------------------------
// The shared prepare → pump → encode → verify → close pipeline
// ---------------------------------------------------------------------------

fn run(kind: &'static str, target: Option<String>, options: Map<String, Value>, common: Common) -> Result<()> {
	// The output location is settled before any network work, so a capture
	// never renders into an unwritable destination
	let output = match &common.output {
		Some(output) => output.clone(),
		None => PathBuf::from(format!("wsync-{kind}.png")),
	};

	let client = Client::connect(&common.targeting)?;

	let mut args = json!({ "kind": kind });

	if let Some(target) = &target {
		args["target"] = json!(target);
	}

	if !options.is_empty() {
		args["options"] = Value::Object(options);
	}

	let timeout_ms = common.timeout.map_or(PREPARE_TIMEOUT_MS, |seconds| seconds * 1000);
	let summary = perform_prepared(&client, "capture_prepare", args, &output, timeout_ms, common.raw)?;

	if common.raw {
		let mut record = serde_json::Map::new();

		record.insert("ok".to_owned(), json!(true));

		if let Some(fields) = summary.as_object() {
			record.extend(fields.iter().map(|(key, field)| (key.clone(), field.clone())));
		}

		print_json(&Value::Object(record));

		return Ok(());
	}

	wsync_info!(
		"Captured {} {}x{} → {} ({})",
		kind.bold(),
		summary.get("width").and_then(Value::as_u64).unwrap_or(0),
		summary.get("height").and_then(Value::as_u64).unwrap_or(0),
		summary.get("path").and_then(Value::as_str).unwrap_or_default().bold(),
		human_size(summary.get("bytes").and_then(Value::as_u64).unwrap_or(0))
	);

	Ok(())
}

/// The complete prepare → pump → encode → verify → close pipeline behind one
/// prepare op, shared with `playtest capture` and the workflow `capture` step
/// (their prepare ops answer with the identical `{captureId, …, sha256}`
/// shape). Returns the artifact summary without printing anything, and closes
/// the plugin-side render even when the pump or the encode fails
pub(crate) fn perform_prepared(
	client: &Client,
	prepare_op: &str,
	args: Value,
	output: &std::path::Path,
	timeout_ms: u64,
	raw: bool,
) -> Result<Value> {
	let output = output.resolve()?;

	if let Some(parent) = output.parent() {
		fs::create_dir_all(parent)
			.with_context(|| format!("Failed to create the output directory {}", parent.to_string()))?;
	}

	let prepared = client
		.request_with_timeout(prepare_op, args, timeout_ms)?
		.into_value(raw)?;

	let capture_id = prepared
		.get("captureId")
		.and_then(Value::as_str)
		.with_context(|| format!("The plugin answered `{prepare_op}` without a captureId"))?
		.to_owned();

	// Close even when the pump, digest, or encode fails: the plugin holds the
	// rendered pixels until it is told the transfer is over
	let result = pull_and_write(client, &prepared, &output, raw);

	client.request("capture_close", json!({ "captureId": capture_id })).ok();

	let (png_bytes, png_sha, width, height) = result?;

	Ok(json!({
		"path": output.to_string(),
		"mime": "image/png",
		"bytes": png_bytes,
		"width": width,
		"height": height,
		"sha256": png_sha,
		"capture": prepared,
	}))
}

/// Pumps the prepared pixels and writes the verified PNG, returning the
/// artifact's size, digest, and dimensions. Shared with `transmit`, whose
/// prepared items carry exactly the same `{captureId, width, height, bytes,
/// sha256}` metadata and are served by the same `capture_read` op
pub(crate) fn pull_and_write(
	client: &Client,
	prepared: &Value,
	output: &std::path::Path,
	raw: bool,
) -> Result<(u64, String, u32, u32)> {
	let capture_id = prepared.get("captureId").and_then(Value::as_str).unwrap_or_default();
	let width = prepared.get("width").and_then(Value::as_u64).unwrap_or(0);
	let height = prepared.get("height").and_then(Value::as_u64).unwrap_or(0);
	let bytes = prepared.get("bytes").and_then(Value::as_u64).unwrap_or(0);
	let sha = prepared.get("sha256").and_then(Value::as_str).unwrap_or_default();

	// The prepared dimensions are re-checked against the same limits the CLI
	// enforces on requests, so a buggy plugin cannot talk the CLI into a
	// runaway allocation
	if width == 0 || height == 0 || width > u64::from(MAX_AXIS) || height > u64::from(MAX_AXIS) {
		bail!("The plugin prepared an out-of-range capture ({width}x{height}; each axis must be 1-{MAX_AXIS})");
	}

	if width * height > MAX_PIXELS {
		bail!(
			"The plugin prepared an oversized capture ({width}x{height} = {} pixels; the limit is {MAX_PIXELS})",
			width * height
		);
	}

	if bytes != width * height * 4 {
		bail!(
			"The plugin prepared {bytes} bytes for a {width}x{height} RGBA capture — expected {}",
			width * height * 4
		);
	}

	if sha.is_empty() {
		bail!("The plugin prepared a capture without a SHA-256 digest");
	}

	let pixels = transfer::pull(
		client,
		&transfer::Prepared {
			op: "capture_read",
			id_key: "captureId",
			id: capture_id,
			bytes,
			sha256: sha,
			label: "capture",
		},
		raw,
	)?;

	write_verified_png(output, width as u32, height as u32, &pixels)
		.map(|(png_bytes, png_sha)| (png_bytes, png_sha, width as u32, height as u32))
}

/// Encodes RGBA8 to a temp file next to `output`, decodes the temp file back
/// and compares every pixel, then moves it into place — so the path the user
/// was given never holds a partial or unreadable image
fn write_verified_png(output: &std::path::Path, width: u32, height: u32, pixels: &[u8]) -> Result<(u64, String)> {
	let name = output.get_name();
	let temp = output.with_file_name(format!(".{name}.tmp-{}", process::id()));

	let written = encode_png(&temp, width, height, pixels).and_then(|()| {
		let encoded = fs::read(&temp).with_context(|| format!("Failed to read back {}", temp.to_string()))?;

		verify_png(&encoded, width, height, pixels)?;

		fs::rename(&temp, output)
			.with_context(|| format!("Failed to move the verified PNG into place at {}", output.to_string()))?;

		Ok((encoded.len() as u64, sha256_hex(&encoded)))
	});

	if written.is_err() {
		fs::remove_file(&temp).ok();
	}

	written
}

fn encode_png(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
	let file = fs::File::create(path).with_context(|| format!("Failed to create {}", path.to_string()))?;
	let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);

	encoder.set_color(png::ColorType::Rgba);
	encoder.set_depth(png::BitDepth::Eight);

	let mut writer = encoder.write_header().context("Failed to write the PNG header")?;

	writer
		.write_image_data(pixels)
		.context("Failed to encode the capture as PNG")?;
	writer.finish().context("Failed to finish the PNG stream")?;

	Ok(())
}

/// Decodes an encoded PNG and proves it reproduces exactly the pixels that
/// were rendered
fn verify_png(encoded: &[u8], width: u32, height: u32, pixels: &[u8]) -> Result<()> {
	let decoder = png::Decoder::new(encoded);
	let mut reader = decoder.read_info().context("The written PNG does not decode")?;
	let mut decoded = vec![0u8; reader.output_buffer_size()];
	let info = reader
		.next_frame(&mut decoded)
		.context("The written PNG does not decode")?;

	if info.width != width || info.height != height {
		bail!(
			"The written PNG decodes to {}x{}, not the captured {width}x{height}",
			info.width,
			info.height
		);
	}

	if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
		bail!("The written PNG is not 8-bit RGBA");
	}

	decoded.truncate(info.buffer_size());

	if decoded != pixels {
		bail!("The written PNG does not decode back to the captured pixels");
	}

	Ok(())
}

// ---------------------------------------------------------------------------
// Option parsing — all pre-network
// ---------------------------------------------------------------------------

pub(crate) fn insert_size(options: &mut Map<String, Value>, size: Option<&str>) -> Result<()> {
	let Some(size) = size else { return Ok(()) };

	let (width, height) = size
		.split_once(['x', 'X'])
		.and_then(|(width, height)| Some((width.trim().parse::<u32>().ok()?, height.trim().parse::<u32>().ok()?)))
		.with_context(|| format!("--size must be WIDTHxHEIGHT, e.g. `1920x1080`, not `{size}`"))?;

	check_dimensions(width, height, "--size")?;

	options.insert("size".to_owned(), json!({ "width": width, "height": height }));

	Ok(())
}

fn insert_region(options: &mut Map<String, Value>, region: Option<&str>) -> Result<()> {
	let Some(region) = region else { return Ok(()) };

	let parts: Vec<u32> = region
		.split(',')
		.map(|part| part.trim().parse::<u32>())
		.collect::<Result<_, _>>()
		.ok()
		.filter(|parts: &Vec<u32>| parts.len() == 4)
		.with_context(|| {
			format!("--region must be x,y,width,height in pixels, e.g. `120,80,1280,720`, not `{region}`")
		})?;

	let (x, y, width, height) = (parts[0], parts[1], parts[2], parts[3]);

	check_dimensions(width, height, "--region")?;

	options.insert(
		"region".to_owned(),
		json!({ "x": x, "y": y, "width": width, "height": height }),
	);

	Ok(())
}

/// The axis and total-pixel limits (capture.json), applied to requested sizes
/// and regions before any request is made
fn check_dimensions(width: u32, height: u32, flag: &str) -> Result<()> {
	if width == 0 || height == 0 {
		bail!("{flag} axes must be at least 1 pixel");
	}

	if width > MAX_AXIS || height > MAX_AXIS {
		bail!("{flag} axes are limited to {MAX_AXIS} pixels ({width}x{height} requested)");
	}

	if u64::from(width) * u64::from(height) > MAX_PIXELS {
		bail!(
			"{flag} is limited to {MAX_PIXELS} total pixels ({width}x{height} = {} requested)",
			u64::from(width) * u64::from(height)
		);
	}

	Ok(())
}

/// Exactly `count` comma-separated finite numbers
fn parse_floats(text: &str, count: usize, flag: &str, expected: &str) -> Result<Vec<f64>> {
	let parts: Vec<f64> = text
		.split(',')
		.map(|part| part.trim().parse::<f64>())
		.collect::<Result<_, _>>()
		.with_context(|| format!("{flag} must be {expected}, not `{text}`"))?;

	if parts.len() != count {
		bail!("{flag} must be {expected} — got {} value(s)", parts.len());
	}

	if let Some(bad) = parts.iter().find(|part| !part.is_finite()) {
		bail!("{flag} values must be finite numbers, not {bad}");
	}

	Ok(parts)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sizes_parse_and_enforce_limits() {
		let mut options = Map::new();

		insert_size(&mut options, Some("1920x1080")).unwrap();
		assert_eq!(options["size"], json!({ "width": 1920, "height": 1080 }));

		// 4096x4096 is exactly the total-pixel limit
		insert_size(&mut Map::new(), Some("4096x4096")).unwrap();

		assert!(insert_size(&mut Map::new(), Some("4097x100")).is_err());
		assert!(insert_size(&mut Map::new(), Some("100x4097")).is_err());
		assert!(insert_size(&mut Map::new(), Some("0x100")).is_err());
		assert!(insert_size(&mut Map::new(), Some("100")).is_err());
		assert!(insert_size(&mut Map::new(), Some("wide x tall")).is_err());
	}

	#[test]
	fn regions_parse_and_enforce_limits() {
		let mut options = Map::new();

		insert_region(&mut options, Some("120,80,1280,720")).unwrap();
		assert_eq!(
			options["region"],
			json!({ "x": 120, "y": 80, "width": 1280, "height": 720 })
		);

		assert!(insert_region(&mut Map::new(), Some("0,0,5000,100")).is_err());
		assert!(insert_region(&mut Map::new(), Some("0,0,100")).is_err());
		assert!(insert_region(&mut Map::new(), Some("0,0,100,0")).is_err());
		assert!(insert_region(&mut Map::new(), Some("-1,0,100,100")).is_err());
	}

	#[test]
	fn float_lists_enforce_arity_and_finiteness() {
		assert_eq!(parse_floats("1,-0.65,1", 3, "--direction", "x,y,z").unwrap().len(), 3);
		assert_eq!(
			parse_floats("0,10,20,1,0,0,0,1,0,0,0,1", 12, "--camera-cframe", "12 values")
				.unwrap()
				.len(),
			12
		);

		assert!(parse_floats("1,2", 3, "--direction", "x,y,z").is_err());
		assert!(parse_floats("1,2,3,4", 3, "--direction", "x,y,z").is_err());
		assert!(parse_floats("1,two,3", 3, "--direction", "x,y,z").is_err());
		assert!(parse_floats("1,NaN,3", 3, "--direction", "x,y,z").is_err());
		assert!(parse_floats("1,inf,3", 3, "--direction", "x,y,z").is_err());
	}
}
