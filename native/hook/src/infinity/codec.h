/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
#ifndef FH2_AZIMUTH_CODEC_H
#define FH2_AZIMUTH_CODEC_H

#include <stddef.h>

/* Cloud save compression without ntdll: Wine (Proton) has no XPRESS formats and no
   RtlDecompressBufferEx, so the hook carries its own codecs. */

/* Bundle method 1: XPRESS Huffman (MS-XCA 2.2.4), what RtlCompressBuffer wrote on Windows.
   Decodes exactly `out_size` bytes; 1 on success. */
int codec_xpress_huffman_decode(const unsigned char *in, size_t in_size, unsigned char *out, size_t out_size);

/* Bundle method 2: LZ77 with varint lengths (see azimuth_codec.c). */
#define CODEC_LZ_WORK_SIZE (sizeof(unsigned) << 16)
/* Packed size, or 0 if it does not fit in `cap`. `work` holds CODEC_LZ_WORK_SIZE bytes. */
size_t codec_lz_encode(const unsigned char *in, size_t size, unsigned char *out, size_t cap, void *work);
/* Decodes exactly `out_size` bytes from all of `in`; 1 on success. */
int codec_lz_decode(const unsigned char *in, size_t in_size, unsigned char *out, size_t out_size);

#endif
