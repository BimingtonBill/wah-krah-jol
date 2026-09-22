//! Directional snow: the projected material a snow-covered `STAT` is drawn with.
//!
//! Skyrim's snow statics are the plain model plus a *projected material*. There is no
//! `*snow*.nif` for the Dwemer facades: `DweFacadeTowerRoof01SnowHeavy` names
//! `Dungeons\Dwemer\Facades\DweFacadeTowerRoof01.nif`, the same model the bare roof names, and the
//! only thing that makes it snow-covered is the `STAT`'s `DNAM` (`docs/research/visual-gaps-spec.md`,
//! gap 3). `DNAM` is a maximum angle and a `MATO`; the `MATO` is the material itself - a projection
//! vector, a falloff, a normal dampener and a flat colour. Without this module every such static
//! renders bare, which is why the roofs of the Alftand ravine come out bronze.
//!
//! [`DirectionalSnowCatalog`] reads the two tables the converter publishes
//! (`statics.material_object` / `statics.material_max_angle` and `matos`, converter schema 16) with
//! its own read-only connection and its own probes. A database without them - every database
//! converted before schema 16, which is what ships today - yields an empty catalog and the engine
//! spawns exactly what it spawned before.
//!
//! # The coverage formula
//!
//! Skyrim's shader is not documented and its data was not readable until the schema-16
//! reconversion, so the formula below is **this engine's reading**, fitted by eye against the
//! reference screenshots (see the calibration plan in the impl-044 report). Everything is in
//! **world space**:
//!
//! ```text
//! up        = normalize(-dir_proj)                       // the axis the snow falls along
//! n_damped  = normalize(n + up * max(normal_dampener, 0)) // the surface normal, pulled towards it
//! cos_tilt  = dot(n_damped, up)
//! window    = 1 - cos(max_angle)
//! drive     = clamp((cos_tilt - cos(max_angle)) / window, 0, 1)
//! coverage  = clamp((drive - falloff_bias) / falloff_scale, 0, 1)
//! colour    = mix(base.rgb, single_pass_colour, coverage * vertex_alpha)
//! ```
//!
//! Parameter by parameter, and why:
//!
//! * **`dir_proj` is world space.** The one `MATO` the snow statics share holds `(0, 0, -1)`, which
//!   in Creation space is straight down - the direction snow falls in, and a world direction, not
//!   a model one: a roof rotated about its own axes would otherwise have its "down" rotated with
//!   it, and the vector would not be axis-aligned. `creation_to_runtime_vector` turns it into
//!   `(0, -1, 0)` in Bevy's Y-up space, so `up` is `+Y`.
//! * **`max_angle` is a hard cut, measured from `up`**, not from `dir_proj`: 120 degrees must cover
//!   *more* than 90, and the heavy roof is the 120 one. Surfaces tilted further than `max_angle`
//!   from straight up get no snow at all, and `drive` re-scales the surviving cone to `[0, 1]`, so
//!   the falloff below acts over the whole window whatever the angle is. At 90 the window is the
//!   upper hemisphere; at 120 it reaches 30 degrees past vertical onto overhangs, which is what
//!   makes the heavy roof's walls snowier.
//! * **`falloff_bias` is where snow starts, `falloff_scale` how wide the ramp is.** With the snow
//!   `MATO`'s 0.4 and 0.35 the material fades in over a `drive` of 0.4 to 0.75. This is the
//!   reading in which `bias` is a threshold: the other one (`cos * scale + bias`) would leave a
//!   bias-wide floor of snow on every vertical wall, which is not what the references show.
//! * **`normal_dampener` pulls the normal towards the fall axis** before the ramp is evaluated,
//!   which softens the boundary and lets steeper faces take some snow. A dampener of 0 is the plain
//!   cosine ramp. At the snow material's 0.4, a surface's *tilt from up* has to be under about 87
//!   degrees for the 90-degree statics (the arch and the three lighter facades) to be touched at
//!   all and under about 56 to be fully covered; the heavy roof's 120-degree window widens both to
//!   about 107 and about 69, so its vertical walls take a little over half. Those four angles are
//!   what `the_documented_tilt_angles_are_where_the_material_fades` pins.
//! * **Vertex colour alpha** multiplies the coverage when the mesh carries `COLOR_0`. No converted
//!   mesh does today (`crates/converter/src/mesh.rs` writes no vertex colours), so this term is
//!   live only if the converter starts publishing them.
//!
//! # Calibration
//!
//! The parameters above are the ones the calibration pass fits; the fit is expected to move
//! `normal_dampener` first (it is the least certain reading), then the `max_angle`/`drive` split.
//! Changing [`coverage_for_normal`] and `snow_coverage` in `snow.wgsl` together keeps the mirror
//! honest; the unit tests below pin the shape, not the numbers.

use bevy::{
    asset::AssetId,
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
};
use rusqlite::{Connection, OpenFlags};
use std::{collections::HashMap, path::Path};

/// The snow-extended standard material: a snow static's own material, plus the projection.
pub type SnowMaterial = ExtendedMaterial<StandardMaterial, SnowExtension>;

/// A `matos` row: the projected material itself, in Bevy's world space.
///
/// Converted once at load time rather than per reference, so the shader receives a vector it can
/// use as it stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectionalMaterial {
    pub falloff_scale: f32,
    pub falloff_bias: f32,
    /// The projection vector: the direction the material is projected *along*, which for snow is
    /// the direction it falls. Bevy world space (`creation_to_runtime_vector` of the record's).
    pub dir_proj: Vec3,
    pub normal_dampener: f32,
    /// The single-pass colour, `(r, g, b)` as the record's 8-bit channels.
    pub color: [u8; 3],
    /// The record's trailing flag. Carried into the settings; see [`SnowCoverage::single_pass`].
    pub single_pass: bool,
}

/// What one snow static draws with: a `matos` row resolved against its `DNAM` max angle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnowCoverage {
    /// The axis snow falls along, `-dir_proj` normalised: Bevy world space, so `+Y` for the snow
    /// statics. A zero-length `dir_proj` falls back to `+Y` rather than producing a NaN axis.
    pub up: Vec3,
    /// `cos(max_angle)` in degrees. `1.0` is a closed window - a static whose angle is 0 gets no
    /// snow, which is what `Max Angle` 0 asks for.
    pub cos_max_angle: f32,
    pub falloff_scale: f32,
    pub falloff_bias: f32,
    pub normal_dampener: f32,
    /// The single-pass colour, `(r, g, b)` as the record's 8-bit channels.
    pub color: [u8; 3],
    /// The record's trailing flag: whether the material is drawn in one pass. The engine draws one
    /// pass either way (the base material and the snow in one fragment shader), so this is carried
    /// for the calibration and for the report rather than branched on.
    pub single_pass: bool,
}

impl SnowCoverage {
    /// One material's coverage of a static whose `DNAM` gave `max_angle` degrees.
    pub fn new(material: &DirectionalMaterial, max_angle: f32) -> Self {
        let axis = -material.dir_proj;
        let up = if axis.length_squared() > 1.0e-12 {
            axis.normalize()
        } else {
            Vec3::Y
        };
        let cos_max_angle = max_angle.to_radians().cos();
        Self {
            up,
            // An angle outside 0..180 is not a cone the shader can use: clamp the cosine rather
            // than let a nonsensical record produce a window wider than the sphere.
            cos_max_angle: if max_angle.is_finite() {
                cos_max_angle.clamp(-1.0, 1.0)
            } else {
                1.0
            },
            falloff_scale: material.falloff_scale,
            falloff_bias: material.falloff_bias,
            normal_dampener: material.normal_dampener,
            color: material.color,
            single_pass: material.single_pass,
        }
    }

    /// The colour the snow is drawn in, in the linear space a material uniform carries.
    pub fn linear_color(&self) -> LinearRgba {
        Color::srgb_u8(self.color[0], self.color[1], self.color[2]).to_linear()
    }
}

/// The snow coverage of a surface, 0 (bare) to 1 (fully snowed).
///
/// This is the CPU mirror of `snow_coverage` in `crates/engine/src/shaders/snow.wgsl` - the shader
/// is what draws, this is what the tests pin. They have to agree; change them together.
pub fn coverage_for_normal(normal: Vec3, coverage: &SnowCoverage) -> f32 {
    let dampened = dampened_normal(normal, coverage.up, coverage.normal_dampener);
    let cos_tilt = dampened.dot(coverage.up);
    let window = 1.0 - coverage.cos_max_angle;
    if window <= 1.0e-6 {
        return 0.0;
    }
    let drive = ((cos_tilt - coverage.cos_max_angle) / window).clamp(0.0, 1.0);
    if coverage.falloff_scale <= 1.0e-6 {
        // A record with no ramp is a step at the bias, not a division by zero.
        return if drive >= coverage.falloff_bias {
            1.0
        } else {
            0.0
        };
    }
    ((drive - coverage.falloff_bias) / coverage.falloff_scale).clamp(0.0, 1.0)
}

/// The surface normal pulled `dampener` of the way towards the fall axis, unit length.
///
/// Two inputs have no direction to normalise. A normal pointing straight down the fall axis with a
/// dampener of 1 cancels exactly, and a degenerate or non-finite normal is not a direction at all;
/// both fall back to the axis, which reads as a fully covered surface. The alternative - letting a
/// division through - would put a NaN in the colour mix, and a NaN base colour is a whole surface
/// of undefined colour rather than one triangle too white.
///
/// `snow.wgsl` spells the finiteness test out on its own, because WGSL has no `is_finite`; the two
/// have to return the same thing for every input, so the branches are the same branches.
pub fn dampened_normal(normal: Vec3, up: Vec3, dampener: f32) -> Vec3 {
    let raw = normal + up * dampener.max(0.0);
    // A non-finite sum is the case that has to go first: `raw.length()` of one is NaN or infinite,
    // and a NaN slips through any `<=` test. Once `raw` is finite its length cannot be NaN - only
    // zero, or an infinity from overflow, which normalises to zero rather than to a NaN.
    if !raw.is_finite() {
        return up;
    }
    let length = raw.length();
    if length <= 1.0e-6 {
        return up;
    }
    raw / length
}

/// The `matos` table and the two snow columns of `statics`, preloaded at startup.
///
/// Empty - and the engine unchanged - when the database has no `matos` table or no
/// `statics.material_object` column, which is every database converted before schema 16.
#[derive(Resource, Default, Debug)]
pub struct DirectionalSnowCatalog {
    materials: HashMap<u32, DirectionalMaterial>,
    /// `statics.id` -> the `MATO` it points at and its `DNAM` max angle in degrees.
    statics: HashMap<u32, (u32, f32)>,
}

impl DirectionalSnowCatalog {
    /// Reads the snow tables out of a converted database.
    ///
    /// A missing file, table or column is not an error: the converter publishes all of them
    /// together, and a database without them is one this engine draws exactly as it did before
    /// there was any snow material. A database that simply predates the tables says so at debug
    /// level; anything else that stops the read is a warning, because it is not the shape of the
    /// data but something wrong with the file.
    pub fn open(path: &Path) -> Self {
        let connection = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(connection) => connection,
            Err(error) => {
                warn!(?path, %error, "cannot open the world database for snow materials; no static will be snowed");
                return Self::default();
            }
        };
        if !has_table(&connection, "matos")
            || !has_column(&connection, "statics", "material_object")
        {
            debug!("the database predates the snow tables; no static is snowed");
            return Self::default();
        }
        if !has_column(&connection, "statics", "material_max_angle") {
            warn!(
                "`statics.material_object` is present without `material_max_angle`; no static is snowed"
            );
            return Self::default();
        }
        let materials = match read_materials(&connection) {
            Ok(materials) => materials,
            Err(error) => {
                warn!(%error, "cannot read the `matos` table; no static is snowed");
                return Self::default();
            }
        };
        let statics = match read_snow_statics(&connection) {
            Ok(statics) => statics,
            Err(error) => {
                warn!(%error, "cannot read the snow columns of `statics`; no static is snowed");
                return Self::default();
            }
        };
        info!(
            materials = materials.len(),
            statics = statics.len(),
            "directional snow materials loaded"
        );
        Self { materials, statics }
    }

    /// What a reference whose base object is `static_form_id` draws with, or `None` for a static
    /// with no `MATO` - which stays exactly as the model published it.
    pub fn coverage_for(&self, static_form_id: u32) -> Option<SnowCoverage> {
        let (material_id, max_angle) = *self.statics.get(&static_form_id)?;
        let material = self.materials.get(&material_id)?;
        Some(SnowCoverage::new(material, max_angle))
    }

    /// How many statics carry a snow material. Reported at startup and by the tests.
    pub fn snowed_static_count(&self) -> usize {
        self.statics.len()
    }
}

/// The one `MATO` every snow static in `Skyrim.esm` points at, `SnowMaterialObject1P` (0x25129),
/// exactly as `crates/converter/src/esm/directional_material.rs` reads it out of the record: a
/// falloff of 0.35 and 0.4, a projection straight down, a normal dampener of 0.4, the colour
/// (107, 116, 126) and the single-pass flag set.
///
/// It is here rather than hard-coded in each test because the fit is against this one record, and a
/// caller that needs a snow material without a database - the streaming tests do - should get the
/// real one. `dir_proj` is the record's `(0, 0, -1)` already mapped into this engine's space by
/// `shared::coordinates::creation_to_runtime_vector`, which is what [`read_materials`] does with
/// it, so it is `-Y` and not the record's own `-Z`.
pub fn snow_material_object_1p() -> DirectionalMaterial {
    DirectionalMaterial {
        falloff_scale: 0.35,
        falloff_bias: 0.4,
        dir_proj: Vec3::new(0.0, -1.0, 0.0),
        normal_dampener: 0.4,
        color: [107, 116, 126],
        single_pass: true,
    }
}

fn has_table(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .is_ok_and(|count| count > 0)
}

fn has_column(connection: &Connection, table: &str, column: &str) -> bool {
    connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name=?2",
            [table, column],
            |row| row.get::<_, i64>(0),
        )
        .is_ok_and(|count| count > 0)
}

fn read_materials(connection: &Connection) -> rusqlite::Result<HashMap<u32, DirectionalMaterial>> {
    let mut statement = connection.prepare(
        "SELECT id,falloff_scale,falloff_bias,dir_proj_x,dir_proj_y,dir_proj_z,\
         normal_dampener,single_pass_color,single_pass FROM matos",
    )?;
    let rows = statement.query_map([], |row| {
        let packed: u32 = row.get(7)?;
        let dir_proj = Vec3::new(row.get(3)?, row.get(4)?, row.get(5)?);
        Ok((
            row.get::<_, u32>(0)?,
            DirectionalMaterial {
                falloff_scale: row.get(1)?,
                falloff_bias: row.get(2)?,
                dir_proj: Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
                    dir_proj.to_array(),
                )),
                normal_dampener: row.get(6)?,
                color: [
                    (packed & 0xff) as u8,
                    ((packed >> 8) & 0xff) as u8,
                    ((packed >> 16) & 0xff) as u8,
                ],
                single_pass: row.get::<_, i64>(8)? != 0,
            },
        ))
    })?;
    rows.collect()
}

fn read_snow_statics(connection: &Connection) -> rusqlite::Result<HashMap<u32, (u32, f32)>> {
    let mut statement = connection.prepare(
        "SELECT id,material_object,material_max_angle FROM statics WHERE material_object IS NOT NULL",
    )?;
    let rows = statement.query_map([], |row| {
        let max_angle: Option<f32> = row.get(2)?;
        Ok((
            row.get::<_, u32>(0)?,
            row.get::<_, u32>(1)?,
            max_angle.unwrap_or(f32::NAN),
        ))
    })?;
    Ok(rows
        .filter_map(std::result::Result::ok)
        // A static whose angle is not a number is not given snow rather than given the wrong
        // amount of it: the record always carries both fields, so this is a hand-written database.
        .filter(|(_, _, max_angle)| max_angle.is_finite())
        .map(|(id, material, max_angle)| (id, (material, max_angle)))
        .collect())
}

/// A catalogue built from values rather than from a database.
///
/// `snow.rs`'s own tests are what check the read; the streaming tests need a static that carries a
/// material without a fixture database of their own.
#[cfg(test)]
impl DirectionalSnowCatalog {
    pub(crate) fn from_parts(
        statics: &[(u32, u32, f32)],
        materials: &[(u32, DirectionalMaterial)],
    ) -> Self {
        Self {
            materials: materials.iter().copied().collect(),
            statics: statics
                .iter()
                .map(|(id, material, angle)| (*id, (*material, *angle)))
                .collect(),
        }
    }
}

/// The per-material uniform: one `matos` row resolved against one static's `DNAM`.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct SnowExtension {
    #[uniform(100)]
    settings: SnowSettings,
}

/// The uniform's layout, which `snow.wgsl` declares field for field.
#[derive(ShaderType, Reflect, Debug, Clone)]
struct SnowSettings {
    /// `xyz`: the axis the snow falls along (`-dir_proj`), unit length, Bevy world space.
    /// `w`: `cos(max_angle)`, the half-angle of the cone the material covers.
    axis_and_cos_max: Vec4,
    /// `x` falloff scale, `y` falloff bias, `z` normal dampener, `w` the record's single-pass flag.
    falloff_and_dampener: Vec4,
    /// The projected material's colour, linear and opaque.
    color: Vec4,
}

impl SnowExtension {
    /// The extension of one snow static's material.
    pub fn new(coverage: &SnowCoverage) -> Self {
        let color = coverage.linear_color();
        Self {
            settings: SnowSettings {
                axis_and_cos_max: Vec4::new(
                    coverage.up.x,
                    coverage.up.y,
                    coverage.up.z,
                    coverage.cos_max_angle,
                ),
                falloff_and_dampener: Vec4::new(
                    coverage.falloff_scale,
                    coverage.falloff_bias,
                    coverage.normal_dampener,
                    if coverage.single_pass { 1.0 } else { 0.0 },
                ),
                color: Vec4::new(color.red, color.green, color.blue, 1.0),
            },
        }
    }

    /// The settings this extension carries, for the tests and for the report.
    pub fn axis_and_cos_max(&self) -> Vec4 {
        self.settings.axis_and_cos_max
    }

    pub fn falloff_and_dampener(&self) -> Vec4 {
        self.settings.falloff_and_dampener
    }
}

impl Default for SnowExtension {
    /// A closed window: `cos(max_angle)` of 1 covers nothing, so a material built without a
    /// `MATO` behind it draws exactly as its base does.
    fn default() -> Self {
        Self {
            settings: SnowSettings {
                axis_and_cos_max: Vec4::new(0.0, 1.0, 0.0, 1.0),
                falloff_and_dampener: Vec4::new(1.0, 0.0, 0.0, 0.0),
                color: Vec4::new(1.0, 1.0, 1.0, 1.0),
            },
        }
    }
}

impl MaterialExtension for SnowExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://engine/shaders/snow.wgsl".into()
    }
}

/// The base material and the projection together, which is what a snow static's meshes are handed.
pub fn snow_material(base: StandardMaterial, coverage: &SnowCoverage) -> SnowMaterial {
    SnowMaterial {
        base,
        extension: SnowExtension::new(coverage),
    }
}

/// A reference whose static carries a `MATO`, waiting for its scene to be validated and then
/// swapped onto [`SnowMaterial`].
///
/// The swap waits for `PendingAssetProfile` to go: the readiness pass validates every spawned
/// mesh's `MeshMaterial3d<StandardMaterial>` - its alpha mode, its culling, its emissive, the
/// colour space of every texture - and a mesh that has already been moved to another material
/// would fail that check as one with no material at all. Validating first and swapping after also
/// means the snow base is the material the glTF handler published, blend pair and emissive
/// included, rather than the raw glTF one.
#[derive(Component, Debug, Clone, Copy)]
pub struct DirectionalSnow {
    /// The static the reference is placed from, which is what the material cache is keyed on.
    pub static_form_id: u32,
    pub coverage: SnowCoverage,
}

/// One [`SnowMaterial`] per (model material, snow static) pair.
///
/// Every reference of a snow static shares one model, and the model's materials are shared too:
/// without the cache each *mesh of each reference* would add its own material asset. The cache
/// grows with the distinct pairs the run streams, not with the references that use them.
#[derive(Resource, Default)]
pub struct SnowMaterialCache {
    materials: HashMap<(AssetId<StandardMaterial>, u32), Handle<SnowMaterial>>,
}

impl SnowMaterialCache {
    /// The material for `base` under `coverage`, building it once per pair.
    pub fn material_for(
        &mut self,
        base: AssetId<StandardMaterial>,
        snow: &DirectionalSnow,
        materials: &Assets<StandardMaterial>,
        snow_materials: &mut Assets<SnowMaterial>,
    ) -> Option<Handle<SnowMaterial>> {
        if let Some(handle) = self.materials.get(&(base, snow.static_form_id)) {
            return Some(handle.clone());
        }
        let base_material = materials.get(base)?.clone();
        let handle = snow_materials.add(snow_material(base_material, &snow.coverage));
        self.materials
            .insert((base, snow.static_form_id), handle.clone());
        Some(handle)
    }

    /// How many materials the cache holds, for the tests.
    pub fn len(&self) -> usize {
        self.materials.len()
    }

    pub fn is_empty(&self) -> bool {
        self.materials.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DDL the converter publishes for the snow tables, copied from
    /// `crates/converter/src/esm/exporter.rs` (schema 16). A test database that drifts from it
    /// would prove nothing, so it is the converter's own text.
    const SCHEMA: &str = "
        CREATE TABLE statics (
            id INTEGER PRIMARY KEY, editor_id TEXT, model_path TEXT, flags INTEGER NOT NULL,
            bounds_min_x REAL NOT NULL DEFAULT -64, bounds_min_y REAL NOT NULL DEFAULT -64,
            bounds_min_z REAL NOT NULL DEFAULT -64, bounds_max_x REAL NOT NULL DEFAULT 64,
            bounds_max_y REAL NOT NULL DEFAULT 64, bounds_max_z REAL NOT NULL DEFAULT 64,
            bounds_valid INTEGER NOT NULL DEFAULT 0,
            material_object INTEGER,
            material_max_angle REAL
        );
        CREATE TABLE matos (
            id INTEGER PRIMARY KEY, editor_id TEXT,
            falloff_scale REAL NOT NULL, falloff_bias REAL NOT NULL,
            noise_uv_scale REAL NOT NULL, material_uv_scale REAL NOT NULL,
            dir_proj_x REAL NOT NULL, dir_proj_y REAL NOT NULL, dir_proj_z REAL NOT NULL,
            normal_dampener REAL NOT NULL,
            single_pass_color INTEGER NOT NULL, single_pass INTEGER NOT NULL
        );";

    /// `DweFacadeTowerRoof01SnowHeavy` (0xDC850): 120 degrees, `MATO` 0x25129.
    const HEAVY_ROOF: u32 = 0x000D_C850;
    /// `DweFacadeTowerArch01Snow` (0x6DD66): 90 degrees, the same `MATO`.
    const ARCH: u32 = 0x0006_DD66;
    /// The `MATO` both of them point at, `SnowMaterialObject1P`.
    const SNOW_MATERIAL_OBJECT: u32 = 0x0002_5129;
    /// The values of `MATO` 0x25129 as `crates/converter/src/esm/directional_material.rs` reads
    /// them out of the record, with the colour packed the way the database packs every colour.
    const SNOW_MATERIAL_COLOR: u32 = 107 | (116 << 8) | (126 << 16);

    /// The two tables as the converter writes them, plus a static with no `MATO` at all.
    fn snow_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO matos VALUES
                   ({SNOW_MATERIAL_OBJECT},'SnowMaterialObject1P',0.35,0.4,48.0,170.66667,
                    0.0,0.0,-1.0,0.4,{SNOW_MATERIAL_COLOR},1);
                 INSERT INTO statics (id,editor_id,flags,material_object,material_max_angle) VALUES
                   ({HEAVY_ROOF},'DweFacadeTowerRoof01SnowHeavy',0,{SNOW_MATERIAL_OBJECT},120.0),
                   ({ARCH},'DweFacadeTowerArch01Snow',0,{SNOW_MATERIAL_OBJECT},90.0),
                   (256,'DweFacadeTowerRoof01',0,NULL,NULL);"
            ))
            .unwrap();
    }

    fn catalog_from(directory: &tempfile::TempDir) -> DirectionalSnowCatalog {
        let path = directory.path().join("snow.db");
        snow_database(&path);
        DirectionalSnowCatalog::open(&path)
    }

    /// The snow static's coverage as the database above publishes it.
    fn heavy_roof() -> SnowCoverage {
        let directory = tempfile::tempdir().unwrap();
        catalog_from(&directory)
            .coverage_for(HEAVY_ROOF)
            .expect("the heavy roof carries a MATO")
    }

    fn arch() -> SnowCoverage {
        let directory = tempfile::tempdir().unwrap();
        catalog_from(&directory)
            .coverage_for(ARCH)
            .expect("the arch carries a MATO")
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-5,
            "{actual} is not {expected}"
        );
    }

    #[test]
    fn reads_the_snow_material_and_the_two_angles() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog_from(&directory);
        assert_eq!(catalog.snowed_static_count(), 2);

        let roof = catalog.coverage_for(HEAVY_ROOF).unwrap();
        // `dir_proj` is (0, 0, -1) in Creation space - straight down - so the axis snow falls
        // along is Bevy's +Y, and that is the only axis this material can produce.
        assert_eq!(roof.up, Vec3::Y);
        assert_eq!(roof.falloff_scale, 0.35);
        assert_eq!(roof.falloff_bias, 0.4);
        assert_eq!(roof.normal_dampener, 0.4);
        assert_eq!(roof.color, [107, 116, 126]);
        assert!(roof.single_pass);
        assert_close(roof.cos_max_angle, 120f32.to_radians().cos());
        assert_close(roof.cos_max_angle, -0.5);

        let arch = catalog.coverage_for(ARCH).unwrap();
        assert_eq!(arch.up, roof.up);
        assert_close(arch.cos_max_angle, 0.0);
        // The two share a material and differ only in the angle, so the angle has to reach the
        // settings on its own: a catalogue that keyed the material alone would give them one
        // coverage.
        assert_ne!(arch.cos_max_angle, roof.cos_max_angle);

        // A static with no `MATO` is untouched, which is every ordinary static in the game.
        assert!(catalog.coverage_for(256).is_none());
        assert!(catalog.coverage_for(0x0001_0000).is_none());
    }

    #[test]
    fn a_database_without_the_snow_tables_is_empty() {
        // The published schema-15 database: the tables the feature reads are not there, and the
        // engine has to draw exactly what it drew before.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE statics (
                     id INTEGER PRIMARY KEY, editor_id TEXT, model_path TEXT,
                     flags INTEGER NOT NULL, bounds_valid INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO statics (id, editor_id, flags) VALUES (1, 'Rock', 0);",
            )
            .unwrap();
        let catalog = DirectionalSnowCatalog::open(&path);
        assert_eq!(catalog.snowed_static_count(), 0);
        assert!(catalog.coverage_for(1).is_none());

        // `matos` without the statics columns is the same: the feature needs both, and half of it
        // is not a reason to snow anything.
        let half = directory.path().join("half.db");
        Connection::open(&half)
            .unwrap()
            .execute_batch(&format!("{SCHEMA} DROP TABLE statics;"))
            .unwrap();
        let catalog = DirectionalSnowCatalog::open(&half);
        assert_eq!(catalog.snowed_static_count(), 0);

        // A path that does not exist at all: empty, and no panic. This is what a fixture run opens.
        let catalog = DirectionalSnowCatalog::open(&directory.path().join("absent.db"));
        assert_eq!(catalog.snowed_static_count(), 0);
        assert!(catalog.coverage_for(HEAVY_ROOF).is_none());
    }

    #[test]
    fn a_static_whose_angle_is_null_is_skipped_rather_than_guessed_at() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("snow.db");
        snow_database(&path);
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE statics SET material_max_angle=NULL WHERE id=?1",
                [ARCH],
            )
            .unwrap();
        let catalog = DirectionalSnowCatalog::open(&path);
        assert_eq!(catalog.snowed_static_count(), 1);
        assert!(catalog.coverage_for(ARCH).is_none());
        assert!(catalog.coverage_for(HEAVY_ROOF).is_some());
    }

    #[test]
    fn the_up_axis_is_minus_the_projection_vector() {
        // The reading that could be wrong is the sign: with `dir_proj` taken as the axis itself,
        // every up-facing surface would be bare and every downward-facing one snowed. The snow
        // material's own vector is what makes it checkable.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("snow.db");
        snow_database(&path);
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE matos SET dir_proj_x=1.0, dir_proj_y=0.0, dir_proj_z=0.0",
                [],
            )
            .unwrap();
        let catalog = DirectionalSnowCatalog::open(&path);
        let coverage = catalog.coverage_for(HEAVY_ROOF).unwrap();
        // Creation +X is runtime +X, and `-dir_proj` is what the material's projection runs
        // against.
        assert_eq!(coverage.up, Vec3::NEG_X);
        assert_close(coverage_for_normal(Vec3::NEG_X, &coverage), 1.0);
        assert_close(coverage_for_normal(Vec3::X, &coverage), 0.0);
    }

    #[test]
    fn an_up_facing_surface_is_fully_snowed_and_a_down_facing_one_is_bare() {
        let roof = heavy_roof();
        assert_close(coverage_for_normal(Vec3::Y, &roof), 1.0);
        assert_close(coverage_for_normal(Vec3::NEG_Y, &roof), 0.0);
        // A tilted roof is snowier the flatter it is: the whole point of the falloff.
        let mut previous = f32::INFINITY;
        for degrees in [0.0f32, 20.0, 40.0, 60.0, 80.0] {
            let radians = degrees.to_radians();
            let normal = Vec3::new(radians.sin(), radians.cos(), 0.0);
            let coverage = coverage_for_normal(normal, &roof);
            assert!(coverage <= previous + 1e-6, "{degrees} degrees went up");
            assert!((0.0..=1.0).contains(&coverage));
            previous = coverage;
        }
        // A vertical wall of the *arch* - the 90-degree window - is bare: the ramp, not the window,
        // is what decides there.
        let arch = arch();
        assert_close(coverage_for_normal(Vec3::X, &arch), 0.0);
        // The heavy roof's 120-degree window reaches past vertical, so the same wall takes some
        // snow - about half, from the 0.4 to 0.75 ramp over the 1.5-wide window. That difference is
        // what the angle is for.
        let wall = coverage_for_normal(Vec3::X, &roof);
        assert!(
            (0.4..0.6).contains(&wall),
            "the heavy roof's wall reads {wall}"
        );
        // And a surface that faces away from the sky is bare whatever the window: an overhang's
        // normal points along the fall.
        assert_close(coverage_for_normal(Vec3::NEG_Y, &roof), 0.0);
    }

    /// The four angles the module doc quotes: where each window's ramp starts and where it
    /// saturates, measured as a tilt of the surface from straight up. Walking them out is what
    /// makes the doc's numbers checkable - a fit that moves `normal_dampener` or the window moves
    /// these, and the doc has to move with them.
    #[test]
    fn the_documented_tilt_angles_are_where_the_material_fades() {
        fn normal_at(degrees: f32) -> Vec3 {
            let radians = degrees.to_radians();
            Vec3::new(radians.sin(), radians.cos(), 0.0)
        }
        // The steepest tilt that still gets any snow, and the steepest that is still fully covered.
        fn tilt_range(coverage: &SnowCoverage) -> (i32, i32) {
            let mut touched = -1;
            let mut full = -1;
            for degrees in 0..=179 {
                let normal = normal_at(degrees as f32);
                let amount = coverage_for_normal(normal, coverage);
                if amount > 1.0e-4 {
                    touched = degrees;
                }
                if amount >= 1.0 - 1.0e-4 {
                    full = degrees;
                }
            }
            (touched, full)
        }

        let (arch_touched, arch_full) = tilt_range(&arch());
        let (roof_touched, roof_full) = tilt_range(&heavy_roof());
        // The 90-degree statics: the ramp runs from about 87 degrees of tilt to about 57.
        assert!((86..=88).contains(&arch_touched), "{arch_touched}");
        assert!((56..=57).contains(&arch_full), "{arch_full}");
        // The heavy roof's 120-degree window widens it to about 108 and about 70, which is what
        // makes its walls snowier.
        assert!((107..=109).contains(&roof_touched), "{roof_touched}");
        assert!((69..=71).contains(&roof_full), "{roof_full}");
        // Past that the surface is bare again, and it is bare at the one tilt that matters most:
        // a ceiling's normal points straight along the fall.
        assert_close(coverage_for_normal(Vec3::NEG_Y, &heavy_roof()), 0.0);
    }

    #[test]
    fn the_max_angle_widens_the_cone_the_material_covers() {
        let roof = heavy_roof();
        let arch = arch();
        assert_ne!(roof.cos_max_angle, arch.cos_max_angle);
        // 120 degrees must cover more of the same surface than 90 - the heavy roof is the 120 one,
        // so a reading that made the bigger angle cover less would be backwards.
        let mut covered_more = false;
        for degrees in 0..89 {
            let radians = (degrees as f32).to_radians();
            let normal = Vec3::new(radians.sin(), radians.cos(), 0.0);
            let heavy = coverage_for_normal(normal, &roof);
            let plain = coverage_for_normal(normal, &arch);
            assert!(
                heavy >= plain - 1e-6,
                "{degrees} degrees: {heavy} < {plain}"
            );
            assert!((0.0..=1.0).contains(&heavy));
            covered_more |= heavy > plain;
        }
        assert!(
            covered_more,
            "the angle changed nothing, so it is not part of the formula"
        );
        // At 90 the window is the upper hemisphere, so a surface facing exactly along the horizon
        // sits on its edge.
        assert_close(coverage_for_normal(Vec3::X, &arch), 0.0);
        assert!(coverage_for_normal(Vec3::Y, &arch) > 0.0);
    }

    #[test]
    fn a_closed_or_inverted_window_covers_nothing() {
        let roof = heavy_roof();
        let mut closed = roof;
        closed.cos_max_angle = 1.0;
        assert_close(coverage_for_normal(Vec3::Y, &closed), 0.0);
        // A window wider than the sphere is the whole sphere; it cannot flip the sign.
        let mut wide = roof;
        wide.cos_max_angle = -1.0;
        assert_close(coverage_for_normal(Vec3::Y, &wide), 1.0);
        assert!(coverage_for_normal(Vec3::NEG_Y, &wide) >= 0.0);

        // A record with no ramp is a step, not a division by zero.
        let mut step = roof;
        step.falloff_scale = 0.0;
        step.falloff_bias = 0.5;
        assert_close(coverage_for_normal(Vec3::Y, &step), 1.0);
        assert_close(coverage_for_normal(Vec3::NEG_Y, &step), 0.0);
    }

    #[test]
    fn the_dampener_pulls_the_normal_towards_the_fall_axis() {
        // At 0 the material is the plain cosine ramp; at 1 a normal that points straight down the
        // fall axis cancels and the axis itself is used, so nothing becomes a NaN.
        let up = Vec3::Y;
        assert_eq!(dampened_normal(Vec3::X, up, 0.0), Vec3::X);
        assert_eq!(dampened_normal(Vec3::NEG_Y, up, 1.0), up);
        assert_eq!(dampened_normal(Vec3::ZERO, up, 0.4), up);
        let damped = dampened_normal(Vec3::X, up, 0.4);
        assert_close(damped.length(), 1.0);
        assert!(damped.y > 0.0, "the dampened normal leans towards up");

        let roof = heavy_roof();
        let mut undamped = roof;
        undamped.normal_dampener = 0.0;
        // The dampener only ever adds snow: a surface within the window keeps at least what the
        // bare cosine gave it.
        for degrees in [10.0f32, 30.0, 50.0, 70.0] {
            let radians = degrees.to_radians();
            let normal = Vec3::new(radians.sin(), radians.cos(), 0.0);
            assert!(
                coverage_for_normal(normal, &roof) >= coverage_for_normal(normal, &undamped) - 1e-6
            );
        }
    }

    #[test]
    fn a_missing_normal_never_produces_a_nan() {
        // A degenerate triangle can hand the shader a zero or non-finite normal. The coverage has
        // to stay a number the colour mix can use: a NaN base colour is a whole surface of
        // undefined colour, not one triangle too white. The dampened normal falls back to the
        // fall axis, which `snow.wgsl` does on the same branch.
        let roof = heavy_roof();
        for normal in [
            Vec3::ZERO,
            Vec3::splat(f32::NAN),
            Vec3::splat(f32::INFINITY),
            Vec3::NEG_Y,
        ] {
            let coverage = coverage_for_normal(normal, &roof);
            assert!(coverage.is_finite(), "{normal:?} gave {coverage}");
            assert!(
                (0.0..=1.0).contains(&coverage),
                "{normal:?} gave {coverage}"
            );
        }
        // The three that cannot be normalised fall back to the fall axis; a normal pointing down
        // the axis the snow falls along is still a direction, and stays one.
        for normal in [
            Vec3::ZERO,
            Vec3::splat(f32::NAN),
            Vec3::splat(f32::INFINITY),
        ] {
            assert_eq!(
                dampened_normal(normal, roof.up, roof.normal_dampener),
                roof.up
            );
        }
        assert_eq!(
            dampened_normal(Vec3::NEG_Y, roof.up, roof.normal_dampener),
            Vec3::NEG_Y
        );
    }

    #[test]
    fn the_extension_carries_what_the_shader_reads() {
        let roof = heavy_roof();
        let extension = SnowExtension::new(&roof);
        let axis = extension.axis_and_cos_max();
        assert_eq!(axis.x, 0.0);
        assert_eq!(axis.y, 1.0);
        assert_eq!(axis.z, 0.0);
        assert_eq!(axis.w, roof.cos_max_angle);
        let falloff = extension.falloff_and_dampener();
        assert_eq!(falloff.x, 0.35);
        assert_eq!(falloff.y, 0.4);
        assert_eq!(falloff.z, 0.4);
        assert_eq!(falloff.w, 1.0, "the record's single-pass flag travels");

        // The colour the record stores as bytes reaches the uniform in linear space, and the
        // single-pass colour of this material is not white: a shader handed the wrong channels
        // would paint every roof grey.
        let expected = Color::srgb_u8(107, 116, 126).to_linear();
        let material = snow_material(StandardMaterial::default(), &roof);
        assert_eq!(material.extension.axis_and_cos_max(), axis);
        assert!(material.extension.settings.color.x > expected.red - 1e-6);
        assert!((material.extension.settings.color.x - expected.red).abs() < 1e-6);

        // The default extension draws nothing: it is the closed window.
        let default = SnowExtension::default();
        assert_eq!(default.axis_and_cos_max().w, 1.0);
        let mut default_coverage = roof;
        default_coverage.cos_max_angle = default.axis_and_cos_max().w;
        assert_close(coverage_for_normal(Vec3::Y, &default_coverage), 0.0);
    }
}
