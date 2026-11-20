// Synthesises the in-between frame at time t (0 = frame A, 1 = frame B).
//
// Three candidates are evaluated per pixel and blended by how well A and B agree:
//   0: static        (HUD, minimap, chat stay razor sharp)
//   1: forward flow  A->B, inverted with a fixed-point search
//   2: backward flow B->A, inverted the same way
// Where no candidate matches (occlusions) we take the temporally nearest frame
// instead of cross-fading, which avoids ghosting / double images.

Texture2D<float4> FrameA : register(t0);
Texture2D<float4> FrameB : register(t1);
Texture2D<float4> FlowAB : register(t2);
Texture2D<float4> FlowBA : register(t3);
SamplerState LinClamp    : register(s0);

cbuffer CB : register(b0)
{
    float2 gSize;
    float2 gInvSize;
    float  gT;
    float  gSharp;     // candidate selectivity
    float  gOccLo;     // mismatch where occlusion handling starts
    float  gOccHi;     // mismatch where it is fully on
    float  gTint;      // debug: tint generated frames magenta
    float3 gPad;
};

float4 Tint(float3 c) { return float4(lerp(c, float3(1.0, 0.0, 1.0), gTint * 0.35), 1.0); }

void VSMain(uint vid : SV_VertexID, out float4 pos : SV_Position)
{
    float2 uv = float2((vid << 1) & 2, vid & 2);
    pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}

bool Inside(float2 p) { return all(p >= 0.0) && all(p <= gSize); }

float Luma(float3 c) { return dot(c, float3(0.299, 0.587, 0.114)); }

float4 PSMain(float4 svpos : SV_Position) : SV_Target
{
    float2 x = svpos.xy;
    float2 inv = gInvSize;
    float t = gT;

    float2 v = FlowAB.SampleLevel(LinClamp, x * inv, 0).xy;
    float2 w = FlowBA.SampleLevel(LinClamp, x * inv, 0).xy;

    // Static neighbourhood: nothing to warp, plain temporal blend is exact.
    [branch] if (dot(v, v) + dot(w, w) < 0.02)
    {
        float3 ca = FrameA.Load(int3(x, 0)).rgb;
        float3 cb = FrameB.Load(int3(x, 0)).rgb;
        return Tint(lerp(ca, cb, t));
    }

    // Fixed-point inversion: find the vector of the pixel that lands on x.
    v = FlowAB.SampleLevel(LinClamp, (x - t * v) * inv, 0).xy;
    v = FlowAB.SampleLevel(LinClamp, (x - t * v) * inv, 0).xy;
    w = FlowBA.SampleLevel(LinClamp, (x - (1.0 - t) * w) * inv, 0).xy;
    w = FlowBA.SampleLevel(LinClamp, (x - (1.0 - t) * w) * inv, 0).xy;

    float2 pa[3];
    float2 pb[3];
    pa[0] = x;               pb[0] = x;
    pa[1] = x - t * v;       pb[1] = x + (1.0 - t) * v;
    pa[2] = x + t * w;       pb[2] = x - (1.0 - t) * w;

    float3 col[3];
    float  err[3];
    float  emin = 1e9;

    [unroll] for (int i = 0; i < 3; i++)
    {
        bool ia = Inside(pa[i]);
        bool ib = Inside(pb[i]);
        float3 ca = FrameA.SampleLevel(LinClamp, pa[i] * inv, 0).rgb;
        float3 cb = FrameB.SampleLevel(LinClamp, pb[i] * inv, 0).rgb;
        float3 d  = abs(ca - cb);
        float  e  = 0.5 * Luma(d) + 0.5 * max(d.r, max(d.g, d.b));

        float occ = smoothstep(gOccLo, gOccHi, e);
        float3 nearest = (t < 0.5) ? ca : cb;
        float3 c = lerp(lerp(ca, cb, t), nearest, occ);

        if (ia && !ib)       { c = ca; e = max(e, 0.04); }   // leaving the screen
        else if (!ia && ib)  { c = cb; e = max(e, 0.04); }   // entering the screen
        else if (!ia && !ib) { e = 1.0; }

        if (i == 0) e *= 0.85;   // static wins ties (HUD stability)

        col[i] = c;
        err[i] = e;
        emin = min(emin, e);
    }

    float3 sum = 0;
    float  wsum = 0;
    [unroll] for (int j = 0; j < 3; j++)
    {
        float wgt = exp2(-(err[j] - emin) * gSharp);
        sum  += col[j] * wgt;
        wsum += wgt;
    }
    return Tint(sum / wsum);
}
