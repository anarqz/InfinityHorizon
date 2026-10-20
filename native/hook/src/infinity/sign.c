/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* Ed25519 verification (RFC 8032), for the self-update's release signature.
   The field and group arithmetic is the verify path of TweetNaCl (Bernstein, van Gastel,
   Janssen, Lange, Schwabe, Smetsers; public domain), with:
   - an incremental SHA-512 (so R || A || M is hashed without a copy of the message),
   - S >= L rejected (RFC 8032 section 5.1.7), which TweetNaCl does not check,
   - left shifts of negative values written as multiplications.
   It checks public data only, so it needs no constant time. Stack use stays under ~5 KB. */
#include "sign.h"

_Static_assert(sizeof(AZIMUTH_SIGN_PREFIX) - 1 + 64 == AZIMUTH_SIGN_LINE, "the update line is 84 bytes");

/* The release key. Made once with OpenSSL 3:
       openssl genpkey -algorithm ED25519 -out ~/.config/forzainfinity/release-ed25519.pem
       openssl pkey -in ~/.config/forzainfinity/release-ed25519.pem -pubout -outform DER | tail -c 32
   The private key lives only on the WINDOWS machine, in
   %USERPROFILE%\.config\forzainfinity\release-ed25519.pem (OD-49, moved there from SAURON on
   2026-10-19; never in a repository); apps/launcher-win/tools/HookSign (release-sign) signs with it. */
const unsigned char azimuth_sign_release_key[32] = {
    0xbf, 0xe5, 0x72, 0xbc, 0x40, 0x36, 0x71, 0x2e, 0x5a, 0x2f, 0x96, 0x83, 0xc6, 0xb9, 0xfe, 0x29,
    0x3d, 0xce, 0x19, 0x62, 0x74, 0xe0, 0x8b, 0x10, 0x83, 0x0e, 0xf0, 0x23, 0x28, 0x60, 0xae, 0xfb};

/* ---- SHA-512 ---- */

typedef struct {
    uint64_t h[8];
    uint64_t length;
    unsigned char block[128];
    unsigned used;
} EdSha512;

static const uint64_t ED_SHA512_K[80] = {
    0x428a2f98d728ae22ull, 0x7137449123ef65cdull, 0xb5c0fbcfec4d3b2full, 0xe9b5dba58189dbbcull,
    0x3956c25bf348b538ull, 0x59f111f1b605d019ull, 0x923f82a4af194f9bull, 0xab1c5ed5da6d8118ull,
    0xd807aa98a3030242ull, 0x12835b0145706fbeull, 0x243185be4ee4b28cull, 0x550c7dc3d5ffb4e2ull,
    0x72be5d74f27b896full, 0x80deb1fe3b1696b1ull, 0x9bdc06a725c71235ull, 0xc19bf174cf692694ull,
    0xe49b69c19ef14ad2ull, 0xefbe4786384f25e3ull, 0x0fc19dc68b8cd5b5ull, 0x240ca1cc77ac9c65ull,
    0x2de92c6f592b0275ull, 0x4a7484aa6ea6e483ull, 0x5cb0a9dcbd41fbd4ull, 0x76f988da831153b5ull,
    0x983e5152ee66dfabull, 0xa831c66d2db43210ull, 0xb00327c898fb213full, 0xbf597fc7beef0ee4ull,
    0xc6e00bf33da88fc2ull, 0xd5a79147930aa725ull, 0x06ca6351e003826full, 0x142929670a0e6e70ull,
    0x27b70a8546d22ffcull, 0x2e1b21385c26c926ull, 0x4d2c6dfc5ac42aedull, 0x53380d139d95b3dfull,
    0x650a73548baf63deull, 0x766a0abb3c77b2a8ull, 0x81c2c92e47edaee6ull, 0x92722c851482353bull,
    0xa2bfe8a14cf10364ull, 0xa81a664bbc423001ull, 0xc24b8b70d0f89791ull, 0xc76c51a30654be30ull,
    0xd192e819d6ef5218ull, 0xd69906245565a910ull, 0xf40e35855771202aull, 0x106aa07032bbd1b8ull,
    0x19a4c116b8d2d0c8ull, 0x1e376c085141ab53ull, 0x2748774cdf8eeb99ull, 0x34b0bcb5e19b48a8ull,
    0x391c0cb3c5c95a63ull, 0x4ed8aa4ae3418acbull, 0x5b9cca4f7763e373ull, 0x682e6ff3d6b2b8a3ull,
    0x748f82ee5defb2fcull, 0x78a5636f43172f60ull, 0x84c87814a1f0ab72ull, 0x8cc702081a6439ecull,
    0x90befffa23631e28ull, 0xa4506cebde82bde9ull, 0xbef9a3f7b2c67915ull, 0xc67178f2e372532bull,
    0xca273eceea26619cull, 0xd186b8c721c0c207ull, 0xeada7dd6cde0eb1eull, 0xf57d4f7fee6ed178ull,
    0x06f067aa72176fbaull, 0x0a637dc5a2c898a6ull, 0x113f9804bef90daeull, 0x1b710b35131c471bull,
    0x28db77f523047d84ull, 0x32caab7b40c72493ull, 0x3c9ebe0a15c9bebcull, 0x431d67c49c100d4cull,
    0x4cc5d4becb3e42b6ull, 0x597f299cfc657e2aull, 0x5fcb6fab3ad6faecull, 0x6c44198c4a475817ull};

static const uint64_t ED_SHA512_IV[8] = {0x6a09e667f3bcc908ull, 0xbb67ae8584caa73bull, 0x3c6ef372fe94f82bull,
                                         0xa54ff53a5f1d36f1ull, 0x510e527fade682d1ull, 0x9b05688c2b3e6c1full,
                                         0x1f83d9abfb41bd6bull, 0x5be0cd19137e2179ull};

static uint64_t ed_ror(uint64_t x, unsigned n) {
    return (x >> n) | (x << (64 - n));
}

static void ed_sha512_block(EdSha512 *s) {
    uint64_t w[80], v[8];
    for (int i = 0; i < 16; ++i) {
        uint64_t x = 0;
        for (int j = 0; j < 8; ++j) {
            x = (x << 8) | s->block[8 * i + j];
        }
        w[i] = x;
    }
    for (int i = 16; i < 80; ++i) {
        uint64_t s0 = ed_ror(w[i - 15], 1) ^ ed_ror(w[i - 15], 8) ^ (w[i - 15] >> 7);
        uint64_t s1 = ed_ror(w[i - 2], 19) ^ ed_ror(w[i - 2], 61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    for (int i = 0; i < 8; ++i) {
        v[i] = s->h[i];
    }
    for (int i = 0; i < 80; ++i) {
        uint64_t e = v[4], a = v[0];
        uint64_t t1 = v[7] + (ed_ror(e, 14) ^ ed_ror(e, 18) ^ ed_ror(e, 41)) + ((e & v[5]) ^ (~e & v[6])) +
                      ED_SHA512_K[i] + w[i];
        uint64_t t2 = (ed_ror(a, 28) ^ ed_ror(a, 34) ^ ed_ror(a, 39)) + ((a & v[1]) ^ (a & v[2]) ^ (v[1] & v[2]));
        v[7] = v[6];
        v[6] = v[5];
        v[5] = v[4];
        v[4] = v[3] + t1;
        v[3] = v[2];
        v[2] = v[1];
        v[1] = v[0];
        v[0] = t1 + t2;
    }
    for (int i = 0; i < 8; ++i) {
        s->h[i] += v[i];
    }
}

static void ed_sha512_init(EdSha512 *s) {
    for (int i = 0; i < 8; ++i) {
        s->h[i] = ED_SHA512_IV[i];
    }
    s->length = 0;
    s->used = 0;
}

static void ed_sha512_update(EdSha512 *s, const unsigned char *data, size_t size) {
    s->length += size;
    while (size) {
        s->block[s->used++] = *data++;
        --size;
        if (s->used == 128) {
            ed_sha512_block(s);
            s->used = 0;
        }
    }
}

static void ed_sha512_final(EdSha512 *s, unsigned char out[64]) {
    uint64_t bits = s->length * 8;
    s->block[s->used++] = 0x80;
    if (s->used > 112) {
        while (s->used < 128) {
            s->block[s->used++] = 0;
        }
        ed_sha512_block(s);
        s->used = 0;
    }
    while (s->used < 120) {
        s->block[s->used++] = 0;
    }
    for (int i = 0; i < 8; ++i) {
        s->block[120 + i] = (unsigned char)(bits >> (56 - 8 * i));
    }
    ed_sha512_block(s);
    for (int i = 0; i < 64; ++i) {
        out[i] = (unsigned char)(s->h[i / 8] >> (56 - 8 * (i % 8)));
    }
}

/* ---- GF(2^255 - 19), 16 limbs of 16 bits (TweetNaCl) ---- */

typedef int64_t ed_gf[16];

static const ed_gf ED_GF0 = {0};
static const ed_gf ED_GF1 = {1};
static const ed_gf ED_D = {0x78a3, 0x1359, 0x4dca, 0x75eb, 0xd8ab, 0x4141, 0x0a4d, 0x0070,
                           0xe898, 0x7779, 0x4079, 0x8cc7, 0xfe73, 0x2b6f, 0x6cee, 0x5203};
static const ed_gf ED_D2 = {0xf159, 0x26b2, 0x9b94, 0xebd6, 0xb156, 0x8283, 0x149a, 0x00e0,
                            0xd130, 0xeef3, 0x80f2, 0x198e, 0xfce7, 0x56df, 0xd9dc, 0x2406};
static const ed_gf ED_X = {0xd51a, 0x8f25, 0x2d60, 0xc956, 0xa7b2, 0x9525, 0xc760, 0x692c,
                           0xdc5c, 0xfdd6, 0xe231, 0xc0a4, 0x53fe, 0xcd6e, 0x36d3, 0x2169};
static const ed_gf ED_Y = {0x6658, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666,
                           0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666};
static const ed_gf ED_I = {0xa0b0, 0x4a0e, 0x1b27, 0xc4ee, 0xe478, 0xad2f, 0x1806, 0x2f43,
                           0xd7a7, 0x3dfb, 0x0099, 0x2b4d, 0xdf0b, 0x4fc1, 0x2480, 0x2b83};
/* the group order L, little-endian */
static const int64_t ED_L[32] = {0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7,
                                 0xa2, 0xde, 0xf9, 0xde, 0x14, 0,    0,    0,    0,    0,    0,
                                 0,    0,    0,    0,    0,    0,    0,    0,    0,    0x10};

static int ed_bytes_equal(const unsigned char *a, const unsigned char *b, size_t n) {
    unsigned char d = 0;
    for (size_t i = 0; i < n; ++i) {
        d |= a[i] ^ b[i];
    }
    return d == 0;
}

static void ed_set(ed_gf r, const ed_gf a) {
    for (int i = 0; i < 16; ++i) {
        r[i] = a[i];
    }
}

static void ed_carry(ed_gf o) {
    for (int i = 0; i < 16; ++i) {
        o[i] += 65536;
        int64_t c = o[i] >> 16;
        o[(i + 1) * (i < 15)] += c - 1 + 37 * (c - 1) * (i == 15);
        o[i] -= c * 65536;
    }
}

static void ed_select(ed_gf p, ed_gf q, int b) {
    int64_t c = ~(int64_t)(b - 1);
    for (int i = 0; i < 16; ++i) {
        int64_t t = c & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

static void ed_pack25519(unsigned char *o, const ed_gf n) {
    ed_gf m, t;
    ed_set(t, n);
    ed_carry(t);
    ed_carry(t);
    ed_carry(t);
    for (int j = 0; j < 2; ++j) {
        m[0] = t[0] - 0xffed;
        for (int i = 1; i < 15; ++i) {
            m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
            m[i - 1] &= 0xffff;
        }
        m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
        int b = (int)((m[15] >> 16) & 1);
        m[14] &= 0xffff;
        ed_select(t, m, 1 - b);
    }
    for (int i = 0; i < 16; ++i) {
        o[2 * i] = (unsigned char)(t[i] & 0xff);
        o[2 * i + 1] = (unsigned char)(t[i] >> 8);
    }
}

static int ed_neq25519(const ed_gf a, const ed_gf b) {
    unsigned char c[32], d[32];
    ed_pack25519(c, a);
    ed_pack25519(d, b);
    return !ed_bytes_equal(c, d, 32);
}

static unsigned char ed_par25519(const ed_gf a) {
    unsigned char d[32];
    ed_pack25519(d, a);
    return d[0] & 1;
}

static void ed_unpack25519(ed_gf o, const unsigned char *n) {
    for (int i = 0; i < 16; ++i) {
        o[i] = n[2 * i] + ((int64_t)n[2 * i + 1] << 8);
    }
    o[15] &= 0x7fff;
}

static void ed_fadd(ed_gf o, const ed_gf a, const ed_gf b) {
    for (int i = 0; i < 16; ++i) {
        o[i] = a[i] + b[i];
    }
}

static void ed_fsub(ed_gf o, const ed_gf a, const ed_gf b) {
    for (int i = 0; i < 16; ++i) {
        o[i] = a[i] - b[i];
    }
}

static void ed_fmul(ed_gf o, const ed_gf a, const ed_gf b) {
    int64_t t[31];
    for (int i = 0; i < 31; ++i) {
        t[i] = 0;
    }
    for (int i = 0; i < 16; ++i) {
        for (int j = 0; j < 16; ++j) {
            t[i + j] += a[i] * b[j];
        }
    }
    for (int i = 0; i < 15; ++i) {
        t[i] += 38 * t[i + 16];
    }
    for (int i = 0; i < 16; ++i) {
        o[i] = t[i];
    }
    ed_carry(o);
    ed_carry(o);
}

static void ed_fsquare(ed_gf o, const ed_gf a) {
    ed_fmul(o, a, a);
}

static void ed_inv25519(ed_gf o, const ed_gf i) {
    ed_gf c;
    ed_set(c, i);
    for (int a = 253; a >= 0; --a) {
        ed_fsquare(c, c);
        if (a != 2 && a != 4) {
            ed_fmul(c, c, i);
        }
    }
    ed_set(o, c);
}

static void ed_pow2523(ed_gf o, const ed_gf i) {
    ed_gf c;
    ed_set(c, i);
    for (int a = 250; a >= 0; --a) {
        ed_fsquare(c, c);
        if (a != 1) {
            ed_fmul(c, c, i);
        }
    }
    ed_set(o, c);
}

/* ---- the curve, extended coordinates (X, Y, Z, T) ---- */

static void ed_point_add(ed_gf p[4], ed_gf q[4]) {
    ed_gf a, b, c, d, t, e, f, g, h;
    ed_fsub(a, p[1], p[0]);
    ed_fsub(t, q[1], q[0]);
    ed_fmul(a, a, t);
    ed_fadd(b, p[0], p[1]);
    ed_fadd(t, q[0], q[1]);
    ed_fmul(b, b, t);
    ed_fmul(c, p[3], q[3]);
    ed_fmul(c, c, ED_D2);
    ed_fmul(d, p[2], q[2]);
    ed_fadd(d, d, d);
    ed_fsub(e, b, a);
    ed_fsub(f, d, c);
    ed_fadd(g, d, c);
    ed_fadd(h, b, a);
    ed_fmul(p[0], e, f);
    ed_fmul(p[1], h, g);
    ed_fmul(p[2], g, f);
    ed_fmul(p[3], e, h);
}

static void ed_point_swap(ed_gf p[4], ed_gf q[4], int b) {
    for (int i = 0; i < 4; ++i) {
        ed_select(p[i], q[i], b);
    }
}

static void ed_point_pack(unsigned char *r, ed_gf p[4]) {
    ed_gf tx, ty, zi;
    ed_inv25519(zi, p[2]);
    ed_fmul(tx, p[0], zi);
    ed_fmul(ty, p[1], zi);
    ed_pack25519(r, ty);
    r[31] ^= (unsigned char)(ed_par25519(tx) << 7);
}

/* p = s * q (q is used up) */
static void ed_scalarmult(ed_gf p[4], ed_gf q[4], const unsigned char *s) {
    ed_set(p[0], ED_GF0);
    ed_set(p[1], ED_GF1);
    ed_set(p[2], ED_GF1);
    ed_set(p[3], ED_GF0);
    for (int i = 255; i >= 0; --i) {
        int b = (s[i / 8] >> (i & 7)) & 1;
        ed_point_swap(p, q, b);
        ed_point_add(q, p);
        ed_point_add(p, p);
        ed_point_swap(p, q, b);
    }
}

static void ed_scalarbase(ed_gf p[4], const unsigned char *s) {
    ed_gf q[4];
    ed_set(q[0], ED_X);
    ed_set(q[1], ED_Y);
    ed_set(q[2], ED_GF1);
    ed_fmul(q[3], ED_X, ED_Y);
    ed_scalarmult(p, q, s);
}

/* r = -(the point encoded in p); -1 when p is not on the curve */
static int ed_unpackneg(ed_gf r[4], const unsigned char p[32]) {
    ed_gf t, chk, num, den, den2, den4, den6;
    ed_set(r[2], ED_GF1);
    ed_unpack25519(r[1], p);
    ed_fsquare(num, r[1]);
    ed_fmul(den, num, ED_D);
    ed_fsub(num, num, r[2]);
    ed_fadd(den, r[2], den);
    ed_fsquare(den2, den);
    ed_fsquare(den4, den2);
    ed_fmul(den6, den4, den2);
    ed_fmul(t, den6, num);
    ed_fmul(t, t, den);
    ed_pow2523(t, t);
    ed_fmul(t, t, num);
    ed_fmul(t, t, den);
    ed_fmul(t, t, den);
    ed_fmul(r[0], t, den);
    ed_fsquare(chk, r[0]);
    ed_fmul(chk, chk, den);
    if (ed_neq25519(chk, num)) {
        ed_fmul(r[0], r[0], ED_I);
    }
    ed_fsquare(chk, r[0]);
    ed_fmul(chk, chk, den);
    if (ed_neq25519(chk, num)) {
        return -1;
    }
    if (ed_par25519(r[0]) == (p[31] >> 7)) {
        ed_fsub(r[0], ED_GF0, r[0]);
    }
    ed_fmul(r[3], r[0], r[1]);
    return 0;
}

/* ---- scalars mod L ---- */

static void ed_mod_l(unsigned char *r, int64_t x[64]) {
    int64_t carry;
    int i, j;
    for (i = 63; i >= 32; --i) {
        carry = 0;
        for (j = i - 32; j < i - 12; ++j) {
            x[j] += carry - 16 * x[i] * ED_L[j - (i - 32)];
            carry = (x[j] + 128) >> 8;
            x[j] -= carry * 256;
        }
        x[j] += carry;
        x[i] = 0;
    }
    carry = 0;
    for (j = 0; j < 32; ++j) {
        x[j] += carry - (x[31] >> 4) * ED_L[j];
        carry = x[j] >> 8;
        x[j] &= 255;
    }
    for (j = 0; j < 32; ++j) {
        x[j] -= carry * ED_L[j];
    }
    for (i = 0; i < 32; ++i) {
        x[i + 1] += x[i] >> 8;
        r[i] = (unsigned char)(x[i] & 255);
    }
}

/* r (64 bytes) = r mod L, in its first 32 bytes */
static void ed_reduce(unsigned char *r) {
    int64_t x[64];
    for (int i = 0; i < 64; ++i) {
        x[i] = r[i];
    }
    for (int i = 0; i < 64; ++i) {
        r[i] = 0;
    }
    ed_mod_l(r, x);
}

/* 1 when the 32-byte little-endian scalar s is below L */
static int ed_scalar_canonical(const unsigned char *s) {
    for (int i = 31; i >= 0; --i) {
        if (s[i] < ED_L[i]) {
            return 1;
        }
        if (s[i] > ED_L[i]) {
            return 0;
        }
    }
    return 0;
}

/* RFC 8032 verification of `signature` over `prefix` || `message` by `key` (the prefix may be
   empty): a two-part message is hashed without a copy. */
static int ed_verify_parts(const unsigned char signature[64], const unsigned char *prefix, size_t prefix_size,
                           const unsigned char *message, size_t size, const unsigned char key[32]) {
    ed_gf p[4], q[4];
    unsigned char h[64], t[32];
    EdSha512 hash;
    if (!ed_scalar_canonical(signature + 32) || ed_unpackneg(q, key)) {
        return 0;
    }
    ed_sha512_init(&hash);
    ed_sha512_update(&hash, signature, 32);
    ed_sha512_update(&hash, key, 32);
    ed_sha512_update(&hash, prefix, prefix_size);
    ed_sha512_update(&hash, message, size);
    ed_sha512_final(&hash, h);
    ed_reduce(h);
    ed_scalarmult(p, q, h);
    ed_scalarbase(q, signature + 32);
    ed_point_add(p, q);
    ed_point_pack(t, p);
    return ed_bytes_equal(signature, t, 32);
}

int azimuth_sign_verify(const unsigned char signature[64], const unsigned char *message, size_t size,
                        const unsigned char key[32]) {
    return ed_verify_parts(signature, message, 0, message, size, key);
}

/* The launch-ticket keys, by kid (ACCOUNTS.md section 4: lt-<year>-<month>, the current and the
   next). The production public keys come from the server's launch-ticket-signing-key (Cloud KMS
   or Secret Manager, ONLINE_OFFLINE.md T11) when it exists: until then no release build takes a
   ticket, and a launcher start plays offline with "session refused". A development build
   (build.py --dev, FI_DEV) takes the local stack's well-known key instead (compose
   FI_LAUNCH_TICKET_SIGNING_KEYS lt-local-1, .env.example), which must never be in a release:
   anyone has its seed. The development build accepts only the local development public key. */
AzimuthLaunchKey azimuth_sign_launch_keys[AZIMUTH_LAUNCH_KEYS] = {
#ifdef FI_DEV
    {"lt-local-1", 1, {0x1f, 0xb7, 0xb2, 0x6e, 0xfc, 0x09, 0xb4, 0x08, 0xd2, 0xe6, 0x7f, 0xac, 0xc9, 0x6c, 0xf4, 0xfe,
                       0x37, 0xb2, 0x09, 0x75, 0xaa, 0xae, 0xa0, 0x59, 0xab, 0xee, 0x40, 0x8b, 0x8c, 0xfd, 0xc4, 0x10}},
#else
    {"lt-2026-10", 0, {0}},
#endif
    {"lt-2026-11", 0, {0}},
};

const AzimuthLaunchKey *azimuth_sign_launch_key(const char *kid, size_t size) {
    for (int i = 0; i < AZIMUTH_LAUNCH_KEYS; ++i) {
        const AzimuthLaunchKey *k = &azimuth_sign_launch_keys[i];
        size_t n = 0;
        while (n < sizeof(k->kid) && k->kid[n]) {
            ++n;
        }
        if (n && n == size && ed_bytes_equal((const unsigned char *)k->kid, (const unsigned char *)kid, n) == 1) {
            return k;
        }
    }
    return 0;
}

int azimuth_sign_launch_ok(const unsigned char signature[64], const unsigned char *claims, size_t size,
                           const unsigned char key[32]) {
    static const char prefix[] = AZIMUTH_LAUNCH_PREFIX;
    return ed_verify_parts(signature, (const unsigned char *)prefix, sizeof(prefix) - 1, claims, size, key);
}

int azimuth_sign_update_ok(const unsigned char signature[64], const unsigned char digest[32],
                           const unsigned char key[32]) {
    static const char prefix[] = AZIMUTH_SIGN_PREFIX;
    static const char hex[] = "0123456789abcdef";
    unsigned char line[AZIMUTH_SIGN_LINE];
    size_t n = sizeof(prefix) - 1;
    for (size_t i = 0; i < n; ++i) {
        line[i] = (unsigned char)prefix[i];
    }
    for (int i = 0; i < 32; ++i) {
        line[n + 2 * i] = (unsigned char)hex[digest[i] >> 4];
        line[n + 2 * i + 1] = (unsigned char)hex[digest[i] & 15];
    }
    return azimuth_sign_verify(signature, line, sizeof(line), key);
}
