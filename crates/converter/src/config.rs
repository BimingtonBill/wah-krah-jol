use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub data_dir: PathBuf,
    pub output_dir: PathBuf,
    #[serde(skip)]
    pub resume_staging: Option<PathBuf>,
    pub plugins_file: Option<PathBuf>,
    pub cpu_jobs: usize,
    pub io_jobs: usize,
    pub enable_ba2: bool,
    pub fail_fast: bool,
    pub invalidate_cache: bool,
    pub verify_cache: bool,
    pub texture_etc1s_quality: u8,
    pub texture_uastc_level: u8,
    pub script_abi_version: u32,
}

impl PipelineConfig {
    pub fn new(data_dir: impl Into<PathBuf>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            output_dir: output_dir.into(),
            resume_staging: None,
            plugins_file: None,
            cpu_jobs: std::thread::available_parallelism().map_or(1, usize::from),
            io_jobs: 2,
            enable_ba2: true,
            fail_fast: false,
            invalidate_cache: false,
            verify_cache: true,
            texture_etc1s_quality: 192,
            texture_uastc_level: 2,
            script_abi_version: 1,
        }
    }

    pub(crate) fn validate(&self) -> color_eyre::Result<()> {
        color_eyre::eyre::ensure!(
            self.data_dir.is_dir(),
            "Skyrim Data directory does not exist: {}",
            self.data_dir.display()
        );
        color_eyre::eyre::ensure!(self.cpu_jobs > 0, "cpu_jobs must be greater than zero");
        color_eyre::eyre::ensure!(self.io_jobs > 0, "io_jobs must be greater than zero");
        color_eyre::eyre::ensure!(
            (1..=255).contains(&self.texture_etc1s_quality),
            "texture_etc1s_quality must be between 1 and 255"
        );
        color_eyre::eyre::ensure!(
            self.texture_uastc_level <= 4,
            "texture_uastc_level must be between 0 and 4"
        );
        color_eyre::eyre::ensure!(
            self.data_dir != self.output_dir,
            "output directory must not be the Skyrim Data directory"
        );
        if let Some(staging) = &self.resume_staging {
            color_eyre::eyre::ensure!(
                staging.is_dir(),
                "resume staging directory does not exist: {}",
                staging.display()
            );
            let output_name = self
                .output_dir
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| color_eyre::eyre::eyre!("output directory has no valid name"))?;
            let staging_name = staging
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            color_eyre::eyre::ensure!(
                staging_name.starts_with(&format!("{output_name}.staging-")),
                "resume directory is not a staging directory for {}",
                self.output_dir.display()
            );
            let output_parent = self.output_dir.parent().unwrap_or_else(|| Path::new("."));
            let staging_parent = staging.parent().unwrap_or_else(|| Path::new("."));
            color_eyre::eyre::ensure!(
                std::fs::canonicalize(output_parent)? == std::fs::canonicalize(staging_parent)?,
                "resume directory must share the output directory parent"
            );
        }
        Ok(())
    }
}
