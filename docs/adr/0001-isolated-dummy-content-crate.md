# ADR-0001: Isolated `dummy-content` crate

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

Issue #2 asks for example files or a generator so contributors without a local copy of Skyrim can
develop and test the converter. The maintainers preferred a CLI built as a dedicated
`dummy-content` crate so that no proprietary data is involved.

## Decision

Add a new workspace crate `crates/dummy-content` with a library plus a `dummy-content` binary. The
crate has no dependency on any other workspace crate; `converter` consumes it as a
`dev-dependency` for fixture-driven tests. Writers only produce bytes; every byte is synthesized
from a caller-provided seed. Validation happens in converter tests through the real parsers
(`ArchiveExtractor`, `TextureConverter`, `ScriptConverter`, and later `EsmParser`/`MeshConverter`).

## Consequences

- No dependency cycles; any crate (including `engine`) can reuse the fixtures.
- Fixture writer defects surface in cross-crate tests, such as
  `crates/converter/tests/fixture_round_trip.rs`.
- The crate must stay workspace-independent to remain generally usable.
