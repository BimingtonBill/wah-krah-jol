//! Splits a `.bto` shape's single glTF primitive into one primitive per
//! non-empty per-cell segment, so the engine half of distant LOD step 4 can
//! hide the part of a level-4 object LOD block that belongs to a loaded cell,
//! the way Skyrim does.
//!
//! Skyrim's level-4 `.bto` blocks store every shape as a `BSSubIndexTriShape`
//! whose triangles are split into up to 16 contiguous ranges, one per cell of
//! the block's 4x4 grid (segment index `i` = `4*dx + dy`, `dx`/`dy` the
//! cell's offset from the block's south-west cell). The NIF parser
//! (`vendor/project-wormhole-nif`) already reads that table
//! (`BSSubIndexTriShape::segments`), but until this module the exported glTF
//! collapsed every shape to one primitive and dropped it. See
//! `local/research/lod-hiding-under-loaded-cells.md` for how the table was
//! measured from the shipped game data.
//!
//! The engine-side contract this module writes to: a `.bto` shape with 2 or
//! more non-empty segments becomes one glTF primitive per non-empty segment,
//! each carrying that segment's triangle range and primitive extras
//! `{"openSkyrim": {"lodSegment": i}}` with `i` the segment's index in the
//! table (0..15). A shape with 0 or 1 segments, and every non-`.bto` model, is
//! exported exactly as before: one primitive, no `lodSegment` extra.

use color_eyre::{
    Result,
    eyre::{bail, ensure},
};
use project_wormhole_nif::{
    bs::prelude::BSGeometrySegmentData, nif_block::NifBlock, nif_file::NifFile,
};
use serde_json::{Value, json};

/// One non-empty entry of a `.bto` shape's segment table, translated into a
/// triangle range of the shape's own triangle list (triangle 0 is the
/// shape's first triangle, not the file's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LodSegment {
    /// The entry's index in the shape's segment table (0..=15): `4*dx + dy`.
    pub segment_index: u8,
    /// First triangle of the range.
    pub start_triangle: u32,
    /// Number of triangles in the range.
    pub triangle_count: u32,
}

/// Returns a `BSSubIndexTriShape`'s non-empty segments, in table order, when
/// it has 2 or more of them: the primitive split only applies then. A table
/// with 0 or 1 non-empty entries (every ordinary shape, and most `.bto`
/// shapes, which have a single segment covering the whole mesh) returns
/// `None`.
fn segments_from_table(table: &[BSGeometrySegmentData]) -> Option<Vec<LodSegment>> {
    let mut start_triangle = 0u32;
    let mut segments = Vec::new();
    for (index, entry) in table.iter().enumerate() {
        if entry.num_primitives > 0 {
            segments.push(LodSegment {
                segment_index: u8::try_from(index).ok()?,
                start_triangle,
                triangle_count: entry.num_primitives,
            });
        }
        start_triangle = start_triangle.checked_add(entry.num_primitives)?;
    }
    (segments.len() >= 2).then_some(segments)
}

/// Returns the source NIF block's non-empty segments (see
/// [`segments_from_table`]), or `None` when the block is not a
/// `BSSubIndexTriShape` or does not qualify for a split.
fn lod_segments(nif: &NifFile, block_index: u32) -> Option<Vec<LodSegment>> {
    let NifBlock::BSSubIndexTriShape(shape) = nif.blocks.get(block_index as usize)? else {
        return None;
    };
    segments_from_table(&shape.segments)
}

/// Splits every exported mesh whose source shape has 2 or more non-empty
/// segments into one glTF primitive per segment. `shape_blocks[mesh_index]`
/// is the source NIF block index for `document["meshes"][mesh_index]`, the
/// same mapping [`crate::material::publish_gltf_materials`] uses.
///
/// Each split primitive keeps the base primitive's attributes: vertex data is
/// shared between segments of the same shape, only the index range differs.
/// New accessors and buffer views are appended for the index ranges, all
/// pointing back into the existing index buffer, so no binary data moves or
/// is duplicated, and the base `POSITION` accessor (and so the mesh's bounds)
/// is untouched. Call this before
/// [`crate::material::publish_gltf_materials`], which assigns the material to
/// every primitive a mesh now has, whatever the count.
pub fn split_lod_segments(document: &mut Value, nif: &NifFile, shape_blocks: &[u32]) -> Result<()> {
    for (mesh_index, &block_index) in shape_blocks.iter().enumerate() {
        let Some(segments) = lod_segments(nif, block_index) else {
            continue;
        };
        split_mesh_primitive(document, mesh_index, &segments)?;
    }
    Ok(())
}

fn split_mesh_primitive(
    document: &mut Value,
    mesh_index: usize,
    segments: &[LodSegment],
) -> Result<()> {
    let primitives_pointer = format!("/meshes/{mesh_index}/primitives");
    let primitive_count = document
        .pointer(&primitives_pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| color_eyre::eyre::eyre!("mesh {mesh_index} has no primitive array"))?
        .len();
    ensure!(
        primitive_count == 1,
        "mesh {mesh_index} has a segmented shape but already exported {primitive_count} primitives; expected one to split"
    );
    let base_primitive = document
        .pointer(&format!("{primitives_pointer}/0"))
        .cloned()
        .expect("primitive 0 exists: primitive_count was checked above");

    let index_accessor = base_primitive
        .get("indices")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "mesh {mesh_index} has a segmented shape with no index accessor"
            )
        })? as usize;
    let accessor = document
        .pointer(&format!("/accessors/{index_accessor}"))
        .cloned()
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("index accessor {index_accessor} is out of range")
        })?;
    let component_size = match accessor.get("componentType").and_then(Value::as_u64) {
        Some(5121) => 1u64, // UNSIGNED_BYTE
        Some(5123) => 2,    // UNSIGNED_SHORT
        Some(5125) => 4,    // UNSIGNED_INT
        other => bail!("mesh {mesh_index} has an unsupported index component type {other:?}"),
    };
    let buffer_view_index = accessor
        .get("bufferView")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("index accessor {index_accessor} has no bufferView")
        })?;
    let buffer_view = document
        .pointer(&format!("/bufferViews/{buffer_view_index}"))
        .cloned()
        .ok_or_else(|| color_eyre::eyre::eyre!("bufferView {buffer_view_index} is out of range"))?;
    let view_byte_offset = buffer_view
        .get("byteOffset")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let view_byte_length = buffer_view
        .get("byteLength")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("bufferView {buffer_view_index} has no byteLength")
        })?;

    let mut new_primitives = Vec::with_capacity(segments.len());
    for segment in segments {
        let index_start = u64::from(segment.start_triangle) * 3;
        let index_count = u64::from(segment.triangle_count) * 3;
        let byte_offset = view_byte_offset + index_start * component_size;
        let byte_length = index_count * component_size;
        ensure!(
            byte_offset + byte_length <= view_byte_offset + view_byte_length,
            "LOD segment {} of mesh {mesh_index} falls outside its shape's index buffer",
            segment.segment_index
        );

        let mut segment_view = buffer_view.clone();
        segment_view["byteOffset"] = json!(byte_offset);
        segment_view["byteLength"] = json!(byte_length);
        let new_view_index = append(document, "/bufferViews", segment_view)?;

        let mut segment_accessor = accessor.clone();
        segment_accessor["bufferView"] = json!(new_view_index);
        segment_accessor["count"] = json!(index_count);
        if let Some(name) = segment_accessor.get("name").and_then(Value::as_str) {
            let name = format!("{name} | SEGMENT:{}", segment.segment_index);
            segment_accessor["name"] = json!(name);
        }
        let new_accessor_index = append(document, "/accessors", segment_accessor)?;

        let mut primitive = base_primitive.clone();
        primitive["indices"] = json!(new_accessor_index);
        primitive["extras"] = json!({ "openSkyrim": { "lodSegment": segment.segment_index } });
        new_primitives.push(primitive);
    }

    let primitives = document
        .pointer_mut(&primitives_pointer)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| color_eyre::eyre::eyre!("mesh {mesh_index} has no primitive array"))?;
    *primitives = new_primitives;
    Ok(())
}

/// Appends `value` to the array at `pointer` and returns its new index.
fn append(document: &mut Value, pointer: &str, value: Value) -> Result<u64> {
    let array = document
        .pointer_mut(pointer)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| color_eyre::eyre::eyre!("glTF document has no array at {pointer}"))?;
    let index = array.len() as u64;
    array.push(value);
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(num_primitives: u32) -> BSGeometrySegmentData {
        BSGeometrySegmentData {
            flag: 0,
            value: 0,
            num_primitives,
        }
    }

    #[test]
    fn empty_table_has_no_split() {
        assert_eq!(segments_from_table(&[]), None);
    }

    #[test]
    fn single_non_empty_segment_has_no_split() {
        // The shape most object LOD blocks use: one segment covering the
        // whole mesh, plus trailing empty cells LODGen already strips.
        assert_eq!(segments_from_table(&[entry(6)]), None);
        assert_eq!(segments_from_table(&[entry(6), entry(0), entry(0)]), None);
    }

    #[test]
    fn several_non_empty_segments_split_with_cumulative_offsets() {
        let table = [
            entry(1), // segment 0: triangles [0, 1)
            entry(0), // segment 1: empty
            entry(0), // segment 2: empty
            entry(0), // segment 3: empty
            entry(0), // segment 4: empty
            entry(2), // segment 5: triangles [1, 3)
            entry(0), // segment 6..14: empty
            entry(0),
            entry(0),
            entry(0),
            entry(0),
            entry(0),
            entry(0),
            entry(0),
            entry(0),
            entry(3), // segment 15: triangles [3, 6)
        ];
        let segments = segments_from_table(&table).expect("3 non-empty segments should split");
        assert_eq!(
            segments,
            vec![
                LodSegment {
                    segment_index: 0,
                    start_triangle: 0,
                    triangle_count: 1
                },
                LodSegment {
                    segment_index: 5,
                    start_triangle: 1,
                    triangle_count: 2
                },
                LodSegment {
                    segment_index: 15,
                    start_triangle: 3,
                    triangle_count: 3
                },
            ]
        );
    }
}
