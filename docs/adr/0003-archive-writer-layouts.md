# ADR-0003: Archive writers emit converter-accepted layouts

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

The BSA and BA2 writers must produce bytes that the converter's parsers accept so fixtures can
exercise real decode paths. Real Skyrim SE archives use BSA v105 with 24-byte folder records,
while the converter also supports legacy v104 archives. The converter resolves entries by table
order and ignores the stored name hashes.

## Decision

- BSA: `v105` by default (24-byte folder records), `v104` optional. Compression `None`, `Zlib` or
  `Lz4`; `Lz4` is rejected for `v104` because the parser only decodes zlib there. LZ4 payloads are
  raw blocks, matching the converter's decode order (LZ4 frame → LZ4 block → zlib).
- BA2: version 1 `GNRL` (`None`/`Zlib`) and version 1 `DX10` with one chunk per texture.
- Name hashes are written as zero.

## Consequences

- Fixtures cover every decoder branch (uncompressed, zlib, LZ4; general and DX10 records).
- Generated archives are not usable by game clients or third-party BSA browsers because name
  hashes are absent.
- If hash-based lookup becomes necessary, this ADR must be revisited.
