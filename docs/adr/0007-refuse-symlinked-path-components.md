# ADR-0007: Fixture writes refuse symlinked path components

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

`dummy-content gen --force` may run inside contributor-controlled or CI-produced directories. A
planted symlink (for example `Data/scripts -> /somewhere/else`) would previously let generation
write outside the requested output root.

## Decision

Before writing any file, `ensure_no_symlink_components` inspects every existing component below
the output root and refuses to proceed through a symlinked component. File publication stays
atomic (temporary file, backup of any previous file, then rename). The output root itself may be a
symlink because that is the caller's explicit choice.

## Consequences

- Generation cannot escape the target directory through planted links.
- Covered by a `cfg(unix)` test that plants a symlink and asserts nothing is written outside.
- Behavior is documented in the CLI help and fixture guide.
