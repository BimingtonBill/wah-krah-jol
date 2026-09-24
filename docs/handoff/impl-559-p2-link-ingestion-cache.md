# impl-559: hard-link the reused ingestion cache and `vfs` in a reconversion

Branch base: `phase2/staging-hard-links` (`a3792e6`). Track: Phase 2 (for upstream).

## Problem

A reconversion reused every unchanged archive-extraction blob and `vfs` file by **copying** it
(`crates/converter/src/archive/mod.rs`, `restore_cached_files`). Staging therefore needed roughly
the full published size again (about 25 GB `vfs` + 22 GB cache for Skyrim); on 2026-09-24 that
filled the disk and crashed a reconversion.

## Approach

1. `link_or_copy` moved from `pipeline.rs` to `cache.rs` as `pub(crate)` (pipeline's three callers
   now import it; behaviour unchanged: remove an existing destination, hard link, fall back to a
   copy where the filesystem refuses).
2. `archive/mod.rs` `copy_file` — the one primitive behind `copy_if_missing`,
   `restore_cached_files` and `persist_cache_blobs` — now calls `link_or_copy` instead of
   `fs::copy`. Every size and SHA-256 validation in `restore_cached_files` is untouched, so a
   corrupt or truncated blob is still a cache miss and still re-extracts.
3. Every writer into `vfs` now removes its destination first (see the table below), so no write can
   travel through a shared inode into the cache blob and the previous output.

Blobs are content-addressed (`blob_path` = `cache_root/sha256/<hash[..2]>/<hash>`), so a blob path
never holds two different contents and nothing needs to rewrite one in place.

## Writers into `vfs` and `.ingestion-cache`

| Writer | Where | How it avoids writing through a link |
| --- | --- | --- |
| `atomic_write` (archive extraction) | `archive/mod.rs:256` | Already removed the destination before renaming its `.partial` over it (`archive/mod.rs:267`); a re-extraction unlinks the old `vfs` entry rather than truncating it. Unchanged. |
| `copy_file` / `copy_if_missing` (cache restore: blob→blob, blob→`vfs`) | `archive/mod.rs:228-253` | Now `link_or_copy`, which unlinks an existing destination first. |
| `persist_cache_blobs` (`vfs`→blob) | `archive/mod.rs:207` | Goes through `copy_if_missing`; both names are links to one freshly written `atomic_write` file, which no later step writes in place. |
| `overlay_loose_assets` (loose `Data` files over `vfs`) | `pipeline.rs:963` | **Changed**: unlinks the destination before `fs::copy`. This was the real hazard — `fs::copy` truncates, and a reconverted entry is a link to the previous output's blob. |
| `publish_srgb_texture_aliases` | `pipeline.rs:864-897` | Already removes an existing destination, then `link_or_copy`. Unchanged. |
| `convert_kind` outputs (`.ktx2`/`.glb`/`.luau`) | `pipeline.rs:547, 582` | Reuse links; a re-converted output unlinks the staged file at its path before the converter writes. Both from `a3792e6`; unchanged. |

Checked and read-only (no change needed): `integration.rs:207` `source_file_index` only walks
`staging/vfs` to build a path index; `convert_kind` reads `vfs` sources and writes into
`staging/{textures,meshes,scripts}`; `finalize_world_database`, `write_cell_cache` and the
`papyrus_runtime.luau` write at `pipeline.rs:357` all target the staging root, never `vfs`.

Lifetime note: publish renames staging over the output and deletes the preserved previous output
(`publish_directory`), which drops one link to each inode; the surviving names keep the bytes.

## Tests

New (both fail on the pre-change code — verified by reverting `copy_file` to `fs::copy` and the
overlay to a plain `fs::copy`, then re-running `cargo test -p converter link`: 2 failed, 2 passed):

- `archive::tests::cache_hits_link_blobs_and_vfs_files_where_the_filesystem_can` — a second
  `extract_cached` with the previous entry as the reuse key links the new blob to the old blob and
  the new `vfs` file to that blob. Appending through the new `vfs` name is visible in the old
  blob, the new blob and the old `vfs` file, which only a shared file can be (same check as
  `link_or_copy_shares_the_file_where_the_filesystem_can`).
- `pipeline::tests::loose_asset_override_does_not_write_through_a_linked_vfs_entry` — with the
  previous output's `vfs` file, its content-addressed blob and the staged entry all one inode, a
  loose `Data/textures/rock.dds` override leaves the staged entry holding the loose bytes while the
  previous output's `vfs` file and blob still hold the archive bytes.

Unchanged and still passing: `archive::tests::reuses_verified_archive_blobs_and_recovers_from_corruption`
(a blob overwritten with different bytes is still detected by the size/SHA-256 check and
re-extracted), plus the pipeline's two-run
`reuses_and_invalidates_archive_ingestion_cache_end_to_end`.

## Gates

`cargo fmt --all -- --check`, `cargo clippy -p converter --all-targets -- -D warnings` and
`cargo test -p converter` (105 passed, 0 failed, 7 ignored) all pass.

Not done here: a real reconversion of `C:\Modding\SkyrimConverted` to confirm the disk saving
(~47 GB) and that the published tree is byte-identical; that needs the full pipeline run, which is
outside this task's checks.
