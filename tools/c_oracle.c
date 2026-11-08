/* Reference oracle: wraps the C LZ4 library so the Rust port can be diff-tested.
 * Not part of the port; build with tools/build_oracle.sh
 *
 *   c_oracle block <level> <in> <out>   raw LZ4 block (level<=0 -> LZ4_compress_default)
 *   c_oracle unblock <in> <out> <size>  decode a raw block into exactly <size> bytes
 *   c_oracle frame <level> <blockid> <linked> <blockcrc> <contentcrc> <contentsize> <in> <out>
 *                                       one whole frame, prefs chosen explicitly
 *   c_oracle frame-dict <dict> <in> <out>   one frame compressed with a dictionary file
 *   c_oracle blockdict <dict> <in> <out>
 *                                       raw block with a dictionary (loadDict + continue)
 *   c_oracle unframe <in> <out>         decode a frame with LZ4F_decompress
 *   c_oracle unframe-dict <dict> <in> <out>  decode a frame that used a dictionary
 *   c_oracle bench <in>                 block compress/decompress throughput, best of 5
 *   c_oracle xxh32 <in>                 XXH32 of the whole file, seed 0
 *   c_oracle xxh32stream <in>           same, fed in irregular chunks (streaming state)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "lz4.h"
#include "lz4frame.h"
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

    if (argc == 10 && strcmp(argv[1], "frame") == 0) {
        /* The streaming API, like the lz4 command line: it honours the requested block
         * size and block mode. (LZ4F_compressFrame, the single-shot API, silently shrinks
         * the block size to the input and forces independent blocks when there is only
         * one block of them — useless for producing test coverage.) */
        LZ4F_preferences_t prefs = LZ4F_INIT_PREFERENCES;
        LZ4F_cctx* cctx = NULL;
        LZ4F_compressOptions_t options;
        memset(&options, 0, sizeof(options));
        size_t cap, pos = 0, done;
        unsigned char* dst;
        prefs.compressionLevel = atoi(argv[2]);
        prefs.frameInfo.blockSizeID = (LZ4F_blockSizeID_t)atoi(argv[3]);
        prefs.frameInfo.blockMode = atoi(argv[4]) ? LZ4F_blockLinked : LZ4F_blockIndependent;
        prefs.frameInfo.blockChecksumFlag = atoi(argv[5]) ? LZ4F_blockChecksumEnabled : LZ4F_noBlockChecksum;
        prefs.frameInfo.contentChecksumFlag = atoi(argv[6]) ? LZ4F_contentChecksumEnabled : LZ4F_noContentChecksum;
        prefs.frameInfo.contentSize = (unsigned long long)0;
        prefs.autoFlush = 1;   /* fixtures must not depend on liblz4's internal buffering */
        src = slurp(argv[8], &srcSize);
        if (atoi(argv[7])) prefs.frameInfo.contentSize = (unsigned long long)srcSize;
        /* the whole-frame bound, not the update bound: it includes the footer */
        cap = LZ4F_compressFrameBound(srcSize, &prefs);
        dst = (unsigned char*)malloc(cap);
        if (!dst) { return 2; }
        if (LZ4F_isError(LZ4F_createCompressionContext(&cctx, LZ4F_VERSION))) { return 1; }
        done = LZ4F_compressBegin(cctx, dst, cap, &prefs);
        if (LZ4F_isError(done)) { fprintf(stderr, "begin: %s\n", LZ4F_getErrorName(done)); return 1; }
        pos = done;
        done = LZ4F_compressUpdate(cctx, dst + pos, cap - pos, src, srcSize, &options);
        if (LZ4F_isError(done)) { fprintf(stderr, "update: %s\n", LZ4F_getErrorName(done)); return 1; }
        pos += done;
        done = LZ4F_compressEnd(cctx, dst + pos, cap - pos, &options);
        if (LZ4F_isError(done)) { fprintf(stderr, "end: %s\n", LZ4F_getErrorName(done)); return 1; }
        pos += done;
        LZ4F_freeCompressionContext(cctx);
        spit(argv[9], dst, pos);
        free(dst); free(src);
        return 0;
    }

    if (argc == 5 && strcmp(argv[1], "frame-dict") == 0) {
        /* A frame whose blocks may reference a dictionary file: the streaming API with
         * LZ4F_compressBegin_usingDict, which is what `lz4 -D` does. Linked blocks are
         * required, since only a linked block can reach into the dictionary. */
        LZ4F_preferences_t prefs = LZ4F_INIT_PREFERENCES;
        LZ4F_cctx* cctx = NULL;
        LZ4F_compressOptions_t options;
        unsigned char* dict;
        unsigned char* dst;
        size_t dictSize, cap, pos, done;
        memset(&options, 0, sizeof(options));
        prefs.frameInfo.blockMode = LZ4F_blockLinked;
        prefs.autoFlush = 1;
        dict = slurp(argv[2], &dictSize);
        src = slurp(argv[3], &srcSize);
        cap = LZ4F_compressFrameBound(srcSize, &prefs) + 64;
        dst = (unsigned char*)malloc(cap);
        if (!dst) { return 2; }
        if (LZ4F_isError(LZ4F_createCompressionContext(&cctx, LZ4F_VERSION))) { return 1; }
        pos = LZ4F_compressBegin_usingDict(cctx, dst, cap, dict, dictSize, &prefs);
        if (LZ4F_isError(pos)) {
            fprintf(stderr, "begin: %s\n", LZ4F_getErrorName(pos));
            return 1;
        }
        done = LZ4F_compressUpdate(cctx, dst + pos, cap - pos, src, srcSize, &options);
        if (LZ4F_isError(done)) {
            fprintf(stderr, "update: %s\n", LZ4F_getErrorName(done));
            return 1;
        }
        pos += done;
        /* one call: the buffer was sized with compressFrameBound, so the footer fits */
        done = LZ4F_compressEnd(cctx, dst + pos, cap - pos, &options);
        if (LZ4F_isError(done)) {
            fprintf(stderr, "end: %s%c", LZ4F_getErrorName(done), 10);
            return 1;
        }
        pos += done;
        LZ4F_freeCompressionContext(cctx);
        spit(argv[4], dst, pos);
        free(dst); free(src); free(dict);
        return 0;
    }

    if (argc == 5 && strcmp(argv[1], "blockdict") == 0) {
        /* The frame layer's linked path, in isolation: load a dictionary, then compress one
         * block with LZ4_compress_fast_continue (external dictionary mode). */
        static LZ4_stream_t stream;
        unsigned char* dict;
        size_t dictSize;
        char* dst;
        int bound, csize;
        dict = slurp(argv[2], &dictSize);
        src = slurp(argv[3], &srcSize);
        bound = LZ4_compressBound((int)srcSize);
        dst = (char*)malloc((size_t)bound);
        if (!dst) { return 2; }
        if (LZ4_loadDict(&stream, (const char*)dict, (int)dictSize) < 0) { return 1; }
        csize = LZ4_compress_fast_continue(&stream, (const char*)src, dst, (int)srcSize, bound, 1);
        if (csize <= 0) { fprintf(stderr, "blockdict failed%c", 10); return 1; }
        spit(argv[4], dst, (size_t)csize);
        free(dst); free(src); free(dict);
        return 0;
    }

    if (argc == 4 && strcmp(argv[1], "unframe") == 0) {
        LZ4F_dctx* dctx = NULL;
        size_t pos = 0, len = 0, cap, hint = 1;
        char* out;
        src = slurp(argv[2], &srcSize);
        /* First guess only: the loop below grows it, so a frame may expand 100x. */
        cap = srcSize * 4 + 65536;
        out = (char*)malloc(cap);
        if (!out) { return 2; }
        if (LZ4F_isError(LZ4F_createDecompressionContext(&dctx, LZ4F_VERSION))) { return 1; }
        while (hint != 0) {
            size_t dstSize = cap - len;
            size_t inSize = srcSize - pos;
            size_t const r = LZ4F_decompress(dctx, out + len, &dstSize, src + pos, &inSize, NULL);
            if (LZ4F_isError(r)) {
                fprintf(stderr, "unframe: %s\n", LZ4F_getErrorName(r));
                return 1;
            }
            len += dstSize; pos += inSize; hint = r;
            if (cap - len < 65536) {
                cap *= 2;
                out = (char*)realloc(out, cap);
                if (!out) { return 2; }
            }
            if (inSize == 0 && dstSize == 0 && hint != 0) {
                fprintf(stderr, "unframe: truncated input\n");
                return 1;
            }
        }
        LZ4F_freeDecompressionContext(dctx);
        spit(argv[3], out, len);
        free(out); free(src);
        return 0;
    }

    if (argc == 5 && strcmp(argv[1], "unframe-dict") == 0) {
        LZ4F_dctx* dctx = NULL;
        unsigned char* dict;
        size_t dictSize, pos = 0, len = 0, cap, hint = 1;
        char* out;
        dict = slurp(argv[2], &dictSize);
        src = slurp(argv[3], &srcSize);
        cap = srcSize * 8 + 65536;
        out = (char*)malloc(cap);
        if (!out) { return 2; }
        if (LZ4F_isError(LZ4F_createDecompressionContext(&dctx, LZ4F_VERSION))) { return 1; }
        while (hint != 0) {
            size_t dstSize = cap - len;
            size_t inSize = srcSize - pos;
            size_t const r = LZ4F_decompress_usingDict(dctx, out + len, &dstSize,
                                                       src + pos, &inSize, dict, dictSize, NULL);
            if (LZ4F_isError(r)) {
                fprintf(stderr, "unframe-dict: %s%c", LZ4F_getErrorName(r), 10);
                return 1;
            }
            len += dstSize; pos += inSize; hint = r;
            if (cap - len < 65536) { cap *= 2; out = (char*)realloc(out, cap); if (!out) return 2; }
            if (inSize == 0 && dstSize == 0 && hint != 0) {
                fprintf(stderr, "unframe-dict: truncated input%c", 10);
                return 1;
            }
        }
        LZ4F_freeDecompressionContext(dctx);
        spit(argv[4], out, len);
        free(out); free(src); free(dict);
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

    fprintf(stderr, "usage: c_oracle block|unblock|frame|unframe|bench|xxh32|xxh32stream ...\n");
    return 2;
}
