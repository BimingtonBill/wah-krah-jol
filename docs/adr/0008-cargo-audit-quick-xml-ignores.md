# ADR-0008: `cargo audit` ignores two build-time-only `quick-xml` advisories

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

`Cargo.lock` resolves `quick-xml` 0.39.4, which is affected by RUSTSEC-2026-0194 and
RUSTSEC-2026-0195 (both high severity). The crate is reachable solely through `wayland-scanner`,
a proc-macro that generates Wayland bindings at build time inside `winit`/`bevy`; it is not part
of shipped runtime code. `cargo update -p quick-xml --precise 0.41.0` is blocked by
`wayland-scanner`'s `^0.39` requirement.

## Decision

The CI `security` job runs `cargo audit` with documented `--ignore` entries for exactly those two
advisory IDs. Unmaintained, unsound and yanked warnings are printed but do not fail the gate.
Revisit when `wayland-scanner` permits `quick-xml >= 0.41`.

## Consequences

- Any new vulnerability fails CI immediately.
- The ignored pair is build-time codegen only and is reviewed whenever the dependency graph
  changes.
- Unknown advisories are never ignored silently: each ignore must be documented here first.
