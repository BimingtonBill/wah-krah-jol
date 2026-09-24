use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Read, Write},
    path::Path,
};

pub const CONVERTER_SCHEMA_VERSION: u32 = 14;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub source_hash: String,
    pub output: String,
    pub output_size: u64,
    pub output_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestedFile {
    pub path: String,
    pub size: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestionCacheEntry {
    pub source_hash: String,
    pub files: Vec<IngestedFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversionManifest {
    pub schema_version: u32,
    pub complete: bool,
    #[serde(default)]
    pub configuration_hash: String,
    #[serde(default)]
    pub inputs_by_kind: BTreeMap<String, u64>,
    #[serde(default)]
    pub failures: BTreeMap<String, String>,
    #[serde(default)]
    pub archives: BTreeMap<String, IngestionCacheEntry>,
    pub entries: BTreeMap<String, CacheEntry>,
}

impl ConversionManifest {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self {
                schema_version: CONVERTER_SCHEMA_VERSION,
                ..Self::default()
            });
        }
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let mut manifest: Self =
            serde_json::from_slice(&bytes).wrap_err("invalid conversion manifest")?;
        if matches!(manifest.schema_version, 12 | 13) && CONVERTER_SCHEMA_VERSION == 14 {
            // Schemas 13/14 change only NIF material publication and LAND
            // normalization. Preserve verified archive ingestion, textures,
            // and scripts, but force every GLB plus the always-rebuilt world
            // database and cell cache through the new contracts.
            manifest.complete = false;
            manifest
                .entries
                .retain(|_, entry| !entry.output.to_ascii_lowercase().ends_with(".glb"));
            return Ok(manifest);
        }
        if manifest.schema_version != CONVERTER_SCHEMA_VERSION {
            return Ok(Self {
                schema_version: CONVERTER_SCHEMA_VERSION,
                ..Self::default()
            });
        }
        Ok(manifest)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let temporary = path.with_extension(format!("json.{}.partial", std::process::id()));
        let mut file = fs::File::create(&temporary)
            .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
            .wrap_err_with(|| format!("failed to publish {}", path.display()))
    }
}

pub fn configuration_hash(config: &crate::config::PipelineConfig) -> Result<String> {
    configuration_hash_for_schema(config, CONVERTER_SCHEMA_VERSION)
}

pub fn configuration_hash_for_schema(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    let relevant = serde_json::json!({
        "schema": schema,
        "texture_etc1s_quality": config.texture_etc1s_quality,
        "texture_uastc_level": config.texture_uastc_level,
        "script_abi_version": config.script_abi_version,
    });
    Ok(hash_bytes(&serde_json::to_vec(&relevant)?))
}

/// Puts `from`'s bytes at `to` as a hard link where the filesystem allows one, else as a copy.
///
/// A reconversion reuses every unchanged output of the previous run; copying them made staging
/// as large as the published set (about 50 GB of textures for Skyrim), where a link costs nothing.
/// Linking is safe because nothing writes a staged output in place: textures and meshes are
/// written to a temporary file and renamed over their output, a reconverted output first unlinks
/// the staged file at its path, and `overlay_loose_assets` unlinks every path it overwrites.
/// Publishing renames staging over the output and deletes the old output, which only drops one of
/// the two links. A published tree therefore holds internal links too (a `vfs` file and its cache
/// blob are one file): a size check counts them twice, and a copy that does not keep links
/// duplicates the data.
pub(crate) fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        // Removing `to` first would delete `from` itself if the two name the same file.
        if fs::canonicalize(from)? == fs::canonicalize(to)? {
            return Ok(());
        }
        fs::remove_file(to)?;
    }
    fs::hard_link(from, to).or_else(|_| fs::copy(from, to).map(|_| ()))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path)
        .wrap_err_with(|| format!("failed to open {} for hashing", path.display()))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .wrap_err_with(|| format!("failed to hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_stable() {
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn recent_schema_migrations_reuse_only_unchanged_asset_kinds() {
        for schema_version in [12, 13] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("conversion-manifest.json");
            let mut manifest = ConversionManifest {
                schema_version,
                complete: true,
                ..ConversionManifest::default()
            };
            for output in ["meshes/a.glb", "textures/a.ktx2", "scripts/a.luau"] {
                manifest.entries.insert(
                    output.to_owned(),
                    CacheEntry {
                        source_hash: "source".to_owned(),
                        output: output.to_owned(),
                        output_size: 1,
                        output_hash: "output".to_owned(),
                    },
                );
            }
            fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

            let migrated = ConversionManifest::load(&path).unwrap();

            assert_eq!(migrated.schema_version, schema_version);
            assert!(!migrated.complete);
            assert!(!migrated.entries.contains_key("meshes/a.glb"));
            assert!(migrated.entries.contains_key("textures/a.ktx2"));
            assert!(migrated.entries.contains_key("scripts/a.luau"));
        }
    }
}
