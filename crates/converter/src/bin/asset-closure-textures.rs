use color_eyre::{Result, eyre::WrapErr};
use converter::{
    asset_path::{
        AssetKind, AssetOverride, AssetSourceIndex, canonical_asset_path, resolve_asset_uri,
    },
    texture::{Ktx2Metadata, TextureConverter, TextureEncoding, TextureSemantic},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Debug, Deserialize)]
struct ClosureReport {
    assets: Vec<ClosureAsset>,
}

#[derive(Debug, Deserialize)]
struct ClosureAsset {
    glb_path: Option<String>,
    #[serde(default)]
    missing_textures: Vec<String>,
    #[serde(default)]
    texture_dependencies: Vec<ClosureTextureDependency>,
}

#[derive(Debug, Deserialize)]
struct ClosureTextureDependency {
    uri: String,
    semantic: TextureSemantic,
    required: bool,
    status: String,
}

#[derive(Debug)]
struct TextureRequest {
    relative: PathBuf,
    required: bool,
    semantics: BTreeSet<TextureSemantic>,
}

#[derive(Debug, Serialize)]
struct TextureResult {
    path: String,
    source: Option<String>,
    source_priority: Option<usize>,
    required: bool,
    semantics: Vec<TextureSemantic>,
    encoding: Option<TextureEncoding>,
    metadata: Option<Ktx2Metadata>,
    status: &'static str,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct TextureReport {
    format_version: u32,
    converter_schema_version: u32,
    assets_root: PathBuf,
    source_roots: Vec<PathBuf>,
    overrides: Vec<AssetOverride>,
    requested: usize,
    required_requested: usize,
    optional_requested: usize,
    converted: usize,
    reused: usize,
    missing_sources: usize,
    missing_required_sources: usize,
    missing_optional_sources: usize,
    failures: usize,
    passed: bool,
    results: Vec<TextureResult>,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args = env::args_os().skip(1);
    let closure_path = required(&mut args, "closure-report.json")?;
    let report_path = required(&mut args, "texture-report.json")?;
    let assets_root = required(&mut args, "assets-root")?.canonicalize()?;
    let source_roots = args
        .map(PathBuf::from)
        .map(|path| path.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;
    color_eyre::eyre::ensure!(
        !source_roots.is_empty(),
        "at least one DDS source root is required"
    );
    let closure: ClosureReport = serde_json::from_slice(&fs::read(&closure_path)?)?;
    let requests = texture_requests(&closure, &assets_root)?;
    let requested = requests.len();
    let required_requested = requests.values().filter(|request| request.required).count();
    let optional_requested = requested - required_requested;
    let source_index = Arc::new(AssetSourceIndex::build(&source_roots, AssetKind::Texture)?);
    let overrides = source_index.overrides().to_vec();
    let queue = Arc::new(Mutex::new(VecDeque::from(
        requests.into_values().collect::<Vec<_>>(),
    )));
    let results = Arc::new(Mutex::new(Vec::with_capacity(requested)));
    let completed = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let completed = Arc::clone(&completed);
            let assets_root = &assets_root;
            let source_index = Arc::clone(&source_index);
            scope.spawn(move || {
                loop {
                    let Some(request) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let relative = request.relative;
                    let output = assets_root.join(&relative);
                    let source_key = canonical_asset_path(
                        &relative.to_string_lossy(),
                        AssetKind::Texture,
                        "dds",
                    )
                    .expect("validated texture request must remain canonical");
                    let source = source_index.get(&source_key);
                    let semantics = request.semantics.into_iter().collect::<Vec<_>>();
                    let encoding = TextureEncoding::from_semantics(
                        &semantics.iter().copied().collect::<BTreeSet<_>>(),
                    );
                    let result = if let Err(error) = encoding {
                        TextureResult {
                            path: normalize(&relative),
                            source: source.map(|entry| entry.path.to_string_lossy().into_owned()),
                            source_priority: source.map(|entry| entry.priority),
                            required: request.required,
                            semantics,
                            encoding: None,
                            metadata: None,
                            status: "failed",
                            error: Some(format!("{error:#}")),
                        }
                    } else if let Some(source) = source {
                        let encoding = encoding.expect("checked texture encoding");
                        match TextureConverter::convert_dds_to_ktx2(&source.path, &output, encoding)
                        {
                            Ok(metadata) => TextureResult {
                                path: normalize(&relative),
                                source: Some(source.path.to_string_lossy().into_owned()),
                                source_priority: Some(source.priority),
                                required: request.required,
                                semantics,
                                encoding: Some(encoding),
                                metadata: Some(metadata),
                                status: "converted",
                                error: None,
                            },
                            Err(error) => TextureResult {
                                path: normalize(&relative),
                                source: Some(source.path.to_string_lossy().into_owned()),
                                source_priority: Some(source.priority),
                                required: request.required,
                                semantics,
                                encoding: Some(encoding),
                                metadata: None,
                                status: "failed",
                                error: Some(format!("{error:#}")),
                            },
                        }
                    } else if output.is_file() {
                        let encoding = encoding.expect("checked texture encoding");
                        let inspected = fs::read(&output)
                            .map_err(color_eyre::Report::from)
                            .and_then(|bytes| converter::texture::inspect_ktx2(&bytes, encoding));
                        let (status, metadata, error) = match inspected {
                            Ok(metadata) => ("reused", Some(metadata), None),
                            Err(error) => ("failed", None, Some(format!("{error:#}"))),
                        };
                        TextureResult {
                            path: normalize(&relative),
                            source: None,
                            source_priority: None,
                            required: request.required,
                            semantics,
                            encoding: Some(encoding),
                            metadata,
                            status,
                            error,
                        }
                    } else {
                        TextureResult {
                            path: normalize(&relative),
                            source: None,
                            source_priority: None,
                            required: request.required,
                            semantics,
                            encoding: Some(encoding.expect("checked texture encoding")),
                            metadata: None,
                            status: "missing_source",
                            error: Some("DDS source is missing".to_owned()),
                        }
                    };
                    results.lock().unwrap().push(result);
                    let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if count.is_multiple_of(100) || count == requested {
                        eprintln!("Processed {count}/{requested} reachable textures");
                    }
                }
            });
        }
    });
    let mut results = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    results.sort_by_key(|result| result.path.to_ascii_lowercase());
    let converted = count_status(&results, "converted");
    let reused = count_status(&results, "reused");
    let missing_sources = count_status(&results, "missing_source");
    let missing_required_sources = results
        .iter()
        .filter(|result| result.required && result.status == "missing_source")
        .count();
    let missing_optional_sources = missing_sources - missing_required_sources;
    let failures = count_status(&results, "failed");
    let passed = converted + reused + missing_optional_sources == requested
        && missing_required_sources == 0
        && failures == 0;
    let report = TextureReport {
        format_version: 3,
        converter_schema_version: converter::cache::CONVERTER_SCHEMA_VERSION,
        assets_root,
        source_roots,
        overrides,
        requested,
        required_requested,
        optional_requested,
        converted,
        reused,
        missing_sources,
        missing_required_sources,
        missing_optional_sources,
        failures,
        passed,
        results,
    };
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("texture closure written to {}", report_path.display());
    color_eyre::eyre::ensure!(passed, "reachable texture conversion is incomplete");
    Ok(())
}

fn texture_requests(
    closure: &ClosureReport,
    assets_root: &Path,
) -> Result<BTreeMap<String, TextureRequest>> {
    let mut requests = BTreeMap::<String, TextureRequest>::new();
    for asset in &closure.assets {
        let Some(glb_path) = &asset.glb_path else {
            continue;
        };
        let glb = assets_root.join(glb_path);
        if asset.texture_dependencies.is_empty() {
            for uri in &asset.missing_textures {
                insert_request(
                    &mut requests,
                    assets_root,
                    &glb,
                    uri,
                    true,
                    TextureSemantic::Unclassified,
                )
                .wrap_err_with(|| format!("invalid texture URI in {glb_path}"))?;
            }
        } else {
            for dependency in asset.texture_dependencies.iter().filter(|dependency| {
                dependency.status == "missing" || dependency.status == "available"
            }) {
                insert_request(
                    &mut requests,
                    assets_root,
                    &glb,
                    &dependency.uri,
                    dependency.required,
                    dependency.semantic,
                )
                .wrap_err_with(|| format!("invalid texture URI in {glb_path}"))?;
            }
        }
    }
    Ok(requests)
}

fn insert_request(
    requests: &mut BTreeMap<String, TextureRequest>,
    assets_root: &Path,
    glb: &Path,
    uri: &str,
    required: bool,
    semantic: TextureSemantic,
) -> Result<()> {
    let candidate = resolve_asset_uri(assets_root, glb, uri)?;
    let relative = candidate.strip_prefix(assets_root)?;
    let canonical = canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "ktx2")?;
    let request = requests
        .entry(canonical.clone())
        .or_insert_with(|| TextureRequest {
            relative: PathBuf::from(&canonical),
            required: false,
            semantics: BTreeSet::new(),
        });
    request.required |= required;
    request.semantics.insert(semantic);
    Ok(())
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn required(args: &mut impl Iterator<Item = std::ffi::OsString>, name: &str) -> Result<PathBuf> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| color_eyre::eyre::eyre!("missing {name}"))
}

fn count_status(results: &[TextureResult], status: &str) -> usize {
    results
        .iter()
        .filter(|result| result.status == status)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_texture_uri_relative_to_glb() {
        let root = Path::new("C:/assets");
        let closure = ClosureReport {
            assets: vec![ClosureAsset {
                glb_path: Some("meshes/architecture/a.glb".to_owned()),
                missing_textures: vec!["../../textures/a.ktx2".to_owned()],
                texture_dependencies: Vec::new(),
            }],
        };
        let requests = texture_requests(&closure, root).unwrap();
        assert!(requests.contains_key("textures/a.ktx2"));
        assert!(requests["textures/a.ktx2"].required);
    }

    #[test]
    fn merges_semantics_and_required_status() {
        let root = Path::new("C:/assets");
        let closure = ClosureReport {
            assets: vec![ClosureAsset {
                glb_path: Some("meshes/a.glb".to_owned()),
                missing_textures: Vec::new(),
                texture_dependencies: vec![
                    ClosureTextureDependency {
                        uri: "../textures/a.ktx2".to_owned(),
                        semantic: TextureSemantic::Normal,
                        required: false,
                        status: "missing".to_owned(),
                    },
                    ClosureTextureDependency {
                        uri: "../textures/a.ktx2".to_owned(),
                        semantic: TextureSemantic::BaseColor,
                        required: true,
                        status: "missing".to_owned(),
                    },
                ],
            }],
        };
        let requests = texture_requests(&closure, root).unwrap();
        let request = &requests["textures/a.ktx2"];
        assert!(request.required);
        assert_eq!(request.semantics.len(), 2);
    }
}
