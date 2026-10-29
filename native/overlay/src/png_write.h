// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// A minimal PNG writer (T19): 8 bits per channel, not interlaced, only IHDR, IDAT and IEND; colour
// type 6 (RGBA) or 2 (RGB). Each row takes the filter with the smallest sum of absolute
// differences; IDAT is one zlib stream at level 9 (MZ_BEST_COMPRESSION: miniz's level 10 costs
// time on the game thread for little gain). Used by png_fit.cpp; no WIC here.
#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

namespace fi::png {

// `pixels`: `height` rows of `width` RGBA (alpha) or RGB (!alpha) bytes, `stride` bytes apart.
// Replaces `out` with the PNG file; false (and `out` empty) on bad sizes or no memory.
bool Write(const uint8_t *pixels, uint32_t width, uint32_t height, size_t stride, bool alpha, std::vector<uint8_t> &out);

// The filter (0 none, 1 sub, 2 up, 3 average, 4 Paeth) of `row` (with `previous`, null for the
// first row) written into `out` (row bytes, without the filter byte). Exposed for the tests.
void FilterRow(int filter, const uint8_t *row, const uint8_t *previous, size_t bytes, unsigned bpp, uint8_t *out);

} // namespace fi::png
