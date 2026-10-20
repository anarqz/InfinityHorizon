/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* Ed25519 signature check for the self-update (adapters/azimuth_update.c).
   A release carries azimuth-tls.dll.sig: the 64-byte raw Ed25519 signature, by the project's
   release key, of the 84-byte ASCII line
       "fh2online-update-v1 " + the DLL's SHA-256 in lowercase hex
   (no newline, nothing else). tools/sign_release.sh makes it. Freestanding: no Windows headers,
   so tests/sign_test.c builds it for the host too. */
#ifndef FH2_AZIMUTH_SIGN_H
#define FH2_AZIMUTH_SIGN_H
#include <stddef.h>
#include <stdint.h>

#define AZIMUTH_SIGN_PREFIX "fh2online-update-v1 "
#define AZIMUTH_SIGN_LINE 84

/* The release public key (raw 32 bytes). */
extern const unsigned char azimuth_sign_release_key[32];

/* 1 when `signature` is a valid Ed25519 signature of `message` (`size` bytes) by `key`
   (RFC 8032 Ed25519, S below the group order), 0 otherwise. */
int azimuth_sign_verify(const unsigned char signature[64], const unsigned char *message, size_t size,
                        const unsigned char key[32]);

/* 1 when `signature` signs the update line of `digest` (the DLL's SHA-256) by `key`. */
int azimuth_sign_update_ok(const unsigned char signature[64], const unsigned char digest[32],
                           const unsigned char key[32]);

/* Launch tickets (ONLINE_OFFLINE.md 3.2, fi_auth::ticket): Ed25519 by the server's
   launch-ticket key over "fi-launch-ticket-v1\n" || claims (the claims' raw bytes, no
   canonicalisation). The key is picked by the claims' kid; two overlap for 30 days (the current
   one and the next, ACCOUNTS.md section 4). */
#define AZIMUTH_LAUNCH_PREFIX "fi-launch-ticket-v1\n"
#define AZIMUTH_LAUNCH_KEYS 2
typedef struct {
    char kid[24];          /* "lt-2026-10"; empty: no key in this slot */
    int present;           /* 0: the kid is known, its public key is not in this build */
    unsigned char key[32]; /* raw Ed25519 public key */
} AzimuthLaunchKey;
extern AzimuthLaunchKey azimuth_sign_launch_keys[AZIMUTH_LAUNCH_KEYS];
/* The slot whose kid is `kid` (`size` bytes, not NUL-terminated), or 0. */
const AzimuthLaunchKey *azimuth_sign_launch_key(const char *kid, size_t size);
/* 1 when `signature` signs the launch ticket `claims` by `key`. */
int azimuth_sign_launch_ok(const unsigned char signature[64], const unsigned char *claims, size_t size,
                           const unsigned char key[32]);
#endif
