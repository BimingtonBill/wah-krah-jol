//! Command-line entry point for the deterministic fixture generator.
#![forbid(unsafe_code)]

use color_eyre::{Result, eyre::bail};

fn main() -> Result<()> {
    color_eyre::install()?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            println!("{}", usage());
            Ok(())
        }
        Some("gen") => bail!("`gen` is not implemented yet"),
        Some(command) => bail!("unknown command `{command}`\n\n{}", usage()),
    }
}

fn usage() -> &'static str {
    "dummy-content: deterministic Skyrim-format fixtures

USAGE:
    dummy-content gen <output-dir> [--seed <n>] [--formats <list>] [--force]

COMMANDS:
    gen    Generate a synthetic Data directory"
}
