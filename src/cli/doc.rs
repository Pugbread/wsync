use anyhow::Result;
use clap::Parser;
use colored::Colorize;

use crate::wsync_info;

// TODO(phase-7): point at the real WSync docs site once it exists
const LINK: &str = "https://github.com/Pugbread/wsync";

/// Open WSync's documentation in the browser
#[derive(Parser)]
pub struct Doc {}

impl Doc {
	pub fn main(self) -> Result<()> {
		wsync_info!("Launched browser. Manually go to: {}", LINK.bold());

		open::that(LINK)?;

		Ok(())
	}
}
