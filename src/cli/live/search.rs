//! Search: `query`, `find`, `find-attr` (query.json, find.json,
//! find-attr.json).
//!
//! `query` has no `--raw`: `--format json` *is* its machine-readable form
//! (query.json pins that shape), while `paths`/`classes` are line formats
//! that report truncation on stderr instead of in the payload.
//!
//! `find`/`find_by_attr` answer `{matches,count,truncated,truncationReason,…}`
//! — the match list is always read out of `.matches`, never assumed to be a
//! bare array.

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use serde_json::{json, Value};

use crate::{
	cli::client::{clip, field, parse_literal, print_json, truncation_advice, Client, Targeting},
	wsync_warn,
};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
	Json,
	Paths,
	Classes,
}

impl Format {
	fn as_str(self) -> &'static str {
		match self {
			Format::Json => "json",
			Format::Paths => "paths",
			Format::Classes => "classes",
		}
	}
}

/// Match a selector inside Studio and optionally project properties,
/// attributes, and tags
#[derive(Parser)]
pub struct Query {
	#[command(flatten)]
	targeting: Targeting,

	/// Selector; `*` matches one path segment, `**` matches zero or more
	#[arg(value_name = "SELECTOR")]
	selector: String,

	/// Project this property onto every match (repeatable)
	#[arg(long = "prop", value_name = "NAME")]
	props: Vec<String>,

	/// Include each match's attributes
	#[arg(long)]
	attributes: bool,

	/// Include each match's tags
	#[arg(long)]
	tags: bool,

	/// Maximum matches to return (1-10000, validated before the request)
	#[arg(long, value_name = "N", default_value = "5000", value_parser = clap::value_parser!(u32).range(1..=10000))]
	limit: u32,

	/// Output format
	#[arg(long, value_enum, default_value = "json")]
	format: Format,
}

impl Query {
	pub fn main(self) -> Result<()> {
		// `--limit` is range-checked by the parser, so an out-of-range value
		// never reaches the network (query.json: CLI-side limit validation)
		let client = Client::connect(&self.targeting)?;

		let args = json!({
			"selector": self.selector,
			"props": self.props,
			"attributes": self.attributes,
			"tags": self.tags,
			"limit": self.limit,
			"format": self.format.as_str(),
		});

		// The JSON format is this command's machine-readable output, so a
		// failed op prints its envelope for the same reason `--raw` does
		let value = client.value("query", args, self.format == Format::Json)?;

		if self.format == Format::Json {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let matches = value.get("matches").and_then(Value::as_array).unwrap_or(&empty);

		for entry in matches {
			match self.format {
				Format::Paths => println!("{}", entry.as_str().unwrap_or_default()),
				Format::Classes => println!("{:<28} {}", field(entry, "class"), field(entry, "path")),
				Format::Json => unreachable!("handled above"),
			}
		}

		if value.get("truncated").and_then(Value::as_bool) == Some(true) {
			wsync_warn!(
				"{}",
				truncation_advice(value.get("truncationReason").and_then(Value::as_str))
			);
		}

		Ok(())
	}
}

/// Find live Studio instances by class name and/or name substring
#[derive(Parser)]
pub struct Find {
	#[command(flatten)]
	targeting: Targeting,

	/// Class name; matches subclasses too (`IsA`)
	#[arg(long, value_name = "CLASS")]
	class: Option<String>,

	/// Case-sensitive substring of the instance name
	#[arg(long, value_name = "TEXT")]
	name: Option<String>,

	/// Scope the search to this subtree (default: the whole DataModel)
	#[arg(long, value_name = "STUDIO-PATH")]
	under: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Find {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let mut args = json!({ "under": self.under.clone().unwrap_or_default() });

		if let Some(class) = &self.class {
			args["class"] = json!(class);
		}

		if let Some(name) = &self.name {
			args["name"] = json!(name);
		}

		let value = client.value("find", args, self.raw)?;

		report_matches(&value, self.raw, self.under.as_deref())
	}
}

/// Find live Studio instances carrying a named attribute
#[derive(Parser)]
pub struct FindAttr {
	#[command(flatten)]
	targeting: Targeting,

	/// Attribute name
	#[arg(long, value_name = "ATTRIBUTE")]
	name: String,

	/// Attribute value as a JSON literal or tagged value; bare text that is
	/// not valid JSON is taken as a string
	#[arg(long, value_name = "JSON")]
	value: Option<String>,

	/// Scope the search to this subtree (default: the whole DataModel)
	#[arg(long, value_name = "STUDIO-PATH")]
	under: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl FindAttr {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let mut args = json!({
			"name": self.name,
			"under": self.under.clone().unwrap_or_default(),
		});

		if let Some(literal) = &self.value {
			args["value"] = parse_literal(literal);
		}

		let value = client.value("find_by_attr", args, self.raw)?;

		report_matches(&value, self.raw, self.under.as_deref())
	}
}

fn report_matches(value: &Value, raw: bool, under: Option<&str>) -> Result<()> {
	if raw {
		print_json(value);

		return Ok(());
	}

	let Some(matches) = value.get("matches").and_then(Value::as_array) else {
		bail!("The plugin answered without a `matches` array");
	};

	for entry in matches {
		println!(
			"{} {}",
			format!("{:<28}", clip(field(entry, "class"), 28)).bold(),
			field(entry, "path")
		);
	}

	println!(
		"\n{} match(es) under {}",
		value
			.get("count")
			.and_then(Value::as_u64)
			.unwrap_or(matches.len() as u64),
		under.filter(|under| !under.is_empty()).unwrap_or("the DataModel")
	);

	if value.get("truncated").and_then(Value::as_bool) == Some(true) {
		wsync_warn!(
			"{}",
			truncation_advice(value.get("truncationReason").and_then(Value::as_str))
		);
	}

	Ok(())
}
