//! Command-line entry point for the deterministic fixture generator.
#![forbid(unsafe_code)]

use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure, eyre},
};
use dummy_content::{
    esm,
    layout::{self, DEFAULT_SEED, Formats},
};
use std::{
    path::{Path, PathBuf},
    process::exit,
};

/// Author string and worldspace the interior preset's plugin carries. The
/// crate's other preset (`layout::generate`) writes the same pair.
const PRESET_AUTHOR: &str = "dummy-content";
const PRESET_WORLDSPACE: &str = "GeneratedWorld";

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
    let mut formats = options.formats;
    if options.with_interior {
        // `write_interior_plugin` writes the plugin itself, so the default one
        // is not generated: `--with-interior` replaces `Skyrim.esm`.
        formats.esm = false;
    }
    layout::prepare_directory(&options.output, options.force)?;
    let mut written = layout::generate(&options.output, options.seed, formats)?;
    if options.with_interior {
        written.push(write_interior_plugin(&options.output)?);
    }
    println!(
        "Generated {} fixture files in {}",
        written.len(),
        options.output.display()
    );
    Ok(())
}

/// Writes `Skyrim.esm` as the interior preset: one exterior cell, its
/// auto-load door into one interior cell, and the return door.
fn write_interior_plugin(output: &Path) -> Result<PathBuf> {
    let cells = [esm::PRESET_EXTERIOR_CELL];
    let bytes = esm::plugin_with_interior(
        &esm::Plugin {
            author: PRESET_AUTHOR,
            worldspace: PRESET_WORLDSPACE,
            cells: &cells,
            model_path: "meshes/generated.nif",
            diffuse: "textures/generated_color.dds",
            normal_texture: "textures/generated_normal.dds",
        },
        &esm::PRESET_INTERIOR,
    )?;
    let path = output.join("Skyrim.esm");
    std::fs::write(&path, bytes).wrap_err_with(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

#[derive(Debug)]
struct GenOptions {
    output: PathBuf,
    seed: u64,
    formats: Formats,
    force: bool,
    with_interior: bool,
}

fn parse_gen(arguments: &[String]) -> Result<GenOptions> {
    let mut output = None;
    let mut seed = DEFAULT_SEED;
    let mut formats = Formats::default();
    let mut force = false;
    let mut with_interior = false;
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
            "--with-interior" => with_interior = true,
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
    if with_interior {
        ensure!(
            formats.esm,
            "--with-interior writes a plugin; add esm to --formats\n\n{}",
            usage()
        );
    }
    Ok(GenOptions {
        output,
        seed,
        formats,
        force,
        with_interior,
    })
}

fn usage() -> &'static str {
    "dummy-content: deterministic Skyrim-format fixtures

USAGE:
    dummy-content gen <output-dir> [--seed <n>] [--formats <list>] [--force]
                      [--with-interior]

COMMANDS:
    gen    Generate a synthetic Data directory

OPTIONS:
    --seed <n>        Seed for generated texture content
    --formats <list>  Comma-separated subset of: dds, pex, nif, bsa, ba2, esm
    --force           Overwrite generated files in a non-empty directory
    --with-interior   Write Skyrim.esm with one exterior cell, one interior
                      cell and a reciprocal pair of load doors (needs esm)"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_the_interior_preset_flag() {
        let options = parse_gen(&arguments(&["out", "--with-interior"])).unwrap();
        assert!(options.with_interior);
        assert_eq!(options.output, PathBuf::from("out"));
        assert!(
            options.formats.esm,
            "the default formats include the plugin"
        );
        assert!(!parse_gen(&arguments(&["out"])).unwrap().with_interior);
        assert!(
            parse_gen(&arguments(&["out", "--with-interior", "--formats", "dds"])).is_err(),
            "the preset needs the esm format"
        );
        assert!(
            parse_gen(&arguments(&[
                "out",
                "--formats",
                "dds,esm",
                "--with-interior"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn writes_the_interior_preset_plugin() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Data");
        run_gen(&arguments(&[
            output.to_str().unwrap(),
            "--formats",
            "esm",
            "--with-interior",
        ]))
        .unwrap();

        let plugin = std::fs::read(output.join("Skyrim.esm")).unwrap();
        assert_eq!(&plugin[..4], b"TES4");
        assert!(plugin.windows(4).any(|window| window == b"DOOR"));
        assert!(plugin.windows(4).any(|window| window == b"XTEL"));
    }
}
