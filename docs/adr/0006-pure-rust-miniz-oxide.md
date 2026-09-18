# ADR-0006: `dummy-content` compresses with pure-Rust `miniz_oxide`

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

In a unified build (`cargo test -p converter`), `project-wormhole-*` enables
`flate2/cloudflare_zlib`, so `flate2` uses its C backend. Compressing fixture-sized payloads
through that backend segfaults in `deflate_slow`/`longest_match` on Linux when generated BSA/BA2
data is built from a converter integration test (confirmed with gdb). The `dummy-content` unit
tests passed only because that build resolved `flate2` to its pure-Rust backend.

## Decision

`dummy-content` uses `miniz_oxide::deflate::compress_to_vec_zlib` and
`miniz_oxide::inflate::decompress_to_vec_zlib` directly and drops its `flate2` dependency. The
converter keeps `flate2` for parsing and for its own tests.

## Consequences

- Fixture generation no longer depends on whichever zlib backend feature unification selects.
- Output stays deterministic for a given `miniz_oxide` version.
- The latent C-zlib crash is still reachable by converter test code that compresses larger
  payloads and should be raised with the maintainers.
