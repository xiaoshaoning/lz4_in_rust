/* Reference oracle: wraps the C LZ4 library so the Rust port can be diff-tested.
 * Not part of the port; build with tools/build_oracle.sh
 *
 *   c_oracle block <level> <in> <out>   raw LZ4 block (level<=0 -> LZ4_compress_default)
 *   c_oracle unblock <in> <out> <size>  decode a raw block into exactly <size> bytes
 *   c_oracle bench <in>                 block compress/decompress throughput, best of 5
 *   c_oracle xxh32 <in>                 XXH32 of the whole file, seed 0
 *   c_oracle xxh32stream <in>           same, fed in irregular chunks (streaming state)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "lz4.h"
#include "lz4hc.h"
#define XXH_STATIC_LINKING_ONLY   /* XXH32_state_t is opaque otherwise */
#include "xxhash.h"

static unsigned char* slurp(const char* path, size_t* size)
{
    FILE* f = fopen(path, "rb");
    unsigned char* buf;
    long len;
    if (!f) { perror(path); exit(2); }
    if (fseek(f, 0, SEEK_END) != 0) { perror("fseek"); exit(2); }
    len = ftell(f);
    rewind(f);
    buf = (unsigned char*)malloc((size_t)len + 1);
    if (!buf) { exit(2); }
    if (len > 0 && fread(buf, 1, (size_t)len, f) != (size_t)len) { perror("fread"); exit(2); }
    fclose(f);
    *size = (size_t)len;
    return buf;
}

static void spit(const char* path, const void* buf, size_t size)
{
    FILE* f = fopen(path, "wb");
    if (!f) { perror(path); exit(2); }
    if (size > 0 && fwrite(buf, 1, size, f) != size) { perror("fwrite"); exit(2); }
    fclose(f);
}

int main(int argc, char** argv)
{
    size_t srcSize;
    unsigned char* src;

    if (argc >= 2 && strcmp(argv[1], "block") == 0) {
        int level, bound, csize;
        char* dst;
        if (argc != 5) { fprintf(stderr, "usage: c_oracle block <level> <in> <out>\n"); return 2; }
        level = atoi(argv[2]);
        src = slurp(argv[3], &srcSize);
        bound = LZ4_compressBound((int)srcSize);
        dst = (char*)malloc((size_t)bound);
        if (!dst) { return 2; }
        csize = (level <= 0)
              ? LZ4_compress_default((const char*)src, dst, (int)srcSize, bound)
              : LZ4_compress_HC((const char*)src, dst, (int)srcSize, bound, level);
        if (csize <= 0) { fprintf(stderr, "compression failed\n"); return 1; }
        spit(argv[4], dst, (size_t)csize);
        free(dst); free(src);
        return 0;
    }

    if (argc == 5 && strcmp(argv[1], "unblock") == 0) {
        int const outSize = atoi(argv[4]);
        char* dst;
        int dsize;
        src = slurp(argv[2], &srcSize);
        dst = (char*)malloc((size_t)outSize + 1);
        if (!dst) { return 2; }
        dsize = LZ4_decompress_safe((const char*)src, dst, (int)srcSize, outSize);
        if (dsize != outSize) {
            fprintf(stderr, "decode failed (%d != %d)\n", dsize, outSize);
            return 1;
        }
        spit(argv[3], dst, (size_t)dsize);
        free(dst); free(src);
        return 0;
    }

    if (argc == 3 && strcmp(argv[1], "bench") == 0) {
        int bound, csize = 0, i, dsize;
        double bestC = 1e9, bestD = 1e9;
        char* dst;
        char* back;
        src = slurp(argv[2], &srcSize);
        bound = LZ4_compressBound((int)srcSize);
        dst = (char*)malloc((size_t)bound);
        back = (char*)malloc(srcSize + 1);
        if (!dst || !back) { return 2; }
        for (i = 0; i < 30; i++) {
            struct timespec t0, t1;
            double dt;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            csize = LZ4_compress_default((const char*)src, dst, (int)srcSize, bound);
            clock_gettime(CLOCK_MONOTONIC, &t1);
            dt = (double)(t1.tv_sec - t0.tv_sec) + (double)(t1.tv_nsec - t0.tv_nsec) / 1e9;
            if (dt < bestC) bestC = dt;
        }
        for (i = 0; i < 30; i++) {
            struct timespec t0, t1;
            double dt;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            dsize = LZ4_decompress_safe(dst, back, csize, (int)srcSize);
            clock_gettime(CLOCK_MONOTONIC, &t1);
            dt = (double)(t1.tv_sec - t0.tv_sec) + (double)(t1.tv_nsec - t0.tv_nsec) / 1e9;
            if (dt < bestD) bestD = dt;
        }
        if (dsize != (int)srcSize || memcmp(back, src, srcSize) != 0) {
            fprintf(stderr, "reference roundtrip failed\n");
            return 1;
        }
        printf("%s: %lu bytes -> %d bytes, compress %.0f MiB/s, decompress %.0f MiB/s\n",
               argv[2], (unsigned long)srcSize, csize,
               ((double)srcSize / 1048576.0) / bestC,
               ((double)srcSize / 1048576.0) / bestD);
        free(dst); free(back); free(src);
        return 0;
    }

    if (argc == 3 && strcmp(argv[1], "xxh32") == 0) {
        src = slurp(argv[2], &srcSize);
        printf("%08x\n", (unsigned)XXH32(src, srcSize, 0));
        free(src);
        return 0;
    }

    if (argc == 3 && strcmp(argv[1], "xxh32stream") == 0) {
        XXH32_state_t state;
        size_t pos = 0, chunk = 1;
        src = slurp(argv[2], &srcSize);
        XXH32_reset(&state, 0);
        while (pos < srcSize) {
            size_t n = chunk < srcSize - pos ? chunk : srcSize - pos;
            XXH32_update(&state, src + pos, n);
            pos += n;
            chunk = chunk * 3 + 1;          /* 1, 4, 13, 40, ... : uneven splits */
        }
        printf("%08x\n", (unsigned)XXH32_digest(&state));
        free(src);
        return 0;
    }

    fprintf(stderr, "usage: c_oracle block|xxh32|xxh32stream ...\n");
    return 2;
}
