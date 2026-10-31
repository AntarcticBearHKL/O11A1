# FidelityFX Super Resolution 1 — license

Source: `ffx_fsr1.h` (v1.20210629) from
<https://github.com/GPUOpen-Effects/FidelityFX-FSR>, fetched unmodified into this
directory as `ffx_fsr1.h`. The repository ships no separate LICENSE file, so the
license text below is the header of that file verbatim.

Only `FsrRcasCon` / `FsrRcasF` (RCAS) are ported, into
`src/dcomp/shaders/enhance.hlsl`. EASU is not ported: at a 1:1 input/output
ratio — the only ratio this player produces — EASU is the identity, and the
reference itself applies RCAS to a pass-through there.

---

FidelityFX Super Resolution Sample

Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and / or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
