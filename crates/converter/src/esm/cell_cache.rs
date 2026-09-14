use crate::esm::{extractors::SubrecordView, records::RawRecord};
use color_eyre::{Result, eyre::WrapErr};
use memmap2::Mmap;
use rkyv::rancor::Error;
use shared::{CELL_CACHE_VERSION, CachedLand, CellCache, LAND_SIDE, TerrainLayer, TerrainWeight};
use std::{collections::HashMap, fs::File, io::Write, path::Path};

pub fn write_cell_cache(records: &HashMap<u32, RawRecord>, path: &Path) -> Result<usize> {
    let water_by_cell: HashMap<u32, (Option<f32>, Option<u32>)> = records
        .values()
        .filter(|record| &record.record_type == b"CELL")
        .map(|record| {
            let view = SubrecordView::new(&record.subrecords);
            let height = view
                .find(b"XCLW")
                .filter(|bytes| bytes.len() >= 4)
                .map(|bytes| {
                    f32::from_le_bytes(bytes[..4].try_into().expect("four-byte water height"))
                })
                .and_then(normalize_water_height);
            (record.form_id, (height, view.get_form_id(b"XCWT")))
        })
        .collect();
    let mut cells_by_id = HashMap::new();
    for record in records
        .values()
        .filter(|record| &record.record_type == b"LAND")
    {
        let view = SubrecordView::new(&record.subrecords);
        let heightmap = view.find(b"VHGT").unwrap_or_default();
        let cell_id = record.cell_form_id.unwrap_or(record.form_id);
        let (water_height, water_type_form_id) =
            water_by_cell.get(&cell_id).copied().unwrap_or((None, None));
        let heights = decode_vhgt(heightmap);
        let normals = decode_normals(view.find(b"VNML").unwrap_or_default(), &heights);
        let vertex_colors = view.find(b"VCLR").unwrap_or_default().to_vec();
        let mut layers = extract_texture_layers(&record.subrecords)
            .wrap_err_with(|| format!("invalid LAND layers for cell {cell_id:08X}"))?;
        normalize_texture_layers(&mut layers);
        let vertex_count = usize::from(LAND_SIDE) * usize::from(LAND_SIDE);
        color_eyre::eyre::ensure!(
            heights.len() == vertex_count,
            "LAND {cell_id:08X} has an incomplete VHGT height field"
        );
        color_eyre::eyre::ensure!(
            normals.len() == vertex_count * 3,
            "LAND {cell_id:08X} has an incomplete VNML normal field"
        );
        color_eyre::eyre::ensure!(
            vertex_colors.is_empty() || vertex_colors.len() == vertex_count * 3,
            "LAND {cell_id:08X} has an incomplete VCLR field"
        );
        for quadrant in 0..4 {
            let quadrant_layers: Vec<_> = layers
                .iter()
                .filter(|layer| layer.quadrant == quadrant)
                .collect();
            let bases = quadrant_layers.iter().filter(|layer| layer.is_base).count();
            color_eyre::eyre::ensure!(
                quadrant_layers.is_empty() || (bases == 1 && quadrant_layers.len() <= 6),
                "LAND {cell_id:08X} quadrant {quadrant} must be empty or have one BTXT and at most five ATXT layers"
            );
        }
        cells_by_id.insert(
            cell_id,
            CachedLand {
                cell_id,
                width: LAND_SIDE,
                height: LAND_SIDE,
                heights,
                normals,
                vertex_colors,
                layers,
                water_height,
                water_type_form_id,
            },
        );
    }
    let mut cells: Vec<_> = cells_by_id.into_values().collect();
    cells.sort_unstable_by_key(|cell| cell.cell_id);
    let count = cells.len();
    let bytes = rkyv::to_bytes::<Error>(&CellCache {
        version: CELL_CACHE_VERSION,
        cells,
    })
    .wrap_err("failed to serialize cell cache")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    validate_cell_cache(path)?;
    Ok(count)
}

fn normalize_water_height(height: f32) -> Option<f32> {
    // Skyrim uses FLT_MAX as the exterior-cell "no water" sentinel. Persisting
    // it as a real height creates non-finite render transforms downstream.
    (height.is_finite() && height.abs() < 1.0e7).then_some(height)
}

fn decode_vhgt(bytes: &[u8]) -> Vec<f32> {
    let count = usize::from(LAND_SIDE) * usize::from(LAND_SIDE);
    if bytes.is_empty() {
        return vec![0.0; count];
    }
    if bytes.len() < 4 + count {
        return Vec::new();
    }
    let offset = f32::from_le_bytes(bytes[..4].try_into().expect("four-byte VHGT offset")) * 8.0;
    let deltas = &bytes[4..4 + count];
    let side = usize::from(LAND_SIDE);
    let mut heights = vec![0.0; count];
    let mut row_origin = offset;
    for row in 0..side {
        row_origin += (deltas[row * side] as i8 as f32) * 8.0;
        let mut height = row_origin;
        heights[row * side] = height;
        for column in 1..side {
            height += (deltas[row * side + column] as i8 as f32) * 8.0;
            heights[row * side + column] = height;
        }
    }
    heights
}

fn decode_normals(bytes: &[u8], heights: &[f32]) -> Vec<i8> {
    let side = usize::from(LAND_SIDE);
    let count = side * side;
    if bytes.len() == count * 3 {
        return bytes.iter().map(|value| *value as i8).collect();
    }
    if !bytes.is_empty() || heights.len() != count {
        return Vec::new();
    }
    let mut normals = Vec::with_capacity(count * 3);
    for y in 0..side {
        for x in 0..side {
            let left = heights[y * side + x.saturating_sub(1)];
            let right = heights[y * side + (x + 1).min(side - 1)];
            let down = heights[y.saturating_sub(1) * side + x];
            let up = heights[(y + 1).min(side - 1) * side + x];
            let normal = [left - right, down - up, 256.0];
            let length =
                (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
            normals.extend([
                (normal[0] / length * 127.0).round() as i8,
                (normal[1] / length * 127.0).round() as i8,
                (normal[2] / length * 127.0).round() as i8,
            ]);
        }
    }
    normals
}

fn extract_texture_layers(subrecords: &[(Vec<u8>, Vec<u8>)]) -> Result<Vec<TerrainLayer>> {
    let mut layers = Vec::new();
    let mut active: Option<usize> = None;
    for (tag, data) in subrecords {
        match tag.as_slice() {
            b"BTXT" | b"ATXT" => {
                color_eyre::eyre::ensure!(
                    data.len() >= 8,
                    "{} subrecord is truncated",
                    String::from_utf8_lossy(tag)
                );
                let texture_form_id = u32::from_le_bytes(data[..4].try_into().unwrap());
                let quadrant = data[4];
                color_eyre::eyre::ensure!(
                    quadrant < 4,
                    "terrain quadrant {quadrant} is outside 0..=3"
                );
                let is_base = tag.as_slice() == b"BTXT";
                let layer = if is_base {
                    0
                } else {
                    u16::from_le_bytes(data[6..8].try_into().unwrap())
                };
                layers.push(TerrainLayer {
                    texture_form_id,
                    quadrant,
                    layer,
                    is_base,
                    weights: Vec::new(),
                });
                active = (!is_base).then_some(layers.len() - 1);
            }
            b"VTXT" => {
                color_eyre::eyre::ensure!(data.len() % 8 == 0, "VTXT payload is truncated");
                let index =
                    active.ok_or_else(|| color_eyre::eyre::eyre!("VTXT has no preceding ATXT"))?;
                for entry in data.as_chunks::<8>().0 {
                    let vertex = u16::from_le_bytes(entry[..2].try_into().unwrap());
                    let opacity = f32::from_le_bytes(entry[4..8].try_into().unwrap());
                    color_eyre::eyre::ensure!(
                        vertex < 17 * 17,
                        "VTXT vertex {vertex} is outside a LAND quadrant"
                    );
                    color_eyre::eyre::ensure!(
                        opacity.is_finite() && (0.0..=1.0).contains(&opacity),
                        "VTXT opacity {opacity} is invalid"
                    );
                    layers[index]
                        .weights
                        .push(TerrainWeight { vertex, opacity });
                }
            }
            _ => active = None,
        }
    }
    layers.sort_by_key(|layer| {
        (
            layer.quadrant,
            !layer.is_base,
            layer.layer,
            layer.texture_form_id,
        )
    });
    for layer in layers.iter().filter(|layer| !layer.is_base) {
        let mut vertices = std::collections::HashSet::new();
        color_eyre::eyre::ensure!(
            layer
                .weights
                .iter()
                .all(|weight| vertices.insert(weight.vertex)),
            "quadrant {} layer {} repeats a VTXT vertex",
            layer.quadrant,
            layer.layer
        );
    }
    for quadrant in 0..4 {
        let bases = layers
            .iter()
            .filter(|layer| layer.quadrant == quadrant && layer.is_base)
            .count();
        color_eyre::eyre::ensure!(
            bases <= 1,
            "quadrant {quadrant} has {bases} BTXT base layers"
        );
    }
    Ok(layers)
}

fn normalize_texture_layers(layers: &mut Vec<TerrainLayer>) {
    for quadrant in 0..4 {
        let has_layers = layers.iter().any(|layer| layer.quadrant == quadrant);
        let has_base = layers
            .iter()
            .any(|layer| layer.quadrant == quadrant && layer.is_base);
        if has_layers && !has_base {
            layers.push(TerrainLayer {
                texture_form_id: 0,
                quadrant,
                layer: 0,
                is_base: true,
                weights: Vec::new(),
            });
        }
        while layers
            .iter()
            .filter(|layer| layer.quadrant == quadrant)
            .count()
            > 6
        {
            let weakest = layers
                .iter()
                .enumerate()
                .filter(|(_, layer)| layer.quadrant == quadrant && !layer.is_base)
                .min_by(|(_, left), (_, right)| {
                    let left_weight: f32 = left.weights.iter().map(|weight| weight.opacity).sum();
                    let right_weight: f32 = right.weights.iter().map(|weight| weight.opacity).sum();
                    left_weight
                        .total_cmp(&right_weight)
                        .then_with(|| right.layer.cmp(&left.layer))
                })
                .map(|(index, _)| index)
                .expect("an over-capacity quadrant must contain an overlay");
            layers.remove(weakest);
        }
    }
    layers.sort_by_key(|layer| {
        (
            layer.quadrant,
            !layer.is_base,
            layer.layer,
            layer.texture_form_id,
        )
    });
}

pub fn validate_cell_cache(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let archived =
        rkyv::access::<shared::ArchivedCellCache, Error>(&mmap).wrap_err("invalid cell cache")?;
    color_eyre::eyre::ensure!(
        archived.version == CELL_CACHE_VERSION,
        "unsupported cell cache version"
    );
    Ok(mmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_vhgt_deltas_into_absolute_heights() {
        let count = usize::from(LAND_SIDE) * usize::from(LAND_SIDE);
        let mut bytes = 2.0f32.to_le_bytes().to_vec();
        bytes.extend(std::iter::repeat_n(1, count));
        let heights = decode_vhgt(&bytes);
        assert_eq!(heights.len(), count);
        assert_eq!(heights[0], 24.0);
        assert_eq!(heights[1], 32.0);
        assert_eq!(heights[usize::from(LAND_SIDE)], 32.0);
    }

    #[test]
    fn supplies_flat_geometry_when_land_omits_vhgt_and_vnml() {
        let heights = decode_vhgt(&[]);
        let normals = decode_normals(&[], &heights);
        assert_eq!(heights.len(), usize::from(LAND_SIDE).pow(2));
        assert!(heights.iter().all(|height| *height == 0.0));
        assert_eq!(normals.len(), heights.len() * 3);
        assert!(
            normals
                .as_chunks::<3>()
                .0
                .iter()
                .all(|normal| normal == &[0, 0, 127])
        );
    }

    #[test]
    fn rejects_truncated_land_geometry_payloads() {
        let heights = vec![0.0; usize::from(LAND_SIDE).pow(2)];
        assert!(decode_vhgt(&[0; 16]).is_empty());
        assert!(decode_normals(&[0; 16], &heights).is_empty());
    }

    #[test]
    fn rejects_skyrim_no_water_sentinel() {
        assert_eq!(normalize_water_height(f32::MAX), None);
        assert_eq!(normalize_water_height(f32::INFINITY), None);
        assert_eq!(normalize_water_height(-11592.0), Some(-11592.0));
    }

    #[test]
    fn preserves_btxt_base_and_reads_atxt_u16_layer() {
        let mut base = 0x1234u32.to_le_bytes().to_vec();
        base.extend([2, 0, 0, 0]);
        let mut alpha = 0x5678u32.to_le_bytes().to_vec();
        alpha.extend([2, 0]);
        alpha.extend(0x0102u16.to_le_bytes());
        let mut weight = 18u16.to_le_bytes().to_vec();
        weight.extend([0, 0]);
        weight.extend(0.75f32.to_le_bytes());
        let layers = extract_texture_layers(&[
            (b"BTXT".to_vec(), base),
            (b"ATXT".to_vec(), alpha),
            (b"VTXT".to_vec(), weight),
        ])
        .unwrap();
        assert!(layers[0].is_base);
        assert_eq!(layers[1].layer, 0x0102);
        assert_eq!(layers[1].weights[0].vertex, 18);
        assert_eq!(layers[1].weights[0].opacity, 0.75);
    }

    #[test]
    fn rejects_invalid_land_layer_payloads() {
        assert!(extract_texture_layers(&[(b"BTXT".to_vec(), vec![0; 7])]).is_err());
        let mut alpha = 1u32.to_le_bytes().to_vec();
        alpha.extend([4, 0, 0, 0]);
        assert!(extract_texture_layers(&[(b"ATXT".to_vec(), alpha)]).is_err());
    }

    #[test]
    fn supplies_implicit_base_and_drops_the_weakest_excess_overlay() {
        let mut layers = (0..6)
            .map(|layer| TerrainLayer {
                texture_form_id: u32::from(layer) + 1,
                quadrant: 2,
                layer,
                is_base: false,
                weights: vec![TerrainWeight {
                    vertex: layer,
                    opacity: if layer == 4 { 0.01 } else { 0.5 },
                }],
            })
            .collect::<Vec<_>>();
        normalize_texture_layers(&mut layers);
        assert_eq!(layers.len(), 6);
        assert!(layers[0].is_base);
        assert_eq!(layers[0].texture_form_id, 0);
        assert!(!layers.iter().any(|layer| layer.texture_form_id == 5));
    }
}
