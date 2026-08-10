//! Reflection: `classinfo`, `enums`, `enum` (classinfo.json, enums.json,
//! enum.json). `enum` maps to the plugin's `enum_list` op.

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};

use crate::cli::client::{field, print_json, Client, Targeting};

/// List properties and methods for a Roblox class
#[derive(Parser)]
pub struct ClassInfo {
	#[command(flatten)]
	targeting: Targeting,

	/// Class name (e.g. `BasePart`)
	#[arg(long, value_name = "CLASS")]
	class: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl ClassInfo {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("class_info", json!({ "class": self.class }), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		println!(
			"{} : {} (metadata source: {})",
			field(&value, "class").bold(),
			field(&value, "superclass"),
			field(&value, "source"),
		);

		let empty = Vec::new();
		let properties = value.get("properties").and_then(Value::as_array).unwrap_or(&empty);
		let mut category = String::new();

		println!("\nProperties ({})", properties.len());

		for record in properties {
			let record_category = field(record, "category");

			if record_category != category {
				record_category.clone_into(&mut category);
				println!("  {}", category.bold());
			}

			println!("    {:<32} {}", field(record, "name"), field(record, "type"));
		}

		for (label, key) in [("Methods", "methods"), ("Events", "events")] {
			let Some(items) = value.get(key).and_then(Value::as_array) else {
				continue;
			};

			println!("\n{label} ({})", items.len());

			for item in items {
				match item {
					Value::String(name) => println!("    {name}"),
					other => println!("    {}", field(other, "name")),
				}
			}
		}

		Ok(())
	}
}

/// List every Enum type name exposed by the connected Studio session
#[derive(Parser)]
pub struct Enums {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Enums {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("enums", json!({}), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let names = value.get("enums").and_then(Value::as_array).unwrap_or(&empty);

		for name in names {
			println!("{}", name.as_str().unwrap_or_default());
		}

		println!("\n{} enum type(s)", names.len());

		Ok(())
	}
}

/// List the items of one Roblox Enum type
#[derive(Parser)]
pub struct Enum {
	#[command(flatten)]
	targeting: Targeting,

	/// Enum type name (e.g. `Material`)
	#[arg(long, value_name = "ENUM")]
	name: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Enum {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("enum_list", json!({ "name": self.name }), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let items = value.get("items").and_then(Value::as_array).unwrap_or(&empty);

		println!("Enum.{}", field(&value, "enum").bold());

		for item in items {
			println!(
				"  {:<32} {}",
				field(item, "name"),
				item.get("value").and_then(Value::as_i64).unwrap_or(0)
			);
		}

		println!("\n{} item(s)", items.len());

		Ok(())
	}
}
