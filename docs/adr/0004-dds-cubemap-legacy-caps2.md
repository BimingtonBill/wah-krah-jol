# ADR-0004: DDS cubemaps use the legacy `caps2` layout

- **Status:** Accepted
- **Date:** 2026-09-18

## Context

The converter detects cube maps either through `caps2.contains(Caps2::CUBEMAP)` or through the
DX10 `misc_flag` `TEXTURECUBE`, and then requires `get_num_array_layers() == 6`. A spec-standard
DX10 cube map stores `array_size = 1` (one cube) with the cubemap bit set, so the converter
rejects it as having the wrong number of faces. Retail SSE cube maps use the legacy D3D9-style
header, which the converter handles.

## Decision

Generate cube maps as DX10 textures with `caps2 = CUBEMAP | CUBEMAP_ALLFACES`, six array layers,
and `is_cubemap = false` in the ddsfile allocation call. `X8R8G8B8` fixtures support neither cube
maps nor volume textures; block-compressed formats are required for both.

## Consequences

- Cube-map and volume fixtures convert cleanly through the current converter.
- The header is a deliberate hybrid of DX10 fields and legacy `caps2` bits; if the converter
  adopts spec-standard cube-map counting, this ADR must be revisited.
