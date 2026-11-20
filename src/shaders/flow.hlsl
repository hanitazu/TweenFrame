// Coarse-to-fine block-matching optical flow, one pyramid level per dispatch.
// Both directions (A->B in z=0, B->A in z=1) are computed in the same dispatch.
//
// For every pixel of the source frame we look for the best matching spot in the
// destination frame. Candidates:
//   * the zero vector,
//   * the upsampled vector of the coarser level (and its 4 neighbours),
//   * the vector found for the previous frame pair (temporal prediction),
//   * an exhaustive search on the coarsest level only.
// Candidates are evaluated on the integer grid (4 Gather instructions fetch a
// whole 4x4 patch), then refined by descent and a sub-pixel parabola fit.
// Matching cost = zero-mean SAD, robust against exposure / lighting changes.
//
// Vectors are stored in full-resolution pixels so every level can read every
// other level without rescaling. .z holds the matching cost.

Texture2D<float>  LumaA   : register(t0);
Texture2D<float>  LumaB   : register(t1);
Texture2D<float4> ParentF : register(t2);
Texture2D<float4> ParentB : register(t3);
Texture2D<float4> PriorF  : register(t4);
Texture2D<float4> PriorB  : register(t5);
RWTexture2D<float4> OutF  : register(u0);
RWTexture2D<float4> OutB  : register(u1);
SamplerState LinClamp     : register(s0);

cbuffer CB : register(b0)
{
    int2   gSize;       // size of this level
    float2 gInvSize;    // 1 / size
    float  gScale;      // full-res pixels per level pixel
    float  gHasParent;  // 1 if a coarser level exists
    float  gHasPrior;   // 1 if a previous-pair flow exists
    float  gRadius;     // exhaustive search radius (coarsest level), else 0
    float  gLambda;     // smoothness penalty per level pixel
    float  gZeroBias;   // multiplier (<=1) favouring the zero vector
    int    gIters;      // max descent steps
};

static float4 A0, A1, A2, A3;
static float MA;
static int2 P;
static float2 PRED;

// 4x4 patch with top-left texel `tl`, fetched with 4 gathers.
void Patch(Texture2D<float> T, int2 tl, out float4 g0, out float4 g1, out float4 g2, out float4 g3)
{
    float2 c = float2(tl) + 1.0; // corner shared by the first 2x2 quad
    g0 = T.Gather(LinClamp, c * gInvSize);
    g1 = T.Gather(LinClamp, (c + float2(2, 0)) * gInvSize);
    g2 = T.Gather(LinClamp, (c + float2(0, 2)) * gInvSize);
    g3 = T.Gather(LinClamp, (c + float2(2, 2)) * gInvSize);
}

float Cost(Texture2D<float> I1, int2 d)
{
    float4 b0, b1, b2, b3;
    Patch(I1, P - 1 + d, b0, b1, b2, b3);
    float4 s4 = b0 + b1 + b2 + b3;
    float mb = dot(s4, 0.0625);
    float o = MA - mb;
    float4 e = abs(A0 - b0 - o) + abs(A1 - b1 - o) + abs(A2 - b2 - o) + abs(A3 - b3 - o);
    float c = dot(e, 0.0625) + 0.25 * abs(o);
    int2 q = P + d;
    if (any(q < 0) || any(q >= gSize)) c += 0.02;
    return c + gLambda * length(float2(d) - PRED);
}

float4 Flow(int2 p, Texture2D<float> I0, Texture2D<float> I1, Texture2D<float4> Parent, Texture2D<float4> Prior)
{
    P = p;
    Patch(I0, p - 1, A0, A1, A2, A3);
    MA = dot(A0 + A1 + A2 + A3, 0.0625);

    float2 uv = (float2(p) + 0.5) * gInvSize;
    PRED = 0;
    if (gHasParent > 0.5) PRED = Parent.SampleLevel(LinClamp, uv, 0).xy / gScale;

    int2 best = 0;
    float bestC = Cost(I1, 0) * gZeroBias;

    #define TRY(vv) { int2 _d = int2(round(vv)); float _c = Cost(I1, _d); if (_c < bestC) { bestC = _c; best = _d; } }

    if (gHasParent > 0.5)
    {
        float2 o = 2.0 * gInvSize; // one parent pixel
        TRY(PRED);
        TRY(Parent.SampleLevel(LinClamp, uv + float2(-o.x, 0), 0).xy / gScale);
        TRY(Parent.SampleLevel(LinClamp, uv + float2( o.x, 0), 0).xy / gScale);
        TRY(Parent.SampleLevel(LinClamp, uv + float2(0, -o.y), 0).xy / gScale);
        TRY(Parent.SampleLevel(LinClamp, uv + float2(0,  o.y), 0).xy / gScale);
    }
    if (gHasPrior > 0.5)
    {
        TRY(Prior.SampleLevel(LinClamp, uv, 0).xy / gScale);
    }
    if (gRadius > 0.5)
    {
        int R = (int)gRadius;
        [loop] for (int dy = -R; dy <= R; dy++)
        [loop] for (int dx = -R; dx <= R; dx++)
        {
            int2 d = int2(dx, dy);
            float c = Cost(I1, d);
            if (c < bestC) { bestC = c; best = d; }
        }
    }

    // Descent: move one pixel at a time while a neighbour is better.
    int2 c = best;
    float cc = bestC;
    float cxm = 0, cxp = 0, cym = 0, cyp = 0;
    bool converged = false;
    [loop] for (int it = 0; it < gIters; it++)
    {
        cxm = Cost(I1, c + int2(-1, 0));
        cxp = Cost(I1, c + int2( 1, 0));
        cym = Cost(I1, c + int2( 0,-1));
        cyp = Cost(I1, c + int2( 0, 1));
        float m = min(min(cxm, cxp), min(cym, cyp));
        if (m >= cc) { converged = true; break; }
        cc = m;
        if (m == cxm)      c.x -= 1;
        else if (m == cxp) c.x += 1;
        else if (m == cym) c.y -= 1;
        else               c.y += 1;
    }

    // Sub-pixel: equiangular line fit (exact for the V-shaped SAD surface,
    // where a parabola fit would be biased towards the integer position).
    float2 sub = 0;
    if (converged)
    {
        float hx = max(cxm, cxp) - cc;
        float hy = max(cym, cyp) - cc;
        if (hx > 1e-6) sub.x = clamp(0.5 * (cxm - cxp) / hx, -0.5, 0.5);
        if (hy > 1e-6) sub.y = clamp(0.5 * (cym - cyp) / hy, -0.5, 0.5);
    }
    return float4((float2(c) + sub) * gScale, cc, 0);
}

[numthreads(8, 8, 1)]
void CSMain(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= (uint2)gSize)) return;
    int2 p = int2(id.xy);
    [branch] if (id.z == 0)
        OutF[p] = Flow(p, LumaA, LumaB, ParentF, PriorF);
    else
        OutB[p] = Flow(p, LumaB, LumaA, ParentB, PriorB);
}
