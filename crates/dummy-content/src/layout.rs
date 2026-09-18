//! Synthetic Skyrim `Data/` directory layouts.

use crate::{Entry, ba2, bsa, dds, pex, rng::Rng};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure, eyre},
};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Seed used when a caller does not provide one.
pub const DEFAULT_SEED: u64 = 0x5EED_5EED;

/// File families emitted by [`generate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formats {
    /// Loose `textures/*.dds` files.
    pub dds: bool,
    /// Loose `scripts/*.pex` files.
    pub pex: bool,
    /// A `Skyrim - Misc.bsa` archive containing the scripts.
    pub bsa: bool,
    /// A `Skyrim - Textures.ba2` archive containing the textures.
    pub ba2: bool,
}

impl Default for Formats {
    fn default() -> Self {
        Self::all()
    }
}

impl Formats {
    /// Enables every format.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            dds: true,
            pex: true,
            bsa: true,
            ba2: true,
        }
    }

    /// Parses a comma-separated list such as `dds,pex,bsa,ba2`.
    pub fn parse(value: &str) -> Result<Self> {
        let mut formats = Self {
            dds: false,
            pex: false,
            bsa: false,
            ba2: false,
        };
        for name in value.split(',') {
            match name.trim() {
                "dds" => formats.dds = true,
                "pex" => formats.pex = true,
                "bsa" => formats.bsa = true,
                "ba2" => formats.ba2 = true,
                "" => bail!("empty format name in {value:?}"),
                other => bail!("unknown format {other:?}; expected dds, pex, bsa or ba2"),
            }
        }
        ensure!(
            formats.dds || formats.pex || formats.bsa || formats.ba2,
            "no output formats selected"
        );
        Ok(formats)
    }
}

/// Ensures `root` exists and is empty unless `force` is set.
///
/// With `force`, existing generated files are replaced but unrelated files in
/// the directory are left untouched.
pub fn prepare_directory(root: &Path, force: bool) -> Result<()> {
    if root.exists() {
        ensure!(
            root.is_dir(),
            "output path is not a directory: {}",
            root.display()
        );
        let mut entries =
            fs::read_dir(root).wrap_err_with(|| format!("failed to read {}", root.display()))?;
        if entries.next().is_some() && !force {
            bail!(
                "output directory {} is not empty; pass --force to overwrite generated files",
                root.display()
            );
        }
    } else {
        fs::create_dir_all(root)
            .wrap_err_with(|| format!("failed to create {}", root.display()))?;
    }
    Ok(())
}

/// Generates a synthetic `Data` tree and returns every written path.
///
/// The tree contains loose scripts and textures plus `Skyrim - Misc.bsa` and
/// `Skyrim - Textures.ba2` archives, filtered by `formats`. Output bytes are
/// fully determined by `seed`.
pub fn generate(root: &Path, seed: u64, formats: Formats) -> Result<Vec<PathBuf>> {
    let mut rng = Rng::new(seed);
    let scripts = [
        ("scripts/generated.pex", pex::minimal("Generated")?),
        ("scripts/second.pex", pex::minimal("Second")?),
    ];
    let mut textures = Vec::new();
    if formats.dds || formats.ba2 {
        textures.push((
            "textures/generated_color.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc1Unorm, 64, 64).with_mip_levels(7),
                &mut rng,
            )?,
        ));
        textures.push((
            "textures/generated_normal.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc5Unorm, 64, 64).with_mip_levels(7),
                &mut rng,
            )?,
        ));
        textures.push((
            "textures/generated_color_x8.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::X8R8G8B8, 32, 32).with_mip_levels(6),
                &mut rng,
            )?,
        ));
        textures.push((
            "textures/generated_cube.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc1Unorm, 32, 32)
                    .with_mip_levels(6)
                    .as_cubemap(),
                &mut rng,
            )?,
        ));
        textures.push((
            "textures/generated_volume.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc1Unorm, 16, 16)
                    .with_depth(16)
                    .with_mip_levels(5),
                &mut rng,
            )?,
        ));
    }

    let mut written = Vec::new();
    if formats.pex {
        for (name, bytes) in &scripts {
            written.push(write_file(root, name, bytes)?);
        }
    }
    if formats.dds {
        for (name, bytes) in &textures {
            written.push(write_file(root, name, bytes)?);
        }
    }
    if formats.bsa {
        let entries: Vec<Entry<'_>> = scripts
            .iter()
            .map(|(name, bytes)| Entry::new(name, bytes))
            .collect();
        let archive = bsa::v105(&entries, bsa::Compression::Zlib)?;
        written.push(write_file(root, "Skyrim - Misc.bsa", &archive)?);
    }
    if formats.ba2 {
        let entries: Vec<Entry<'_>> = textures
            .iter()
            .map(|(name, bytes)| Entry::new(name, bytes))
            .collect();
        let archive = ba2::general(&entries, ba2::Compression::Zlib)?;
        written.push(write_file(root, "Skyrim - Textures.ba2", &archive)?);
    }
    Ok(written)
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) -> Result<PathBuf> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| eyre!("invalid fixture path {}", path.display()))?
        .to_string_lossy()
        .into_owned();
    let temporary = path.with_file_name(format!("{file_name}.{}.partial", std::process::id()));
    let backup = path.with_file_name(format!("{file_name}.{}.backup", std::process::id()));
    ensure!(
        !temporary.exists() && !backup.exists(),
        "stale fixture temporary exists for {}",
        path.display()
    );

    let mut file = fs::File::create(&temporary)
        .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if path.exists() {
        fs::rename(&path, &backup)
            .wrap_err_with(|| format!("failed to preserve {}", path.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if backup.exists() {
            let _ = fs::rename(&backup, &path);
        }
        return Err(error).wrap_err_with(|| format!("failed to publish {}", path.display()));
    }
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_FILES: [&str; 9] = [
        "scripts/generated.pex",
        "scripts/second.pex",
        "textures/generated_color.dds",
        "textures/generated_normal.dds",
        "textures/generated_color_x8.dds",
        "textures/generated_cube.dds",
        "textures/generated_volume.dds",
        "Skyrim - Misc.bsa",
        "Skyrim - Textures.ba2",
    ];

    #[test]
    fn parses_format_lists() {
        assert_eq!(
            Formats::parse("dds").unwrap(),
            Formats {
                dds: true,
                pex: false,
                bsa: false,
                ba2: false,
            }
        );
        assert_eq!(Formats::parse("dds, pex,bsa ,ba2").unwrap(), Formats::all());
        for value in ["", "dds,", "nif", "dds,nif"] {
            assert!(Formats::parse(value).is_err(), "{value:?} was accepted");
        }
    }

    #[test]
    fn refuses_non_empty_directories_without_force() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        prepare_directory(&root, false).unwrap();
        assert!(prepare_directory(&root, false).is_ok());
        fs::write(root.join("keep.txt"), b"keep").unwrap();
        assert!(prepare_directory(&root, false).is_err());
        prepare_directory(&root, true).unwrap();
        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn generates_the_expected_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        prepare_directory(&root, false).unwrap();
        let written = generate(&root, DEFAULT_SEED, Formats::all()).unwrap();
        assert_eq!(written.len(), EXPECTED_FILES.len());
        for relative in EXPECTED_FILES {
            assert!(root.join(relative).is_file(), "missing {relative}");
        }
    }

    #[test]
    fn generation_is_deterministic_per_seed() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_root = first.path().join("Data");
        let second_root = second.path().join("Data");
        generate(&first_root, DEFAULT_SEED, Formats::all()).unwrap();
        generate(&second_root, DEFAULT_SEED, Formats::all()).unwrap();
        for relative in EXPECTED_FILES {
            assert_eq!(
                fs::read(first_root.join(relative)).unwrap(),
                fs::read(second_root.join(relative)).unwrap(),
                "{relative} differs for the same seed"
            );
        }

        let other = tempfile::tempdir().unwrap();
        let other_root = other.path().join("Data");
        generate(&other_root, DEFAULT_SEED + 1, Formats::all()).unwrap();
        assert_ne!(
            fs::read(first_root.join("textures/generated_color.dds")).unwrap(),
            fs::read(other_root.join("textures/generated_color.dds")).unwrap(),
            "a different seed produced identical textures"
        );
        assert_eq!(
            fs::read(first_root.join("scripts/generated.pex")).unwrap(),
            fs::read(other_root.join("scripts/generated.pex")).unwrap(),
            "script bytes must not depend on the seed"
        );
    }

    #[test]
    fn formats_filter_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        let formats = Formats {
            dds: false,
            pex: true,
            bsa: true,
            ba2: false,
        };
        let written = generate(&root, DEFAULT_SEED, formats).unwrap();
        assert_eq!(written.len(), 3);
        assert!(root.join("scripts/generated.pex").is_file());
        assert!(root.join("Skyrim - Misc.bsa").is_file());
        assert!(!root.join("textures").exists());
        assert!(!root.join("Skyrim - Textures.ba2").exists());
    }
}
