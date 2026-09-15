//! Parse actual documented examples without executing commands or opening sockets.
use clap::Parser;
use serde_json::Value;
use wsync::cli::Cli;

#[test]
fn all_documented_examples_parse() {
	let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/commands");
	let mut failures = Vec::new();
	let mut checked = 0;
	for file in std::fs::read_dir(directory).unwrap() {
		let path = file.unwrap().path();
		if path.extension().and_then(|e| e.to_str()) != Some("json") {
			continue;
		}
		let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
		let Some(examples) = doc["examples"].as_array() else {
			continue;
		};
		for example in examples {
			let example = example.as_str().unwrap();
			let words = shell_words::split(example).expect("Example must have balanced shell quotes");
			// Validate each WSync segment in a documented pipeline too. No shell
			// or input-producing command is ever executed by this parser test.
			let start = words
				.iter()
				.position(|word| word == "wsync")
				.expect("Example must call wsync");
			let segment = words[start..]
				.iter()
				.take_while(|word| !matches!(word.as_str(), "|" | "&&" | ";"));
			if let Err(error) = Cli::try_parse_from(segment) {
				if !matches!(
					error.kind(),
					clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
				) {
					failures.push(format!("{}: {example}\n{error}", path.display()));
				}
			}
			checked += 1;
		}
	}
	assert!(checked >= 150, "Only {checked} examples tested");
	assert!(
		failures.is_empty(),
		"{} examples do not parse:\n{}",
		failures.len(),
		failures.join("\n")
	);
}
