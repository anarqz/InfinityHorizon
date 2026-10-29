// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
#include "png_write.h"

#include <cstdlib>
#include <cstring>
#include <new>

// The same switches as miniz.c's own compile (build.ps1): only its deflate/zlib core is used.
#define MINIZ_NO_STDIO
#define MINIZ_NO_TIME
#define MINIZ_NO_ARCHIVE_APIS
#define MINIZ_NO_ZLIB_COMPATIBLE_NAMES
#include "miniz.h"

namespace fi::png {
namespace {

constexpr uint32_t kMaxSide = 16384;

uint8_t Paeth(uint8_t a, uint8_t b, uint8_t c) {
    const int p = a + b - c, pa = std::abs(p - a), pb = std::abs(p - b), pc = std::abs(p - c);
    return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
}

void Put32(std::vector<uint8_t> &out, uint32_t value) {
    out.push_back(static_cast<uint8_t>(value >> 24));
    out.push_back(static_cast<uint8_t>(value >> 16));
    out.push_back(static_cast<uint8_t>(value >> 8));
    out.push_back(static_cast<uint8_t>(value));
}

void Chunk(std::vector<uint8_t> &out, const char type[4], const uint8_t *data, size_t size) {
    Put32(out, static_cast<uint32_t>(size));
    const size_t start = out.size();
    out.insert(out.end(), type, type + 4);
    if (size) out.insert(out.end(), data, data + size);
    Put32(out, static_cast<uint32_t>(mz_crc32(MZ_CRC32_INIT, out.data() + start, size + 4)));
}

} // namespace

void FilterRow(int filter, const uint8_t *row, const uint8_t *previous, size_t bytes, unsigned bpp, uint8_t *out) {
    for (size_t i = 0; i < bytes; ++i) {
        const uint8_t a = i >= bpp ? row[i - bpp] : 0, b = previous ? previous[i] : 0,
                      c = previous && i >= bpp ? previous[i - bpp] : 0;
        uint8_t predictor = 0;
        switch (filter) {
        case 1: predictor = a; break;
        case 2: predictor = b; break;
        case 3: predictor = static_cast<uint8_t>((a + b) / 2); break;
        case 4: predictor = Paeth(a, b, c); break;
        default: break;
        }
        out[i] = static_cast<uint8_t>(row[i] - predictor);
    }
}

bool Write(const uint8_t *pixels, uint32_t width, uint32_t height, size_t stride, bool alpha, std::vector<uint8_t> &out) {
    out.clear();
    const unsigned bpp = alpha ? 4u : 3u;
    if (!pixels || !width || !height || width > kMaxSide || height > kMaxSide || stride < size_t{width} * bpp)
        return false;
    try {
        const size_t row_bytes = size_t{width} * bpp;
        std::vector<uint8_t> filtered((row_bytes + 1) * height), candidate(row_bytes);
        for (uint32_t y = 0; y < height; ++y) {
            const uint8_t *row = pixels + stride * y, *previous = y ? pixels + stride * (y - 1) : nullptr;
            uint8_t *line = filtered.data() + (row_bytes + 1) * y;
            uint64_t best_cost = UINT64_MAX;
            for (int filter = 0; filter <= 4; ++filter) {
                FilterRow(filter, row, previous, row_bytes, bpp, candidate.data());
                uint64_t cost = 0;
                for (size_t i = 0; i < row_bytes; ++i) cost += static_cast<uint64_t>(std::abs(static_cast<int8_t>(candidate[i])));
                if (cost < best_cost) {
                    best_cost = cost;
                    line[0] = static_cast<uint8_t>(filter);
                    std::memcpy(line + 1, candidate.data(), row_bytes);
                }
            }
        }
        mz_ulong packed_size = mz_compressBound(static_cast<mz_ulong>(filtered.size()));
        std::vector<uint8_t> packed(packed_size);
        if (mz_compress2(packed.data(), &packed_size, filtered.data(), static_cast<mz_ulong>(filtered.size()),
                         MZ_BEST_COMPRESSION) != MZ_OK)
            return false;
        static const uint8_t signature[8] = {0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a};
        const uint8_t header[13] = {static_cast<uint8_t>(width >> 24), static_cast<uint8_t>(width >> 16),
                                    static_cast<uint8_t>(width >> 8),  static_cast<uint8_t>(width),
                                    static_cast<uint8_t>(height >> 24), static_cast<uint8_t>(height >> 16),
                                    static_cast<uint8_t>(height >> 8), static_cast<uint8_t>(height),
                                    8, static_cast<uint8_t>(alpha ? 6 : 2), 0, 0, 0};
        out.reserve(8 + 25 + 12 + packed_size + 12);
        out.insert(out.end(), signature, signature + 8);
        Chunk(out, "IHDR", header, sizeof(header));
        Chunk(out, "IDAT", packed.data(), packed_size);
        Chunk(out, "IEND", nullptr, 0);
        return true;
    } catch (const std::bad_alloc &) {
        out.clear();
        return false;
    }
}

} // namespace fi::png
