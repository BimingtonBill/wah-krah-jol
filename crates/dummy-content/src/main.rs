//! Command-line entry point for the deterministic fixture generator.
#![forbid(unsafe_code)]

use color_eyre::{
    Result,
    eyre::{bail, ensure, eyre},
};
use dummy_content::layout::{self, DEFAULT_SEED, Formats};
use std::{path::PathBuf, process::exit};

fn main() -> Result<()> {
    color_eyre::install()?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("-h" | "--help") | None => {
            println!("{}", usage());
            Ok(())
        }
        Some("gen") => run_gen(&arguments[1..]),
        Some(command) => bail!("unknown command `{command}`\n\n{}", usage()),
    }
}

fn run_gen(arguments: &[String]) -> Result<()> {
    let options = parse_gen(arguments)?;
    layout::prepare_directory(&options.output, options.force)?;
    let written = layout::generate(&options.output, options.seed, options.formats)?;
    println!(
        "Generated {} fixture files in {}",
        written.len(),
        options.output.display()
    );
    Ok(())
}

#[derive(Debug)]
struct GenOptions {
    output: PathBuf,
    seed: u64,
    formats: Formats,
    force: bool,
}

fn parse_gen(arguments: &[String]) -> Result<GenOptions> {
    let mut output = None;
    let mut seed = DEFAULT_SEED;
    let mut formats = Formats::default();
    let mut force = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--seed" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| eyre!("--seed requires a value"))?;
                seed = value
                    .parse()
                    .map_err(|_| eyre!("invalid --seed value {value:?}"))?;
            }
            "--formats" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| eyre!("--formats requires a value"))?;
                formats = Formats::parse(value)?;
            }
            "--force" => force = true,
            "-h" | "--help" => {
                println!("{}", usage());
                exit(0);
            }
            argument if argument.starts_with("--") => {
                bail!("unknown option `{argument}`\n\n{}", usage())
            }
            argument => {
                ensure!(output.is_none(), "unexpected extra argument `{argument}`");
                output = Some(PathBuf::from(argument));
            }
        }
        index += 1;
    }
    let output =
        output.ok_or_else(|| eyre!("`gen` requires an output directory\n\n{}", usage()))?;
    Ok(GenOptions {
        output,
        seed,
        formats,
        force,
    })
}

fn usage() -> &'static str {
    "dummy-content: deterministic Skyrim-format fixtures

USAGE:
    dummy-content gen <output-dir> [--seed <n>] [--formats <list>] [--force]

COMMANDS:
    gen    Generate a synthetic Data directory

OPTIONS:
    --seed <n>        Seed for generated texture content
    --formats <list>  Comma-separated subset of: dds, pex, bsa, ba2
    --force           Overwrite generated files in a non-empty directory"
}
