// f202: the hand-written enhancement passes (CAS, Anime4K Clamp_Highlights).
// The Anime4K CNN passes are generated - see `anime4k_restore_m.hlsl` and
// `scripts/qa-sr-anime4k-gen.py`.
//
// f231: the third, FSR-derived pass is gone. At 1:1 it reduced to another
// 5-tap cross-and-clamp sharpener, i.e. the same job as 通用 (CAS) at a higher
// cost, so the product kept one sharpener and one CNN tier.
//
// One cbuffer for every entry point: `dims.xy` is the inclusive maximum pixel
// index, `dims.zw` the size in pixels, `extra.x` / `extra.y` the per-mode
// constants (CAS strength).
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

// ------------------------------------------------------------- brightness
// 0.4..2.0 multiplier on the picture only. `extra.x` is the multiplier; the UI
// and danmaku are separate composition layers and are never touched.
float4 brightness_main(float4 frag : SV_Position) : SV_Target
{
    float4 colour = L(tex_src, int2(frag.xy));
    return float4(saturate(colour.rgb * extra.x), 1.0);
}

// ------------------------------------------------------------ GPU 插帧
// Two decoded pictures -> one in-between picture. `extra.x` is the weight of
// `tex_aux` in 0..1. Both textures are the picture's own size, so a point load
// is exact and the whole 4K blend stays on the GPU (no BGRA readback).
float4 blend_main(float4 frag : SV_Position) : SV_Target
{
    int2 p = int2(frag.xy);
    float3 a = L(tex_src, p).rgb;
    float3 b = L(tex_aux, p).rgb;
    return float4(lerp(a, b, saturate(extra.x)), 1.0);
}

// ------------------------------------------------------ NV12 -> BGRA (GPU)
// The decoder's Y (R8) and interleaved UV (R8G8) planes become one BGRA
// picture. Coefficients are the same BT.709 limited-range integers the old CPU
// `convert.rs` used (298/403/48/120/475 over 256), and chroma is sampled with
// `p / 2` exactly like the CPU loop did. This replaces the per-frame 4K
// NV12→BGRA pass that limited playback to ~22 fps.
float4 nv12_main(float4 frag : SV_Position) : SV_Target
{
    int2 p = int2(frag.xy);
    float yy = tex_src.Load(int3(p, 0)).x * 255.0;
    float2 uv = tex_aux.Load(int3(p / 2, 0)).rg * 255.0;
    float luma = (yy - 16.0) * (298.0 / 256.0);
    float cb = uv.x - 128.0;
    float cr = uv.y - 128.0;
    float rr = luma + cr * (403.0 / 256.0);
    float gg = luma - (48.0 * cb + 120.0 * cr) / 256.0;
    float bb = luma + cb * (475.0 / 256.0);
    return float4(saturate(rr / 255.0), saturate(gg / 255.0), saturate(bb / 255.0), 1.0);
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
    // extra.y is the user's brightness multiplier (1.0 when untouched).
    return float4(saturate(rgb * extra.y), 1.0);
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
// One triangle covering the viewport: no vertex buffer, no input layout.
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
