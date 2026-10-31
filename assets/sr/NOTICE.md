# 画质增强 (f202) — third-party notices

The 画质增强 tiers run third-party image filters inside the player's DComp video
sink (`src/dcomp/`). Nothing here is shipped as a DLL; the shaders are compiled
from source at runtime by the same `d3dcompiler_47.dll` the player already uses.

| tier | upstream | license | what we use |
|------|----------|---------|-------------|
| Anime4K | <https://github.com/bloc97/Anime4K> v4.0.1, Copyright (c) 2019-2021 bloc97 | MIT (`anime4k/LICENSE`) | `Anime4K_Clamp_Highlights.glsl` and `Anime4K_Restore_CNN_M.glsl` (and `_S`, kept as the fallback), mechanically translated to HLSL by `scripts/qa-sr-anime4k-gen.py` into `src/dcomp/shaders/anime4k_restore_{m,s}.hlsl` |
| FSR | FidelityFX-FSR `ffx_fsr1.h` v1.20210629, Copyright (c) 2021 Advanced Micro Devices, Inc. | MIT (`fsr/LICENSE.md`) | `FsrRcasF` / `FsrRcasCon` (RCAS), hand-ported into `src/dcomp/shaders/enhance.hlsl` |
| 锐化 | AMD FidelityFX CAS, described in the same header | MIT | re-implemented from the published 5-tap cross + soft clamp (no code copied) |

RTX VSR was evaluated and then **removed** from the product. It never shipped a
working tier: the driver's NGX core initialises but reports
`ImageSuperResolution.Available = 0` and `VideoSuperResolution.Available = 0`,
every NGX feature including the DLSS baseline fails `CreateFeature` with
`0xBAD0000B`, and `nvngx_dlisr.dll` (NVIDIA app) turned out to be a CUDA-only
feature module whose D3D11 entry is a six-byte stub returning a constant. No
NVIDIA binary, header or source is in this repository; the measurement evidence
is `docs/f206_ngx_features.log`.

The upstream `.glsl` files are kept byte-identical to the v4.0.1 release so the
generator can be re-run and diffed. `Anime4K_Upscale_CNN_x2_M.glsl` and
`Anime4K_AutoDownscalePre_x2.glsl` are included for provenance only: that
shader's own `//!WHEN OUTPUT.w MAIN.w / 1.200 >` gate disables it below a 1.2x
ratio, and this player's enhancement always runs at the video layer's own size
(ratio 1.0). They are not compiled into the binary.
