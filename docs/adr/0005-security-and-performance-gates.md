# ADR-0005: Security and performance gates are part of the deliverable

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

Generated fixtures are inputs to parsers that must never panic, hang, or overflow on malformed
data. The project also wants performance regressions caught in CI without introducing flaky
timing assertions.

## Decision

- Security: deterministic truncation sweeps (every prefix) and bounded mutation sweeps for every
  generated format, executed in the normal test suite under `catch_unwind`; writer input
  validation tests for dimensions, mip counts, size limits and unsafe names. No `cargo-fuzz`.
- Supply chain: `cargo audit` runs as a CI gate with documented ignores only. `cargo-deny` is not
  used because the vendored GPL-3.0 NIF parser needs a maintainer license decision first.
- Performance: criterion benches (non-gating) plus `#[ignore = "performance"]` release budget
  tests with 5–10× headroom, median of three runs, `--test-threads=1`.

## Consequences

- Parser panics and new vulnerabilities fail CI immediately.
- Budget tests stay stable on shared runners while still catching order-of-magnitude regressions.
- Fuzzing coverage is bounded by the deterministic sweeps; a future move to `cargo-fuzz` is
  possible without changing the gates.
