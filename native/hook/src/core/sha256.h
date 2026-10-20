/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md. */
#ifndef FI_SHA256_H
#define FI_SHA256_H
#include "infinity_base.h"

typedef struct {
    uint32_t h[8];
    uint64_t length;
    unsigned char block[64];
    unsigned used;
} Sha256Context;

void sha256_initialize(Sha256Context *context);
void sha256_update(Sha256Context *context, const unsigned char *data, size_t size);
void sha256_finalize(Sha256Context *context, unsigned char digest[32]);
#endif
