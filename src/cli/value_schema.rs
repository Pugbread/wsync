//! Offline live-value contract, shared with the generated plugin codec schema.
//! This is the remote JSON wire format, not the filesystem sidecar serializer.

use std::sync::OnceLock;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

const CATALOG: &str = include_str!("../../docs/value-types.json");

fn parsed() -> &'static Value {
	static VALUE: OnceLock<Value> = OnceLock::new();
	VALUE.get_or_init(|| serde_json::from_str(CATALOG).expect("embedded value catalog must be valid JSON"))
}

pub fn catalog() -> Value {
	parsed().clone()
}

/// Compact real `--help` examples, drawn from the same source as offline
/// schemas and runtime validation. Agents must not have to guess after help.
pub fn help() -> String {
	format!(
		"JSON VALUES:\n  Vector3: {}\n  Color3:  {}\n  Use the exact __type key and lowercase fields. Color3 channels use the 0–1 scale.\n  Full offline schemas: wsync commands types [TypeName]\n  Content uses native Content.fromUri(...) in eval/exec; it has no JSON input tag.",
		lookup("Vector3").expect("Vector3 catalog entry")["example"],
		lookup("Color3").expect("Color3 catalog entry")["example"],
	)
}

pub fn lookup(name: &str) -> Option<Value> {
	parsed()["types"]
		.as_array()?
		.iter()
		.chain(parsed()["nativeOnly"].as_array()?.iter())
		.find(|entry| entry["name"].as_str().is_some_and(|tag| tag.eq_ignore_ascii_case(name)))
		.cloned()
}

/// Validate every tagged object before connecting to Studio. Plain JSON remains
/// allowed: only Studio knows which datatype a particular property requires.
pub fn validate(value: &Value) -> Result<(), String> {
	let mut nodes = 0;
	let mut bytes = 0;
	visit(value, "$", 1, &mut nodes, &mut bytes)
}

/// Keep raw CLI failures machine-readable even when validation prevents the
/// network request. A local rejection guarantees that execution never started.
pub fn validate_cli(value: &Value, raw: bool) -> anyhow::Result<()> {
	if let Err(message) = validate(value) {
		if raw {
			println!(
				"{}",
				serde_json::json!({
					"ok": false,
					"error": { "code": "INVALID_ARGUMENT", "message": message },
					"meta": { "completion": "notStarted", "retrySafe": true }
				})
			);
		}
		anyhow::bail!(message);
	}
	Ok(())
}

fn visit(value: &Value, path: &str, depth: usize, nodes: &mut usize, bytes: &mut usize) -> Result<(), String> {
	*nodes += 1;
	if depth > 64 || *nodes > 20000 {
		return Err(format!("{path}: value exceeds codec limits (depth 64, 20000 nodes)"));
	}
	if let Some(text) = value.as_str() {
		*bytes += text.len();
		if *bytes > 4194304 {
			return Err(format!("{path}: value exceeds the 4194304-byte codec string budget"));
		}
	}
	match value {
		Value::Array(items) => {
			if items.len() > 10000 {
				return Err(format!("{path}: array exceeds the 10000-entry codec limit"));
			}
			for (i, item) in items.iter().enumerate() {
				visit(item, &format!("{path}[{i}]"), depth + 1, nodes, bytes)?;
			}
		}
		Value::Object(fields) => {
			if fields.len() > 10000 {
				return Err(format!("{path}: object exceeds the 10000-entry codec limit"));
			}
			for (key, item) in fields {
				visit(item, &format!("{path}.{key}"), depth + 1, nodes, bytes)?;
			}
			if let Some(raw_tag) = fields.get("__type") {
				let tag = raw_tag
					.as_str()
					.ok_or_else(|| format!("{path}.__type must be a string; run `wsync commands types`"))?;
				let entry = parsed()["types"].as_array().unwrap().iter().find(|entry| entry["name"] == tag)
					.ok_or_else(|| format!("{path}: unknown __type '{tag}'; run `wsync commands types` (Content uses native Content.fromUri in eval/exec)"))?;
				if entry["decode"] != true {
					return Err(format!(
						"{path}: {tag} is output-only and cannot be sent as input; run `wsync commands types {tag}`"
					));
				}
				validate_schema(value, &entry["schema"], path)
					.and_then(|_| validate_relations(value, tag, path))
					.map_err(|message| {
						format!(
							"{message}. Expected {tag} shape, e.g. {}; run `wsync commands types {tag}`",
							entry["example"]
						)
					})?;
			}
		}
		_ => {}
	}
	Ok(())
}

fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
	if let Some(expected) = schema.get("const") {
		if value != expected {
			return Err(format!("{path} must be {expected}"));
		}
	}
	if let Some(choices) = schema["enum"].as_array() {
		if !choices.contains(value) {
			return Err(format!("{path} must be one of {}", schema["enum"]));
		}
	}
	let kind = schema["type"].as_str().unwrap_or("");
	let matches = match kind {
		"number" => value.is_number(),
		"integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
		"string" => value.is_string(),
		"boolean" => value.is_boolean(),
		"array" => value.is_array(),
		"object" => value.is_object(),
		_ => true,
	};
	if !matches {
		return Err(format!("{path} must be {kind}"));
	}
	if let Some(number) = value.as_f64() {
		if !number.is_finite() {
			return Err(format!("{path} must be finite"));
		}
		if schema["minimum"].as_f64().is_some_and(|min| number < min)
			|| schema["maximum"].as_f64().is_some_and(|max| number > max)
		{
			return Err(format!(
				"{path} is out of range (minimum {}, maximum {})",
				schema["minimum"], schema["maximum"]
			));
		}
	}
	if schema["format"] == "base64" {
		STANDARD
			.decode(value.as_str().unwrap())
			.map_err(|_| format!("{path} must be canonical padded base64"))?;
	}
	if let Some(items) = value.as_array() {
		let count = items.len() as u64;
		if schema["minItems"].as_u64().is_some_and(|min| count < min)
			|| schema["maxItems"].as_u64().is_some_and(|max| count > max)
		{
			return Err(format!(
				"{path} must contain {}–{} entries",
				schema["minItems"], schema["maxItems"]
			));
		}
		if let Some(lengths) = schema["allowedLengths"].as_array() {
			if !lengths.iter().any(|length| length.as_u64() == Some(count)) {
				return Err(format!(
					"{path} must have one of these lengths: {}",
					schema["allowedLengths"]
				));
			}
		}
		for (index, item) in items.iter().enumerate() {
			validate_schema(item, &schema["items"], &format!("{path}[{index}]"))?;
		}
	}
	if let Some(fields) = value.as_object() {
		if let Some(required) = schema["required"].as_array() {
			for field in required.iter().filter_map(Value::as_str) {
				if !fields.contains_key(field) {
					return Err(format!("{path}.{field} is required"));
				}
			}
		}
		for (field, item) in fields {
			match schema["properties"].get(field) {
				Some(child) => validate_schema(item, child, &format!("{path}.{field}"))?,
				None if schema["additionalProperties"] == false => {
					return Err(format!("{path}.{field} is not a supported field"))
				}
				None => {}
			}
		}
	}
	Ok(())
}

fn validate_relations(value: &Value, tag: &str, path: &str) -> Result<(), String> {
	match tag {
		"NumberRange" if value["min"].as_f64() > value["max"].as_f64() => {
			return Err(format!("{path}.min must be <= max"))
		}
		"NumberSequence" | "ColorSequence" => {
			let points = value["keypoints"].as_array().unwrap();
			if points[0]["time"].as_f64() != Some(0.0) || points.last().unwrap()["time"].as_f64() != Some(1.0) {
				return Err(format!("{path}.keypoints must start at time 0 and end at time 1"));
			}
			if points
				.windows(2)
				.any(|pair| pair[0]["time"].as_f64() > pair[1]["time"].as_f64())
			{
				return Err(format!("{path}.keypoints must be in nondecreasing time order"));
			}
		}
		"BinaryString" | "Buffer" if value.get("bytes").is_some() => {
			let len = STANDARD.decode(value["base64"].as_str().unwrap()).unwrap().len() as f64;
			if value["bytes"].as_f64() != Some(len) {
				return Err(format!("{path}.bytes must equal decoded byte length {len}"));
			}
		}
		_ => {}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn every_writable_catalog_example_validates() {
		for entry in parsed()["types"].as_array().unwrap() {
			if entry["decode"] == true {
				validate(&entry["example"]).unwrap_or_else(|error| panic!("{}: {error}", entry["name"]));
			} else {
				assert!(validate(&entry["example"]).is_err());
			}
		}
		assert_eq!(lookup("vector3").unwrap()["name"], "Vector3");
		assert_eq!(lookup("Content").unwrap()["decode"], false);
	}

	#[test]
	fn invalid_shapes_explain_the_required_contract() {
		let value = json!({"props":{"Size":{"__type":"Vector3","x":1,"y":2}}});
		let error = validate(&value).unwrap_err();
		assert!(error.contains("$.props.Size.z is required"));
		assert!(error.contains(r#""__type":"Vector3""#));
		assert!(error.contains("wsync commands types Vector3"));
		for bad in [
			json!({"__type":"Color3","r":1,"g":0,"b":0,"R":255}),
			json!({"__type":"Vector3int16","x":"32768","y":0,"z":0}),
			json!({"__type":"Axes","x":"true"}),
			json!({"__type":"NumberRange","min":3,"max":1}),
			json!({"__type":"CFrame","components":[1,2,3,4]}),
			json!({"__type":"InstancePath","path":"Workspace","rooted":false}),
			json!({"__type":"NumberSequence","keypoints":[{"time":0,"value":1},{"time":0.5,"value":2}]}),
			json!({"__type":"Buffer","base64":"!!!!"}),
			json!({"__type":"BinaryString","base64":"aGk=","bytes":3}),
			json!({"__type":"BinaryString","base64":"Zh=="}),
		] {
			assert!(validate(&bad).is_err(), "accepted malformed value: {bad}");
		}
	}

	#[test]
	fn constructor_normalization_remains_available() {
		for valid in [
			json!({"__type":"Vector3int16","x":32768,"y":0.5,"z":0}),
			json!({"__type":"UDim","scale":0.5,"offset":0.75}),
			json!({"__type":"Color3","r":5,"g":-1,"b":0}),
			json!({"__type":"PhysicalProperties","density":200,"friction":-1,"elasticity":2}),
		] {
			validate(&valid).unwrap();
		}
	}

	#[test]
	fn nested_tag_strings_cannot_bypass_budgets() {
		assert!(
			validate(&json!({"__type":"Font","family":"a".repeat(4194305),"weight":"Regular","style":"Normal"}))
				.unwrap_err()
				.contains("string budget")
		);
		assert!(validate(&json!({"nested":[true,3,"hello",null,{"a":1}]})).is_ok());
	}
}
