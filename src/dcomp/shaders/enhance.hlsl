// f202: the hand-written enhancement passes (CAS, FSR1 RCAS, Anime4K
// Clamp_Highlights). The Anime4K CNN passes are generated - see
// `anime4k_restore_m.hlsl` and `scripts/qa-sr-anime4k-gen.py`.
//
// One cbuffer for every entry point: `dims.xy` is the inclusive maximum pixel
// index, `dims.zw` the size in pixels, `extra.x` / `extra.y` the per-mode
// constants (CAS strength, RCAS `con`).
//
// `L` is a clamped point load, the same tap mpv's `_texOff` performs for the
// Anime4K shaders: enhancement runs at the layer's own resolution, so there is
// no filtering to do here - the compositor scales the result afterwards.

cbuffer Dims : register(b0)
{
    float4 dims;
    float4 extra;
};

Texture2D<float4> tex_src : register(t0);
Texture2D<float4> tex_aux : register(t1);

float4 L(Texture2D<float4> tex, int2 p)
{
    return tex.Load(int3(clamp(p, int2(0, 0), int2((int)dims.x, (int)dims.y)), 0));
}

float luma(float3 rgb)
{
    return dot(rgb, float3(0.299, 0.587, 0.114));
}

// ---------------------------------------------------------------- copy
// The 关闭 chain never builds this; it exists for `BILI_SR_TRACE` (a pass that
// only moves pixels proves the RT -> backbuffer path without a filter).
float4 copy_main(float4 frag : SV_Position) : SV_Target
{
    return float4(L(tex_src, int2(frag.xy)).rgb, 1.0);
}

// ------------------------------------------------------- AMD CAS (锐化)
// 5-tap cross plus the soft clamp: the negative lobe is limited to what cannot
// drive a tap past [0, 1], then scaled by the strength. `extra.x` is the
// strength (0..1, this build uses 0.6).
float4 cas_main(float4 frag : SV_Position) : SV_Target
{
    int2 ip = int2(frag.xy);
    float3 m = L(tex_src, ip).rgb;
    float3 n = L(tex_src, ip + int2(0, -1)).rgb;
    float3 s = L(tex_src, ip + int2(0, 1)).rgb;
    float3 w = L(tex_src, ip + int2(-1, 0)).rgb;
    float3 e = L(tex_src, ip + int2(1, 0)).rgb;

    // AMD's peak: 8 - 3 * sharpness, sharpness in 0..1 -> lobe -1/8 .. -1/5.
    float peak = 8.0 - 3.0 * saturate(extra.x);
    float lobe = -1.0 / peak;

    float3 mn = min(min(min(n, s), min(w, e)), m);
    float3 mx = max(max(max(n, s), max(w, e)), m);
    // Soft clamp: how much headroom is left before a tap clips.
    float3 amp = saturate(min(mn, 2.0 - mx) / max(mx, 1.0 / 255.0));
    amp = sqrt(amp);
    float3 weight = amp * lobe;
    float3 rgb = (weight * (n + s + w + e) + m) / (1.0 + 4.0 * weight);
    return float4(saturate(rgb), 1.0);
}

// ------------------------------------------------ FSR1 RCAS (FSR 档)
// Port of `FsrRcasF` from GPUOpen FidelityFX-FSR `ffx_fsr1.h` (MIT, see
// assets/fsr/LICENSE.md): a 5-tap cross, solved so the sharpen cannot clip, with
// the local-contrast noise gate. `extra.y` is `FsrRcasCon`'s `con.x`
// (`exp2(-sharpness)`), computed on the CPU.
#define FSR_RCAS_LIMIT (0.25 - (1.0 / 16.0))

float rcas_luma(float3 rgb)
{
    // Luma times two, exactly as the reference builds it.
    return rgb.b * 0.5 + (rgb.r * 0.5 + rgb.g);
}

float4 rcas_main(float4 frag : SV_Position) : SV_Target
{
    int2 ip = int2(frag.xy);
    float3 b = L(tex_src, ip + int2(0, -1)).rgb;
    float3 d = L(tex_src, ip + int2(-1, 0)).rgb;
    float3 e = L(tex_src, ip).rgb;
    float3 f = L(tex_src, ip + int2(1, 0)).rgb;
    float3 h = L(tex_src, ip + int2(0, 1)).rgb;

    float bL = rcas_luma(b);
    float dL = rcas_luma(d);
    float eL = rcas_luma(e);
    float fL = rcas_luma(f);
    float hL = rcas_luma(h);

    // Noise detection: a normalised high pass. The reference uses the medium
    // precision reciprocal; the exact one only differs in the last bit.
    float nz = 0.25 * bL + 0.25 * dL + 0.25 * fL + 0.25 * hL - eL;
    float span = max(max(max(bL, dL), max(eL, fL)), hL) - min(min(min(bL, dL), min(eL, fL)), hL);
    nz = saturate(abs(nz) / max(span, 1.0 / 1024.0));
    nz = -0.5 * nz + 1.0;

    float mnR = min(min(b.r, d.r), min(f.r, h.r));
    float mnG = min(min(b.g, d.g), min(f.g, h.g));
    float mnB = min(min(b.b, d.b), min(f.b, h.b));
    float mxR = max(max(b.r, d.r), max(f.r, h.r));
    float mxG = max(max(b.g, d.g), max(f.g, h.g));
    float mxB = max(max(b.b, d.b), max(f.b, h.b));

    // Peak range constants: (1, -4).
    float hitMinR = min(mnR, e.r) / (4.0 * mxR + 1e-7);
    float hitMinG = min(mnG, e.g) / (4.0 * mxG + 1e-7);
    float hitMinB = min(mnB, e.b) / (4.0 * mxB + 1e-7);
    float hitMaxR = (1.0 - max(mxR, e.r)) / (4.0 * mnR - 4.0);
    float hitMaxG = (1.0 - max(mxG, e.g)) / (4.0 * mnG - 4.0);
    float hitMaxB = (1.0 - max(mxB, e.b)) / (4.0 * mnB - 4.0);

    float lobeR = max(-hitMinR, hitMaxR);
    float lobeG = max(-hitMinG, hitMaxG);
    float lobeB = max(-hitMinB, hitMaxB);
    float lobe = max(-FSR_RCAS_LIMIT, min(max(lobeR, max(lobeG, lobeB)), 0.0)) * extra.y;

    float rcpL = 1.0 / (4.0 * lobe + 1.0);
    return float4(
        saturate((lobe * (b.r + d.r + h.r + f.r) + e.r) * rcpL),
        saturate((lobe * (b.g + d.g + h.g + f.g) + e.g) * rcpL),
        saturate((lobe * (b.b + d.b + h.b + f.b) + e.b) * rcpL),
        1.0);
}

// ------------------------------------------- Anime4K Clamp_Highlights
// Three passes of the upstream shader (assets/anime4k/Anime4K_Clamp_Highlights
// .glsl): a 5x5 max of luma, split into a horizontal and a vertical sweep, then
// every pixel's luma is clamped to that local maximum - the de-ringing step that
// has to run before the CNN so it cannot amplify a compression halo.
#define KERNELSIZE 5
#define KERNELHALFSIZE 2

float4 clamp_stats_x(float4 frag : SV_Position) : SV_Target
{
    int2 ip = int2(frag.xy);
    float gmax = 0.0;
    for (int i = 0; i < KERNELSIZE; i++)
    {
        float g = luma(L(tex_src, ip + int2(i - KERNELHALFSIZE, 0)).rgb);
        gmax = max(g, gmax);
    }
    return float4(gmax, 0.0, 0.0, 1.0);
}

float4 clamp_stats_y(float4 frag : SV_Position) : SV_Target
{
    int2 ip = int2(frag.xy);
    float gmax = 0.0;
    for (int i = 0; i < KERNELSIZE; i++)
    {
        gmax = max(L(tex_aux, ip + int2(0, i - KERNELHALFSIZE)).x, gmax);
    }
    return float4(gmax, 0.0, 0.0, 1.0);
}

float4 clamp_apply(float4 frag : SV_Position) : SV_Target
{
    int2 ip = int2(frag.xy);
    // tex_src.x is unused here; the stats texture arrives on t1.
    float4 colour = L(tex_src, ip);
    float current = luma(colour.rgb);
    float ceiling = L(tex_aux, ip).x;
    float newLuma = min(current, ceiling);
    return float4(colour.rgb - (current - newLuma), 1.0);
}

// ------------------------------------------------------------- vertex
// One triangle covering the viewport; the same trick `warp.rs` uses (no vertex
// buffer, no input layout).
struct VSOut
{
    float4 pos : SV_Position;
};

VSOut vs_main(uint id : SV_VertexID)
{
    float2 uv = float2((id << 1) & 2, id & 2);
    VSOut o;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
