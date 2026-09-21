# Synthetic Fixtures (`dummy-content`)

`dummy-content` generates deterministic, procedurally built Skyrim-format files so contributors
without a local game installation can develop, test, and demo the converter without touching
copyrighted assets. No game data is read, copied, or required.

Tracks [`issue #2`](https://github.com/realfakenerd/OpenSkyrim/issues/2).

## Quick start

```bash
# Generate a synthetic Data directory
cargo run -p dummy-content -- gen Data

# Convert it with the real pipeline
cargo run -p converter --bin converter -- Data modern_assets
```

The second command produces the same layout the launcher/engine expect from a full conversion:
`modern_assets/` with KTX2 textures, Luau scripts, `vfs/`, the ingestion cache and
`conversion-manifest.json` (`complete: true`).

## CLI

```text
dummy-content gen <output-dir> [--seed <n>] [--formats <list>] [--force]
```

- `--seed <n>` — seed for all generated texture content (SplitMix64). The default is stable.
- `--formats dds,pex,bsa,ba2` — restrict output. The default generates everything.
- `--force` — allow writing into a non-empty directory. Existing generated files are replaced
  atomically; unrelated files are left untouched. Generation refuses to follow symlinked path
  components.

## Generated tree

| Path | Content |
| :--- | :--- |
| `scripts/generated.pex`, `scripts/second.pex` | Minimal Papyrus `3.2` scripts (magic `0xFA57C0DE`, big-endian header). |
| `textures/generated_color.dds` | BC1 color texture, 64×64, 7 mips. |
| `textures/generated_normal.dds` | BC5 normal texture, 64×64, 7 mips. |
| `textures/generated_color_x8.dds` | Uncompressed `X8R8G8B8` texture, 32×32, 6 mips. |
| `textures/generated_cube.dds` | BC1 cube map, 32×32, 6 mips, six faces. |
| `textures/generated_volume.dds` | BC1 volume texture, 16×16×16, 5 mips. |
| `Skyrim - Misc.bsa` | SSE `v105` BSA (24-byte folder records) with zlib payloads. |
| `Skyrim - Textures.ba2` | Version 1 `GNRL` BA2 with zlib payloads. |

## Library API

The crate can be consumed directly (it is used by converter tests as a dev-dependency):

```rust
use dummy_content::{Entry, bsa};

let archive = bsa::v105(
    &[Entry::new("scripts/hello.pex", b"PEX")],
    bsa::Compression::Zlib,
)?;
# Ok::<(), color_eyre::Report>(())
```

Supported writers:

- `dds`: `X8R8G8B8`, BC1, BC5, BC7; mip chains, cube maps and volume textures with strict
  validation (mip bounds, block alignment, non-zero dimensions).
- `pex`: minimal Skyrim `3.2` script; validates the object name.
- `bsa`: `v105` (default) and `v104`, compression `None`, `Zlib` or `Lz4` (`Lz4` is rejected for
  `v104`). Entries are grouped by folder in first-seen order.
- `ba2`: version 1 `GNRL` (`None`/`Zlib`) and version 1 `DX10` (one chunk per texture).
- `layout`: the `Data/` tree above, with atomic publication and symlink refusal.

Output is byte-for-byte deterministic per seed, which makes fixtures safe to use in golden tests.

## Intentional deviations

Some writers emit layouts that the current converter accepts rather than what a retail game
client would consume; the [ADRs](../../adr/README.md) record the reasoning:

- BSA/BA2 name hashes are zero: the converter resolves entries by table order.
- Cube maps use the legacy `caps2` six-layer layout because the converter rejects spec-standard
  DX10 cube maps.
- `X8R8G8B8` fixtures are 2D only; cube/volume fixtures use block-compressed formats.
- ESM/NIF writers are planned for the second PR; until then, fixtures cover scripts, textures
  and archives.

## Validating with a local game install

Real-asset checks stay opt-in and never run in CI with proprietary data. Point them at an
**unmodded** `Data` directory (for example the Steam install):

```bash
export OPENSKYRIM_SKYRIM_DATA="$HOME/.local/share/Steam/steamapps/common/Skyrim Special Edition/Data"
export OPENSKYRIM_NIF_FIXTURE="/path/to/a/static.nif"
cargo test -p converter -- --ignored
```

Mod-manager "Stock Game" directories are not suitable: their loose files are often modified.

## Development

```bash
cargo test -p dummy-content                                        # unit + integration tests
cargo clippy -p dummy-content --all-targets --all-features -- -D warnings
cargo bench -p dummy-content                                       # criterion benches
cargo test --release -p dummy-content -- --ignored performance     # release budgets
```

Security sweeps (truncation and mutation) for generated archives, DDS files and PEX scripts live
in the converter unit tests; the generated pipeline is covered end to end by
`crates/converter/tests/fixture_round_trip.rs`.
