# OpenSkyrim SQLite 3 Database Schema (`skyrim_world.db`)

This specification details the canonical DDL schema, tables, indices, and column constraints for `skyrim_world.db`, as implemented in [`crates/converter/src/esm/exporter.rs`](file:///C:/Users/lucas.augusto/Documents/programs/OpenSkyrim/crates/converter/src/esm/exporter.rs).

---

## 1. Schema Overview

`skyrim_world.db` is built by `crates/converter` by parsing master files (`Skyrim.esm`) and plugin files (`.esp`/`.esl`) in priority load order defined by `plugins.txt`.

`schema_info.version` is `shared::WORLD_DATABASE_SCHEMA_VERSION` (currently 4). The runtime refuses to open a database with any other version, and the asset conversion rewrites the database from the plugins, so a version bump invalidates previously converted asset sets.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      `skyrim_world.db` Implemented Schema                   │
│  ┌──────────────────────────┬──────────────────────┬─────────────────────┐  │
│  │   `plugins`              │   `records`          │ `worldspaces`       │  │
│  │   (Active Plugin Order)  │   (Raw FormID Data)  │ (Worldspace EDIDs)  │  │
│  ├──────────────────────────┼──────────────────────┼─────────────────────┤  │
│  │   `cells`                │   `references`       │ `refs_rtree`        │  │
│  │   (Cell Grid & Names)    │   (3D World Placements)│ (3D Spatial R-Tree) │  │
│  ├──────────────────────────┼──────────────────────┼─────────────────────┤  │
│  │   `land`                 │   `lod_grid` …       │ `scripts`           │  │
│  │   (Terrain Heightmaps)   │   (Distant LOD)      │ (Papyrus Bytecode)  │  │
│  ├──────────────────────────┴──────────────────────┴─────────────────────┤  │
│  │   `formid_map` & `conversion_cache`                                   │  │
│  │   (32-bit to 64-bit ID Bridge & Cache Hashes)                        │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Table Definitions

### 1. Active Plugin Registry (`plugins`)

Stores loaded `.esm`/`.esp`/`.esl` plugin file metadata, load order priority, and checksums.

```sql
CREATE TABLE IF NOT EXISTS plugins (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    priority INTEGER NOT NULL,
    checksum BLOB NOT NULL
);
```

---

### 2. Primary Record Database (`records`)

Stores unparsed raw subrecord byte payloads indexed by 32-bit Skyrim `FormID` and 4-character record type codes.

```sql
CREATE TABLE IF NOT EXISTS records (
    id INTEGER PRIMARY KEY,
    form_id INTEGER NOT NULL,
    record_type TEXT NOT NULL,          -- 'CELL', 'REFR', 'NPC_', 'WEAP', 'ARMOR', 'SPEL', etc.
    data BLOB NOT NULL                  -- Serialized subrecords payload
);

CREATE INDEX IF NOT EXISTS idx_records_formid ON records(form_id);
CREATE INDEX IF NOT EXISTS idx_records_type ON records(record_type);
```

---

### 3. Worldspace Registry (`worldspaces`)

Stores worldspace hierarchy and parent world relations (e.g. Tamriel `0x0000003C`, Solstheim).

```sql
CREATE TABLE IF NOT EXISTS worldspaces (
    id INTEGER PRIMARY KEY,             -- WorldSpace FormID
    editor_id TEXT NOT NULL,            -- EDID string (e.g. 'Tamriel')
    parent_world INTEGER,               -- Parent WorldSpace FormID (if child worldspace)
    flags INTEGER NOT NULL
);
```

---

### 4. Cell Registry (`cells`)

Stores exterior cell grid coordinates and interior cell names.

```sql
CREATE TABLE IF NOT EXISTS cells (
    id INTEGER PRIMARY KEY,             -- CELL FormID
    worldspace_id INTEGER NOT NULL,     -- Parent WorldSpace FormID
    grid_x INTEGER,                     -- Exterior cell Grid X (NULL if interior)
    grid_y INTEGER,                     -- Exterior cell Grid Y (NULL if interior)
    interior_name TEXT,                 -- Interior cell name (NULL if exterior)
    flags INTEGER NOT NULL,
    data BLOB                           -- Optional cell binary payload
);
```

---

### 5. Placed World References (`references`)

Stores 3D positions, rotations, scales, and cell parentage for all placed world objects (`REFR`, `ACHR`, `ACRE`, `PGRE`, `PMIS`).

```sql
CREATE TABLE IF NOT EXISTS references (
    id INTEGER PRIMARY KEY,
    cell_id INTEGER NOT NULL,           -- Parent CELL FormID
    form_id INTEGER NOT NULL,           -- Base Object FormID
    pos_x REAL NOT NULL,                -- 3D X Coordinate
    pos_y REAL NOT NULL,                -- 3D Y Coordinate
    pos_z REAL NOT NULL,                -- 3D Z Coordinate
    rot_x REAL NOT NULL,                -- Rotation X (Radians)
    rot_y REAL NOT NULL,                -- Rotation Y (Radians)
    rot_z REAL NOT NULL,                -- Rotation Z (Radians)
    scale REAL NOT NULL DEFAULT 1.0,    -- Scale multiplier
    data BLOB                           -- Subrecords payload
);

-- Index for O(1) interior cell reference loading
CREATE INDEX IF NOT EXISTS idx_references_cell_id ON references(cell_id);
```

---

### 6. Hybrid Spatial Indexing (`refs_rtree` & Interior `cell_id` Index)

To prevent `float32` single-precision accuracy loss at large exterior coordinates (e.g. Tamriel bounds $\pm 200,000$) and avoid coordinate collisions between interior local origins $(0,0,0)$ and exterior global space, OpenSkyrim uses a **Two-Tier Hybrid Spatial Strategy**:

1. **Exterior Worldspace R-Tree (`refs_rtree`):** Coordinates inside the R-Tree virtual table are stored normalized relative to cell centers (values constrained between $-2048.0$ and $+2048.0$), keeping numbers small to guarantee high single-precision float accuracy.
2. **Interior Cell Direct Lookup (`idx_references_cell_id`):** Interior dungeons and houses do not use R-Trees. All interior references are loaded directly by `cell_id` for instant $O(1)$ lookup upon entering interior doors.

```sql
-- R-Tree virtual table for Exterior 3D bounding box spatial queries
CREATE VIRTUAL TABLE IF NOT EXISTS refs_rtree USING rtree(
    id,                                 -- Matches internal reference ID
    minX, maxX,                         -- Local cell X offset (-2048.0 to +2048.0)
    minY, maxY,                         -- Local cell Y offset (-2048.0 to +2048.0)
    minZ, maxZ,                         -- World Z Height units
    +cell_id,                           -- Exterior CELL FormID
    +worldspace_id                      -- Parent WorldSpace FormID (e.g. 0x0000003C for Tamriel)
);
```

---

### 7. Terrain Heightmaps (`land`)

Stores 33x33 terrain heightmap data, vertex textures (`vtex`), and vertex colors (`vclr`) extracted from `LAND` records.

```sql
CREATE TABLE IF NOT EXISTS land (
    cell_id INTEGER PRIMARY KEY,        -- Parent CELL FormID
    heightmap BLOB NOT NULL,            -- 33x33 float/byte heightmap buffer
    vtext BLOB,                         -- Land texture layers
    vclr BLOB                           -- Land vertex colors
);
```

---

### 8. Distant Level of Detail (`lod_grid`, `lod_block`, `lod_tree_type`, `lod_tree_instance`)

The distant-LOD inventory the runtime streams beyond the full-detail cells: the
per-worldspace grid header (`lodsettings/<worldspace>.lod`), the terrain and
object block meshes (`meshes/terrain/<worldspace>/[objects/]<worldspace>.<level>.<x>.<y>.btr|bto`,
converted to GLB with `meshes/**` paths), and the tree billboards
(`.lst` types plus `.btt` instances). Block availability comes from the files
that exist, never from the `.lod` extents. Written after the mesh stage of the
conversion (`crates/converter/src/lod.rs`) and validated by
`crates/converter/src/integration.rs`. This replaced the never-written `lod`
table in schema 4.

```sql
CREATE TABLE IF NOT EXISTS lod_grid (       -- from lodsettings/<ws>.lod
    worldspace_id INTEGER PRIMARY KEY,
    origin_x INTEGER NOT NULL, origin_y INTEGER NOT NULL,   -- grid south-west, in cells
    levels TEXT NOT NULL                    -- declared block levels, e.g. '4,8,16,32'
);
CREATE TABLE IF NOT EXISTS lod_block (      -- one converted GLB per block
    worldspace_id INTEGER NOT NULL,
    kind TEXT NOT NULL,                     -- 'terrain' | 'objects'
    level INTEGER NOT NULL,
    block_x INTEGER NOT NULL,               -- south-west cell X (a multiple of level)
    block_y INTEGER NOT NULL,
    mesh_path TEXT NOT NULL,                -- 'meshes/terrain/...' GLB
    bounds_min_x REAL, bounds_min_y REAL, bounds_min_z REAL,
    bounds_max_x REAL, bounds_max_y REAL, bounds_max_z REAL,
    PRIMARY KEY (worldspace_id, kind, level, block_x, block_y)
);
CREATE TABLE IF NOT EXISTS lod_tree_type (  -- one billboard mesh per .lst entry
    worldspace_id INTEGER NOT NULL,
    tree_index INTEGER NOT NULL,            -- .lst position, also the .btt group key
    mesh_path TEXT NOT NULL,
    size_x REAL NOT NULL, size_y REAL NOT NULL,                 -- Creation units at scale 1
    u0 REAL NOT NULL, v0 REAL NOT NULL, u1 REAL NOT NULL, v1 REAL NOT NULL,  -- atlas rect, v from the top
    PRIMARY KEY (worldspace_id, tree_index)
);
CREATE TABLE IF NOT EXISTS lod_tree_instance (  -- from <ws>.4.<x>.<y>.btt
    worldspace_id INTEGER NOT NULL,
    block_x INTEGER NOT NULL, block_y INTEGER NOT NULL,         -- level-4 block south-west cell
    tree_index INTEGER NOT NULL,            -- lod_tree_type.tree_index
    pos_x REAL NOT NULL, pos_y REAL NOT NULL, pos_z REAL NOT NULL,  -- z is the tree's base
    rotation REAL NOT NULL DEFAULT 0,       -- yaw in radians
    scale REAL NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_lod_tree_block ON lod_tree_instance(worldspace_id, block_x, block_y);
```

A block whose converted mesh carries no bounds is still recorded, with NULL
bounds, and counted the way statics with unbounded models are; the runtime
treats NULL as an unbounded block. Tree LOD is only generated for levels the
files actually use (level 4), and a worldspace whose `.lst` has no entries
(`dlc01soulcairn`, `dlc2apocryphaworld`) has no types and no instances.

---

### 9. Compiled Scripts (`scripts`)

Stores compiled Papyrus script bytecode and property bindings.

```sql
CREATE TABLE IF NOT EXISTS scripts (
    form_id INTEGER PRIMARY KEY,        -- Script FormID
    script_name TEXT NOT NULL,          -- Script EDID name
    bytecode BLOB NOT NULL,             -- Papyrus PEX binary bytecode
    properties BLOB                     -- Script properties table
);
```

---

### 10. FormID Translation Map (`formid_map`)

Bridges 32-bit Skyrim FormIDs to 64-bit internal database row IDs across merged plugins.

```sql
CREATE TABLE IF NOT EXISTS formid_map (
    form_id INTEGER NOT NULL,           -- 32-bit Skyrim FormID
    plugin_name TEXT NOT NULL,          -- Plugin origin (e.g. 'merged')
    internal_id INTEGER NOT NULL,       -- Internal database row ID
    record_type TEXT NOT NULL,          -- Record type ('REFR', 'NPC_', etc.)
    PRIMARY KEY (form_id, plugin_name)
);
```

---

### 11. Asset Conversion Cache (`conversion_cache`)

Stores plugin file path hashes and timestamps to bypass re-converting unchanged files.

```sql
CREATE TABLE IF NOT EXISTS conversion_cache (
    plugin_path TEXT PRIMARY KEY,
    file_hash BLOB NOT NULL,
    last_converted INTEGER NOT NULL
);
```

---

### 12. Water Definitions (`waters`)

Stores one row per `WATR` record: Skyrim's per-water colours and reflectivity, decoded from the
record's `DNAM` subrecord (offsets in `docs/research/water.md` section 1.2), plus the raw
subrecords for anything not broken out into a column.

```sql
CREATE TABLE IF NOT EXISTS waters (
    id INTEGER PRIMARY KEY,             -- WATR FormID
    editor_id TEXT,
    opacity INTEGER,                    -- ANAM, 0-100
    flags INTEGER NOT NULL,             -- record header flags
    shallow_color INTEGER,              -- DNAM+40: packed 0x00BBGGRR
    deep_color INTEGER,                 -- DNAM+44: packed 0x00BBGGRR
    reflection_color INTEGER,           -- DNAM+48: packed 0x00BBGGRR
    fresnel REAL,                       -- DNAM+24: Fresnel Amount (Schlick F0)
    reflectivity REAL,                  -- DNAM+20: Reflectivity Amount
    flow_normal_path TEXT,              -- NAM5, canonicalised (SSE flowmap waters only)
    data BLOB NOT NULL                  -- Serialized subrecords payload
);
```

`shallow_color`/`deep_color`/`reflection_color`/`fresnel`/`reflectivity` are additive columns: a
`skyrim_world.db` built before they existed has a `waters` table without them, and
`AssetCatalog::water_colors` (`crates/engine/src/world/database.rs`) returns `None` for every
water against such a database rather than failing to open it. The engine falls back to Skyrim's
DefaultWater values (`render::DEFAULT_WATER_FRESNEL` / `render::DEFAULT_WATER_REFLECTIVITY`, and
`streaming.rs`'s deep-colour constant) until a reconversion populates them. Adding them did **not**
bump `shared::WORLD_DATABASE_SCHEMA_VERSION`: nothing that already reads `waters` depends on their
presence, and the fallback exists specifically so a reconversion is not required.
