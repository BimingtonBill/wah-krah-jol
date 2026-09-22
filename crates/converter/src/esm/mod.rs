use crate::esm::{
    binary::{parse_plugin_file, parse_plugin_metadata},
    exporter::{create_tables, export_to_db},
    records::RawRecord,
};
use color_eyre::Result;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
pub mod binary;
pub mod cell_cache;
pub mod directional_material;
pub mod exporter;
pub mod extractors;
pub mod lighting;
pub mod mmap_reader;
pub mod records;
pub mod types;

pub struct EsmParser;

impl EsmParser {
    /// Parses .esm files and exports world data to skyrim_world.db
    pub fn convert_plugins(plugin_paths: &[PathBuf], db_path: &Path) -> Result<()> {
        let conn = Connection::open(db_path)?;
        create_tables(&conn)?;
        for (priority, path) in plugin_paths.iter().enumerate() {
            let checksum = Sha256::digest(std::fs::read(path)?);
            conn.execute(
                "INSERT OR REPLACE INTO plugins (id, name, priority, checksum) VALUES (?1, ?2, ?3, ?4)",
                params![priority as i64, path.file_name().unwrap_or_default().to_string_lossy(), priority as i64, checksum.as_slice()],
            )?;
        }
        let master = Self::merge_plugins(plugin_paths)?;
        export_to_db(&conn, &master)?;

        Ok(())
    }

    pub fn merge_plugins(plugin_paths: &[PathBuf]) -> Result<HashMap<u32, RawRecord>> {
        let names: Vec<String> = plugin_paths
            .iter()
            .map(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase()
            })
            .collect();
        let mut normal_indices = HashMap::new();
        let mut light_indices = HashMap::new();
        let mut next_normal = 0u32;
        let mut next_light = 0u32;
        for (path, name) in plugin_paths.iter().zip(&names) {
            let metadata = parse_plugin_metadata(path)?;
            if path
                .extension()
                .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("esl"))
                || metadata.flags & 0x0000_0200 != 0
            {
                light_indices.insert(name.clone(), next_light);
                next_light += 1;
            } else {
                normal_indices.insert(name.clone(), next_normal);
                next_normal += 1;
            }
        }
        let mut merged = HashMap::new();
        for (priority, path) in plugin_paths.iter().enumerate() {
            let metadata = parse_plugin_metadata(path)?;
            for mut record in parse_plugin_file(path)? {
                record.load_order = priority as u32;
                remap_record_form_ids(
                    &mut record,
                    &names[priority],
                    &metadata.masters,
                    &normal_indices,
                    &light_indices,
                )?;
                if record.is_deleted() {
                    merged.remove(&record.form_id);
                } else {
                    merged.insert(record.form_id, record);
                }
            }
        }
        Ok(merged)
    }
}

pub fn read_plugins_txt(path: &Path, data_dir: &Path) -> Result<Vec<PathBuf>> {
    let contents = std::fs::read_to_string(path)?;
    let mut plugins = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let enabled = line.starts_with('*');
        let name = line.strip_prefix('*').unwrap_or(line).trim();
        if !matches!(
            Path::new(name)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("esm" | "esp" | "esl")
        ) {
            continue;
        }
        if !enabled && !name.to_ascii_lowercase().ends_with(".esm") {
            continue;
        }
        let exact = data_dir.join(name);
        if exact.is_file() {
            plugins.push(exact);
            continue;
        }
        if let Some(found) = std::fs::read_dir(data_dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|candidate| {
                candidate
                    .file_name()
                    .is_some_and(|file| file.to_string_lossy().eq_ignore_ascii_case(name))
            })
        {
            plugins.push(found);
        } else {
            color_eyre::eyre::bail!("active plugin not found: {name}");
        }
    }
    Ok(plugins)
}

/// Finds the offset of a 32-bit FormID inside a subrecord that carries one, so
/// that load-order remapping can overwrite it.
///
/// This record-aware check ensures that subrecords with shared tag names (such as
/// `CNAM` or `SNAM`) containing strings (e.g. `TES4` author/description), float
/// physics arrays (`TREE` trunk flexibility), or RGBA color structures (`CLFM`/`AACT`)
/// are not inadvertently overwritten as 4-byte FormIDs.
///
/// Three subrecords carry a FormID inside a longer payload, and each is
/// remapped at the offset the layout puts it at while the bytes around it are
/// left alone:
///
/// - a load door's `XTEL` is a destination FormID followed by six floats;
/// - a climate's `WLST` is a weather FormID followed by a chance and a global;
/// - a `STAT`'s `DNAM` is a max angle followed by the `MATO` FormID.
///
/// `XTEL` and `WLST` start with theirs, so the guard on those is one byte width
/// instead of four: a truncated record still starts with the FormID. `DNAM`'s
/// FormID sits after a float, so a `DNAM` shorter than eight bytes has no
/// FormID to remap and is left alone.
fn form_id_offset_in_subrecord(record_type: &[u8; 4], tag: &[u8], len: usize) -> Option<usize> {
    if tag.len() < 4 {
        return None;
    }
    let tag_4: &[u8; 4] = tag[..4].try_into().unwrap();
    if tag_4 == b"XTEL" && matches!(record_type, b"REFR" | b"ACHR" | b"ACRE" | b"PGRE" | b"PMIS") {
        return (len >= 4).then_some(0);
    }
    // A climate's weather list: the destination weather is the first field.
    if tag_4 == b"WLST" && record_type == b"CLMT" {
        return (len >= 4).then_some(0);
    }
    // A static's directional material: the `MATO` follows the max angle.
    if tag_4 == b"DNAM" && record_type == b"STAT" {
        return (len >= 8).then_some(4);
    }
    if len != 4 {
        return None;
    }
    match (record_type, tag_4) {
        (b"TES4" | b"CLFM" | b"AACT", _) => None,
        (b"TREE", b"CNAM") => None,
        (b"TREE", b"SNAM" | b"PFIG") if len == 4 => Some(0),
        (b"WRLD", b"WNAM" | b"CNAM" | b"RNAM" | b"TNAM") if len == 4 => Some(0),
        (b"CELL", b"XOWN" | b"XGLB" | b"XEZN" | b"XLCN" | b"XLRL") if len == 4 => Some(0),
        // The lighting template a cell or a worldspace renders with.
        (b"CELL" | b"WRLD", b"LTMP") if len == 4 => Some(0),
        (b"NPC_", b"RNAM" | b"CNAM" | b"INAM") if len == 4 => Some(0),
        (b"NPC_", b"SNAM") if len >= 4 => Some(0),
        (
            b"REFR" | b"ACHR" | b"ACRE" | b"PGRE" | b"PMIS",
            b"NAME" | b"XOWN" | b"XGLB" | b"XEZN" | b"XLCN" | b"XLRL",
        ) if len == 4 => Some(0),
        (_, b"XOWN" | b"XGLB" | b"XEZN" | b"XLCN" | b"XLRL") if len == 4 => Some(0),
        _ => None,
    }
}

/// Remaps local FormIDs within a record header, parent cell/worldspace references,
/// and relevant subrecords according to master plugin load-order indices.
fn remap_record_form_ids(
    record: &mut RawRecord,
    plugin_name: &str,
    masters: &[String],
    normal_indices: &HashMap<String, u32>,
    light_indices: &HashMap<String, u32>,
) -> Result<()> {
    let remap = |form_id: u32| -> Result<u32> {
        if form_id == 0 {
            return Ok(0);
        }
        let local_index = (form_id >> 24) as usize;
        let owner = if local_index < masters.len() {
            masters[local_index].to_ascii_lowercase()
        } else {
            plugin_name.to_owned()
        };
        if let Some(index) = light_indices.get(&owner) {
            return Ok(0xFE00_0000 | (index << 12) | (form_id & 0xFFF));
        }
        let index = normal_indices.get(&owner).ok_or_else(|| {
            color_eyre::eyre::eyre!("master {owner} is not present in load order")
        })?;
        Ok((index << 24) | (form_id & 0x00FF_FFFF))
    };
    record.form_id = remap(record.form_id)?;
    record.cell_form_id = record.cell_form_id.map(&remap).transpose()?;
    record.worldspace_form_id = record.worldspace_form_id.map(&remap).transpose()?;
    for (tag, data) in &mut record.subrecords {
        if tag.as_slice() == b"VMAD" {
            records::record_type::vmad::remap_primary_form_ids(data, &remap)?;
            continue;
        }
        if let Some(offset) = form_id_offset_in_subrecord(&record.record_type, tag, data.len()) {
            let value = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            data[offset..offset + 4].copy_from_slice(&remap(value)?.to_le_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_corrupt_tes4_author_strings_or_clfm_colors() {
        let normal_indices = HashMap::from([("skyrim.esm".to_string(), 0)]);
        let light_indices = HashMap::new();

        let mut tes4 = RawRecord {
            form_id: 0,
            record_type: *b"TES4",
            flags: 0,
            subrecords: vec![
                (b"CNAM".to_vec(), b"Bethesda Game Studios\0".to_vec()),
                (b"SNAM".to_vec(), b"Master description\0".to_vec()),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        remap_record_form_ids(
            &mut tes4,
            "skyrim.esm",
            &[],
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(tes4.subrecords[0].1, b"Bethesda Game Studios\0");
        assert_eq!(tes4.subrecords[1].1, b"Master description\0");

        let mut clfm = RawRecord {
            form_id: 0x00012345,
            record_type: *b"CLFM",
            flags: 0,
            subrecords: vec![(b"CNAM".to_vec(), vec![128, 64, 32, 255])],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        remap_record_form_ids(
            &mut clfm,
            "skyrim.esm",
            &[],
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(clfm.subrecords[0].1, vec![128, 64, 32, 255]);
    }

    #[test]
    fn remaps_the_xtel_destination_and_leaves_the_arrival_floats_alone() {
        let normal_indices = HashMap::from([
            ("skyrim.esm".to_string(), 0),
            ("update.esm".to_string(), 1),
            ("dawnguard.esm".to_string(), 2),
        ]);
        let light_indices = HashMap::new();

        // A door in `update.esm` whose destination is `dawnguard.esm`'s local
        // index 1, i.e. master index 1 of the plugin that owns the door.
        let mut xtel = 0x0100_0020u32.to_le_bytes().to_vec();
        for value in [947.038f32, 3958.835, 591.917, 0.0, 0.0, 2.96989] {
            xtel.extend_from_slice(&value.to_le_bytes());
        }
        let original = xtel.clone();
        let mut refr = RawRecord {
            form_id: 0x00000001,
            record_type: *b"REFR",
            flags: 0,
            subrecords: vec![(b"XTEL".to_vec(), xtel)],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut refr,
            "update.esm",
            &["skyrim.esm".to_string(), "dawnguard.esm".to_string()],
            &normal_indices,
            &light_indices,
        )
        .unwrap();

        let data = &refr.subrecords[0].1;
        assert_eq!(data.len(), 28);
        assert_eq!(
            u32::from_le_bytes(data[..4].try_into().unwrap()),
            0x0200_0020,
            "the destination FormID is remapped"
        );
        assert_eq!(data[4..], original[4..], "the six arrival floats are not");
    }

    #[test]
    fn remaps_the_lighting_template_the_snow_material_and_the_climate_weather() {
        let normal_indices =
            HashMap::from([("skyrim.esm".to_string(), 0), ("update.esm".to_string(), 1)]);
        let light_indices = HashMap::new();
        let masters = vec!["skyrim.esm".to_string()];

        // A cell of `update.esm` pointing at its template, master index 0 of a
        // record in `skyrim.esm` stays index 0 - so the second subrecord, whose
        // FormID is local, is what proves the remap happened.
        let mut cell = RawRecord {
            form_id: 0x00000100,
            record_type: *b"CELL",
            flags: 0,
            subrecords: vec![
                (b"LTMP".to_vec(), 0x0000_1234u32.to_le_bytes().to_vec()),
                (b"XCLL".to_vec(), vec![0; 92]),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut cell,
            "update.esm",
            &masters,
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(
            u32::from_le_bytes(cell.subrecords[0].1[..4].try_into().unwrap()),
            0x0000_1234,
            "a local FormID in the plugin that owns the record keeps its own index"
        );
        assert_eq!(cell.subrecords[1].1, vec![0u8; 92], "XCLL is not a FormID");

        // `DweFacadeTowerRoof01SnowHeavy`: 120 degrees, `MATO` 0x25129. A STAT
        // in `update.esm` naming the material, whose FormID is local to it.
        let mut dnam = 120.0f32.to_le_bytes().to_vec();
        dnam.extend_from_slice(&0x0002_5129u32.to_le_bytes());
        let mut stat = RawRecord {
            form_id: 0x00000200,
            record_type: *b"STAT",
            flags: 0,
            subrecords: vec![(b"DNAM".to_vec(), dnam)],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut stat,
            "update.esm",
            &masters,
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        let data = &stat.subrecords[0].1;
        assert_eq!(f32::from_le_bytes(data[..4].try_into().unwrap()), 120.0);
        assert_eq!(
            u32::from_le_bytes(data[4..8].try_into().unwrap()),
            0x0002_5129,
            "the MATO is remapped"
        );

        // A `DNAM` too short to hold the FormID is left completely alone.
        let mut truncated = RawRecord {
            form_id: 0x00000201,
            record_type: *b"STAT",
            flags: 0,
            subrecords: vec![(b"DNAM".to_vec(), vec![0xAA, 0xBB, 0xCC, 0xDD, 0x01, 0x00])],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut truncated,
            "update.esm",
            &masters,
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(
            truncated.subrecords[0].1,
            vec![0xAA, 0xBB, 0xCC, 0xDD, 0x01, 0x00]
        );

        // A climate's weather list: the weather is remapped, the chance (100)
        // and the global after it are not.
        let mut wlst = 0x0000_1234u32.to_le_bytes().to_vec();
        wlst.extend_from_slice(&100u32.to_le_bytes());
        wlst.extend_from_slice(&0u32.to_le_bytes());
        let original = wlst.clone();
        let mut climate = RawRecord {
            form_id: 0x00000300,
            record_type: *b"CLMT",
            flags: 0,
            subrecords: vec![
                (b"WLST".to_vec(), wlst),
                (b"FNAM".to_vec(), b"Sky\\Sun.dds\0".to_vec()),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut climate,
            "update.esm",
            &masters,
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(climate.subrecords[0].1[4..], original[4..]);
        assert_eq!(
            u32::from_le_bytes(climate.subrecords[0].1[..4].try_into().unwrap()),
            0x0000_1234
        );
        assert_eq!(climate.subrecords[1].1, b"Sky\\Sun.dds\0");

        // A worldspace's `CNAM` is its climate, and was already remapped.
        let mut world = RawRecord {
            form_id: 0x00000400,
            record_type: *b"WRLD",
            flags: 0,
            subrecords: vec![(b"CNAM".to_vec(), 0x0000_0812u32.to_le_bytes().to_vec())],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut world,
            "update.esm",
            &masters,
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(
            u32::from_le_bytes(world.subrecords[0].1[..4].try_into().unwrap()),
            0x0000_0812
        );
    }

    #[test]
    fn remaps_actual_form_id_subrecords() {
        let normal_indices =
            HashMap::from([("skyrim.esm".to_string(), 0), ("update.esm".to_string(), 1)]);
        let light_indices = HashMap::new();

        let mut refr = RawRecord {
            form_id: 0x00000001,
            record_type: *b"REFR",
            flags: 0,
            subrecords: vec![
                (b"NAME".to_vec(), 0x00000020u32.to_le_bytes().to_vec()),
                (b"XOWN".to_vec(), 0x00000030u32.to_le_bytes().to_vec()),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut refr,
            "update.esm",
            &["skyrim.esm".to_string()],
            &normal_indices,
            &light_indices,
        )
        .unwrap();
        assert_eq!(
            u32::from_le_bytes(refr.subrecords[0].1[..4].try_into().unwrap()),
            0x00000020
        );
        assert_eq!(
            u32::from_le_bytes(refr.subrecords[1].1[..4].try_into().unwrap()),
            0x00000030
        );
    }
}
