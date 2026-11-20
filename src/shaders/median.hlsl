// 3x3 per-component median of both flow fields (z=0 forward, z=1 backward):
// removes outlier vectors while keeping motion edges sharp.

Texture2D<float4>   SrcF : register(t0);
Texture2D<float4>   SrcB : register(t1);
RWTexture2D<float4> DstF : register(u0);
RWTexture2D<float4> DstB : register(u1);

cbuffer CB : register(b0)
{
    int2 gSize;
};

#define SORT(a, b) { float _t = min(a, b); b = max(a, b); a = _t; }

float Median9(float p[9])
{
    SORT(p[1], p[2]); SORT(p[4], p[5]); SORT(p[7], p[8]);
    SORT(p[0], p[1]); SORT(p[3], p[4]); SORT(p[6], p[7]);
    SORT(p[1], p[2]); SORT(p[4], p[5]); SORT(p[7], p[8]);
    SORT(p[0], p[3]); SORT(p[5], p[8]); SORT(p[4], p[7]);
    SORT(p[3], p[6]); SORT(p[1], p[4]); SORT(p[2], p[5]);
    SORT(p[4], p[7]); SORT(p[4], p[2]); SORT(p[6], p[4]);
    SORT(p[4], p[2]);
    return p[4];
}

float4 Med(Texture2D<float4> Src, int2 p)
{
    float x[9];
    float y[9];
    int k = 0;
    [unroll] for (int dy = -1; dy <= 1; dy++)
    [unroll] for (int dx = -1; dx <= 1; dx++)
    {
        float4 s = Src.Load(int3(clamp(p + int2(dx, dy), int2(0, 0), gSize - 1), 0));
        x[k] = s.x;
        y[k] = s.y;
        k++;
    }
    return float4(Median9(x), Median9(y), Src.Load(int3(p, 0)).z, 0);
}

[numthreads(8, 8, 1)]
void CSMain(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= (uint2)gSize)) return;
    int2 p = int2(id.xy);
    [branch] if (id.z == 0) DstF[p] = Med(SrcF, p);
    else                    DstB[p] = Med(SrcB, p);
}
