// Builds the luma pyramid used for motion estimation.
// Every level is a 2x2 box-filtered copy of the level above it.

Texture2D<float4> SrcColor : register(t0);
Texture2D<float>  SrcLuma  : register(t1);
RWTexture2D<float> Dst     : register(u0);
SamplerState LinClamp      : register(s0);

cbuffer CB : register(b0)
{
    int2   gDstSize;
    float2 gSrcInvSize;
};

// One bilinear tap exactly between four source texels = 2x2 average.
float2 SrcUV(uint2 id) { return (float2(id) * 2.0 + 1.0) * gSrcInvSize; }

[numthreads(8, 8, 1)]
void CSFromColor(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= (uint2)gDstSize)) return;
    float3 c = SrcColor.SampleLevel(LinClamp, SrcUV(id.xy), 0).rgb;
    Dst[id.xy] = dot(c, float3(0.299, 0.587, 0.114));
}

[numthreads(8, 8, 1)]
void CSFromLuma(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= (uint2)gDstSize)) return;
    Dst[id.xy] = SrcLuma.SampleLevel(LinClamp, SrcUV(id.xy), 0);
}
