/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* Cloud save codecs (see azimuth_codec.h).

   XPRESS Huffman, decoding only (MS-XCA 2.2.4): blocks of 64 KiB of output, each starting with
   256 bytes of 4-bit code lengths for 512 symbols (low nibble = even symbol), then a bit
   stream read as little-endian 16-bit words, most significant bit first. Symbols < 256 are
   literals; 256 + (offset bits << 4 | length) is a match whose length 15 continues in the byte
   stream (u8, then u16, then u32).

   Method 2: sequences of varint literal count, the literals, then (unless the output is
   complete) varint match length - 4 and varint offset - 1. Varints are LEB128. A match may
   overlap its own output, so a run of zeros is one sequence. */
#include "codec.h"

/* -------------------------------------------------------- XPRESS Huffman */

#define XPRESS_SYMBOLS 512
#define XPRESS_TABLE_BITS 15
#define XPRESS_BLOCK 65536u
#define XPRESS_NONE 0xffffu

typedef struct {
    const unsigned char *in;
    size_t size, at;
    unsigned bits;  /* next bits, most significant first */
    int extra;      /* valid bits in `bits` beyond 16 */
} XpressReader;

static unsigned short xpress_table[1u << XPRESS_TABLE_BITS];
static unsigned char xpress_lengths[XPRESS_SYMBOLS];

static unsigned xpress_word(XpressReader *r) {
    unsigned v = 0;
    if (r->at < r->size) {
        v = r->in[r->at];
    }
    if (r->at + 1 < r->size) {
        v |= (unsigned)r->in[r->at + 1] << 8;
    }
    r->at += 2; /* past the end reads zeros: the output size ends the stream */
    return v;
}

static void xpress_take(XpressReader *r, int n) {
    if (n <= 0) {
        return;
    }
    r->bits <<= n;
    r->extra -= n;
    if (r->extra < 0) {
        r->bits |= xpress_word(r) << -r->extra;
        r->extra += 16;
    }
}

/* Canonical code from the 512 lengths: by length, then by symbol. */
static int xpress_build(const unsigned char *lengths) {
    for (unsigned i = 0; i < (1u << XPRESS_TABLE_BITS); ++i) {
        xpress_table[i] = XPRESS_NONE;
    }
    for (unsigned i = 0; i < XPRESS_SYMBOLS; ++i) {
        xpress_lengths[i] = (unsigned char)((lengths[i / 2] >> (4 * (i & 1))) & 15);
    }
    unsigned code = 0;
    for (unsigned length = 1; length <= XPRESS_TABLE_BITS; ++length) {
        for (unsigned symbol = 0; symbol < XPRESS_SYMBOLS; ++symbol) {
            if (xpress_lengths[symbol] != length) {
                continue;
            }
            unsigned first = code << (XPRESS_TABLE_BITS - length), count = 1u << (XPRESS_TABLE_BITS - length);
            if (first + count > (1u << XPRESS_TABLE_BITS)) {
                return 0; /* over-subscribed */
            }
            for (unsigned i = 0; i < count; ++i) {
                xpress_table[first + i] = (unsigned short)symbol;
            }
            ++code;
        }
        code <<= 1;
    }
    return 1;
}

int codec_xpress_huffman_decode(const unsigned char *in, size_t in_size, unsigned char *out, size_t out_size) {
    XpressReader r = {in, in_size, 0, 0, 0};
    size_t done = 0;
    while (done < out_size) {
        if (r.at + 256 > in_size || !xpress_build(in + r.at)) {
            return 0;
        }
        r.at += 256;
        r.bits = xpress_word(&r) << 16;
        r.bits |= xpress_word(&r);
        r.extra = 16;
        for (size_t block = 0; block < XPRESS_BLOCK && done < out_size;) {
            unsigned symbol = xpress_table[r.bits >> (32 - XPRESS_TABLE_BITS)];
            if (symbol == XPRESS_NONE) {
                return 0;
            }
            xpress_take(&r, xpress_lengths[symbol]);
            if (symbol < 256) {
                out[done++] = (unsigned char)symbol;
                ++block;
                continue;
            }
            symbol -= 256;
            size_t length = symbol & 15;
            int offset_bits = (int)(symbol >> 4);
            if (length == 15) {
                if (r.at >= in_size) {
                    return 0;
                }
                length = in[r.at++];
                if (length == 255) {
                    if (r.at + 2 > in_size) {
                        return 0;
                    }
                    length = (size_t)in[r.at] | (size_t)in[r.at + 1] << 8;
                    r.at += 2;
                    if (length == 0) {
                        if (r.at + 4 > in_size) {
                            return 0;
                        }
                        length = (size_t)in[r.at] | (size_t)in[r.at + 1] << 8 | (size_t)in[r.at + 2] << 16 |
                                 (size_t)in[r.at + 3] << 24;
                        r.at += 4;
                    }
                    if (length < 15) {
                        return 0;
                    }
                    length -= 15;
                }
                length += 15;
            }
            length += 3;
            size_t offset = (size_t)1 << offset_bits;
            if (offset_bits) {
                offset += r.bits >> (32 - offset_bits);
                xpress_take(&r, offset_bits);
            }
            if (offset > done || length > out_size - done) {
                return 0;
            }
            for (size_t i = 0; i < length; ++i, ++done) {
                out[done] = out[done - offset];
            }
            block += length;
        }
    }
    return 1;
}

/* ---------------------------------------------------------------- method 2 */

#define LZ_MIN_MATCH 4
#define LZ_HASH_BITS 16

static int lz_put(unsigned char *out, size_t cap, size_t *at, size_t value) {
    do {
        if (*at >= cap) {
            return 0;
        }
        unsigned char byte = (unsigned char)(value & 127);
        value >>= 7;
        out[(*at)++] = (unsigned char)(byte | (value ? 128 : 0));
    } while (value);
    return 1;
}

static int lz_get(const unsigned char *in, size_t size, size_t *at, size_t *value) {
    size_t v = 0;
    for (int shift = 0; shift < 35; shift += 7) {
        if (*at >= size) {
            return 0;
        }
        unsigned char byte = in[(*at)++];
        v |= (size_t)(byte & 127) << shift;
        if (!(byte & 128)) {
            *value = v;
            return 1;
        }
    }
    return 0;
}

static unsigned lz_read32(const unsigned char *p) {
    return (unsigned)p[0] | (unsigned)p[1] << 8 | (unsigned)p[2] << 16 | (unsigned)p[3] << 24;
}

static unsigned lz_hash(const unsigned char *p) {
    return (lz_read32(p) * 2654435761u) >> (32 - LZ_HASH_BITS);
}

static int lz_literals(const unsigned char *in, size_t from, size_t to, unsigned char *out, size_t cap, size_t *at) {
    if (!lz_put(out, cap, at, to - from) || to - from > cap - *at) {
        return 0;
    }
    for (size_t i = from; i < to; ++i) {
        out[(*at)++] = in[i];
    }
    return 1;
}

size_t codec_lz_encode(const unsigned char *in, size_t size, unsigned char *out, size_t cap, void *work) {
    unsigned *table = work; /* position + 1 of the last 4 bytes with each hash, 0 = none */
    if (!size) {
        return 0; /* stored instead */
    }
    for (size_t i = 0; i < ((size_t)1 << LZ_HASH_BITS); ++i) {
        table[i] = 0;
    }
    size_t at = 0, anchor = 0, i = 0;
    while (i + LZ_MIN_MATCH <= size) {
        unsigned h = lz_hash(in + i);
        size_t candidate = table[h];
        table[h] = (unsigned)(i + 1);
        if (!candidate || lz_read32(in + candidate - 1) != lz_read32(in + i)) {
            ++i;
            continue;
        }
        size_t from = candidate - 1, length = LZ_MIN_MATCH;
        while (i + length < size && in[from + length] == in[i + length]) {
            ++length;
        }
        if (!lz_literals(in, anchor, i, out, cap, &at) || !lz_put(out, cap, &at, length - LZ_MIN_MATCH) ||
            !lz_put(out, cap, &at, i - from - 1)) {
            return 0;
        }
        /* remember a few positions inside the match (all of a short one) */
        size_t end = i + length;
        for (size_t p = i + 1; p + LZ_MIN_MATCH <= size && p < end && p < i + 16; ++p) {
            table[lz_hash(in + p)] = (unsigned)(p + 1);
        }
        if (end - 1 > i + 16 && end - 1 + LZ_MIN_MATCH <= size) {
            table[lz_hash(in + end - 1)] = (unsigned)end;
        }
        i = anchor = end;
    }
    /* the decoder stops once the output is complete: no empty run after a final match */
    if (anchor < size && !lz_literals(in, anchor, size, out, cap, &at)) {
        return 0;
    }
    return at;
}

int codec_lz_decode(const unsigned char *in, size_t in_size, unsigned char *out, size_t out_size) {
    size_t at = 0, done = 0, n = 0;
    while (done < out_size) {
        if (!lz_get(in, in_size, &at, &n) || n > out_size - done || n > in_size - at) {
            return 0;
        }
        for (size_t i = 0; i < n; ++i) {
            out[done++] = in[at++];
        }
        if (done == out_size) {
            break;
        }
        size_t length = 0, offset = 0;
        if (!lz_get(in, in_size, &at, &length) || !lz_get(in, in_size, &at, &offset)) {
            return 0;
        }
        length += LZ_MIN_MATCH;
        offset += 1;
        if (offset > done || length > out_size - done) {
            return 0;
        }
        for (size_t i = 0; i < length; ++i, ++done) {
            out[done] = out[done - offset];
        }
    }
    return at == in_size;
}
