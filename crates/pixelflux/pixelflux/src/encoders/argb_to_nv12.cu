/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// ARGB/ABGR -> NV12, and to P010 and planar 4:4:4 at 10 bits, BT.709 at limited range. NVENC's own conversion follows the matrix a
// session declares but weights the two columns of a 4:2:0 block 3:1 instead of averaging them,
// which is what these kernels replace. A hash per band of rows follows, for change detection.

__device__ __forceinline__ float luma(float r, float g, float b)
{
    return 0.2126f * r + 0.7152f * g + 0.0722f * b;
}

__device__ __forceinline__ unsigned char clamp8(float v)
{
    return (unsigned char)__float2int_rn(fminf(fmaxf(v, 0.0f), 255.0f));
}

__device__ __forceinline__ unsigned char luma8(float r, float g, float b)
{
    return clamp8(16.0f + luma(r, g, b) * (219.0f / 255.0f));
}

// The chroma pair of a 2x2 block's average RGB.
__device__ __forceinline__ void chroma8(float r, float g, float b, unsigned char* cb, unsigned char* cr)
{
    float y = luma(r, g, b);
    *cb = clamp8(128.0f + (b - y) * (224.0f / 255.0f) / (2.0f * (1.0f - 0.0722f)));
    *cr = clamp8(128.0f + (r - y) * (224.0f / 255.0f) / (2.0f * (1.0f - 0.2126f)));
}

// A pixel's B, G, and R, from either byte order.
__device__ __forceinline__ void unpack(const unsigned char* px, int swap_rb, float* r, float* g, float* b)
{
    *b = px[swap_rb ? 2 : 0];
    *g = px[1];
    *r = px[swap_rb ? 0 : 2];
}

extern "C" __global__ void argb_to_nv12(
    const unsigned char* __restrict__ src, int src_pitch,
    unsigned char* __restrict__ dst, int dst_pitch,
    int width, int height, int swap_rb)
{
    int cx = blockIdx.x * blockDim.x + threadIdx.x;   // chroma column
    int cy = blockIdx.y * blockDim.y + threadIdx.y;   // chroma row
    if (cx * 2 >= width || cy * 2 >= height) return;

    float sr = 0.0f, sg = 0.0f, sb = 0.0f;
    unsigned char* uv = dst + (long)dst_pitch * height + (long)dst_pitch * cy + cx * 2;
    for (int dy = 0; dy < 2; ++dy) {
        int y = min(cy * 2 + dy, height - 1);
        const unsigned char* row = src + (long)src_pitch * y;
        unsigned char* luma_row = dst + (long)dst_pitch * y;
        for (int dx = 0; dx < 2; ++dx) {
            int x = min(cx * 2 + dx, width - 1);
            float r, g, b;
            unpack(row + (long)x * 4, swap_rb, &r, &g, &b);
            sr += r; sg += g; sb += b;
            luma_row[x] = luma8(r, g, b);
        }
    }
    chroma8(sr * 0.25f, sg * 0.25f, sb * 0.25f, &uv[0], &uv[1]);
}

// The same convert for an import the driver hands back as a CUDA array rather than linear
// memory: the texture unit resolves the array's layout, so the RGB is read in place instead of
// being copied into a linear surface first.
extern "C" __global__ void argb_tex_to_nv12(
    cudaTextureObject_t src,
    unsigned char* __restrict__ dst, int dst_pitch,
    int width, int height, int swap_rb)
{
    int cx = blockIdx.x * blockDim.x + threadIdx.x;
    int cy = blockIdx.y * blockDim.y + threadIdx.y;
    if (cx * 2 >= width || cy * 2 >= height) return;

    float sr = 0.0f, sg = 0.0f, sb = 0.0f;
    unsigned char* uv = dst + (long)dst_pitch * height + (long)dst_pitch * cy + cx * 2;
    for (int dy = 0; dy < 2; ++dy) {
        int y = min(cy * 2 + dy, height - 1);
        unsigned char* luma_row = dst + (long)dst_pitch * y;
        for (int dx = 0; dx < 2; ++dx) {
            int x = min(cx * 2 + dx, width - 1);
            uchar4 px = tex2D<uchar4>(src, x, y);
            float b = swap_rb ? px.z : px.x, g = px.y, r = swap_rb ? px.x : px.z;
            sr += r; sg += g; sb += b;
            luma_row[x] = luma8(r, g, b);
        }
    }
    chroma8(sr * 0.25f, sg * 0.25f, sb * 0.25f, &uv[0], &uv[1]);
}

// The 10-bit converts: the same matrix and siting from the 8-bit RGB straight to 10-bit samples,
// which NVENC's own upconversion of an 8-bit surface cannot recover. Samples are 16 bits wide
// with the ten in the high bits, as NVENC's 10-bit surface formats hold them.

__device__ __forceinline__ unsigned short sample10(float v)
{
    return (unsigned short)(__float2int_rn(fminf(fmaxf(v, 0.0f), 1023.0f)) << 6);
}

__device__ __forceinline__ unsigned short luma10(float r, float g, float b)
{
    return sample10(64.0f + luma(r, g, b) * (876.0f / 255.0f));
}

__device__ __forceinline__ void chroma10(float r, float g, float b, unsigned short* cb, unsigned short* cr)
{
    float y = luma(r, g, b);
    *cb = sample10(512.0f + (b - y) * (896.0f / 255.0f) / (2.0f * (1.0f - 0.0722f)));
    *cr = sample10(512.0f + (r - y) * (896.0f / 255.0f) / (2.0f * (1.0f - 0.2126f)));
}

// A pixel of the linear surface or of the texture over an array-typed import.
struct Linear {
    const unsigned char* src;
    int pitch;
    int swap_rb;
    __device__ __forceinline__ void operator()(int x, int y, float* r, float* g, float* b) const
    {
        unpack(src + (long)pitch * y + (long)x * 4, swap_rb, r, g, b);
    }
};

struct Texture {
    cudaTextureObject_t tex;
    int swap_rb;
    __device__ __forceinline__ void operator()(int x, int y, float* r, float* g, float* b) const
    {
        uchar4 px = tex2D<uchar4>(tex, x, y);
        *b = swap_rb ? px.z : px.x;
        *g = px.y;
        *r = swap_rb ? px.x : px.z;
    }
};

// One 2x2 block into P010: a luma plane and an interleaved chroma plane of the block averages.
template <class Read>
__device__ __forceinline__ void block_p010(Read read, unsigned char* dst, int dst_pitch, int width, int height)
{
    int cx = blockIdx.x * blockDim.x + threadIdx.x;
    int cy = blockIdx.y * blockDim.y + threadIdx.y;
    if (cx * 2 >= width || cy * 2 >= height) return;

    float sr = 0.0f, sg = 0.0f, sb = 0.0f;
    unsigned short* uv = (unsigned short*)(dst + (long)dst_pitch * height + (long)dst_pitch * cy) + cx * 2;
    for (int dy = 0; dy < 2; ++dy) {
        int y = min(cy * 2 + dy, height - 1);
        unsigned short* luma_row = (unsigned short*)(dst + (long)dst_pitch * y);
        for (int dx = 0; dx < 2; ++dx) {
            int x = min(cx * 2 + dx, width - 1);
            float r, g, b;
            read(x, y, &r, &g, &b);
            sr += r; sg += g; sb += b;
            luma_row[x] = luma10(r, g, b);
        }
    }
    chroma10(sr * 0.25f, sg * 0.25f, sb * 0.25f, &uv[0], &uv[1]);
}

// One 2x2 block into planar 4:4:4: the Y, Cb, and Cr planes one after another, a sample a pixel.
template <class Read>
__device__ __forceinline__ void block_yuv444p10(Read read, unsigned char* dst, int dst_pitch, int width, int height)
{
    int cx = blockIdx.x * blockDim.x + threadIdx.x;
    int cy = blockIdx.y * blockDim.y + threadIdx.y;
    if (cx * 2 >= width || cy * 2 >= height) return;

    long plane = (long)dst_pitch * height;
    for (int dy = 0; dy < 2; ++dy) {
        int y = cy * 2 + dy;
        if (y >= height) break;
        unsigned short* row = (unsigned short*)(dst + (long)dst_pitch * y);
        for (int dx = 0; dx < 2; ++dx) {
            int x = cx * 2 + dx;
            if (x >= width) break;
            float r, g, b;
            read(x, y, &r, &g, &b);
            unsigned short cb, cr;
            chroma10(r, g, b, &cb, &cr);
            row[x] = luma10(r, g, b);
            ((unsigned short*)((unsigned char*)row + plane))[x] = cb;
            ((unsigned short*)((unsigned char*)row + 2 * plane))[x] = cr;
        }
    }
}

extern "C" __global__ void argb_to_p010(
    const unsigned char* __restrict__ src, int src_pitch,
    unsigned char* __restrict__ dst, int dst_pitch,
    int width, int height, int swap_rb)
{
    Linear read = { src, src_pitch, swap_rb };
    block_p010(read, dst, dst_pitch, width, height);
}

extern "C" __global__ void argb_tex_to_p010(
    cudaTextureObject_t src,
    unsigned char* __restrict__ dst, int dst_pitch,
    int width, int height, int swap_rb)
{
    Texture read = { src, swap_rb };
    block_p010(read, dst, dst_pitch, width, height);
}

extern "C" __global__ void argb_to_yuv444p10(
    const unsigned char* __restrict__ src, int src_pitch,
    unsigned char* __restrict__ dst, int dst_pitch,
    int width, int height, int swap_rb)
{
    Linear read = { src, src_pitch, swap_rb };
    block_yuv444p10(read, dst, dst_pitch, width, height);
}

extern "C" __global__ void argb_tex_to_yuv444p10(
    cudaTextureObject_t src,
    unsigned char* __restrict__ dst, int dst_pitch,
    int width, int height, int swap_rb)
{
    Texture read = { src, swap_rb };
    block_yuv444p10(read, dst, dst_pitch, width, height);
}

// A hash of each band of `band_rows` rows of a packed 32-bit frame: the change detection of a
// capture that reports no damage (NvFBC's), run where the frame lies. Each pixel's color, its
// alpha byte aside, is mixed with its place in the band and the mixes summed, a reduction that
// needs no order: one block of HASH_THREADS threads a band and segment of its columns (the grid's
// y), whose sums the caller adds into the band's.
#define HASH_THREADS 256

__device__ __forceinline__ unsigned long long mix64(unsigned long long x)
{
    x ^= x >> 30;
    x *= 0xbf58476d1ce4e5b9ULL;
    x ^= x >> 27;
    x *= 0x94d049bb133111ebULL;
    return x ^ (x >> 31);
}

extern "C" __global__ void band_hash(
    const unsigned char* src, int src_pitch,
    int width, int height, int band_rows,
    unsigned long long* out)
{
    __shared__ unsigned long long sums[HASH_THREADS];
    int top = blockIdx.x * band_rows;
    int bottom = min(top + band_rows, height);
    int stride = gridDim.y * HASH_THREADS;
    unsigned long long sum = 0;
    for (int y = top; y < bottom; ++y) {
        const unsigned int* row = (const unsigned int*)(src + (long)src_pitch * y);
        unsigned long long at = (unsigned long long)(y - top) * width;
        for (int x = blockIdx.y * HASH_THREADS + threadIdx.x; x < width; x += stride)
            sum += mix64((at + x) << 24 | (row[x] & 0xffffffu));
    }
    sums[threadIdx.x] = sum;
    __syncthreads();
    for (int s = HASH_THREADS / 2; s > 0; s >>= 1) {
        if (threadIdx.x < s)
            sums[threadIdx.x] += sums[threadIdx.x + s];
        __syncthreads();
    }
    if (threadIdx.x == 0)
        out[blockIdx.x * gridDim.y + blockIdx.y] = sums[0];
}
