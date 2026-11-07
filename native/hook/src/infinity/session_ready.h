/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   W7b (OD-206, CR-S99 4, lobby spec 11.3-11.4, route 1): SessionReady (0x2a, launcher -> hook) names the host's private session
   an invited FM5 player joins: {u32 LE nonce, str scid, str templateName, str name}, each str a u16 LE length and 1 to 128
   bytes of [A-Za-z0-9._-] (the launcher's GET /launcher/v1/invites session, sent only after its POST /mm/v1/join {worldId} was
   granted). The hook answers SessionReadyResult (0x2b, hook -> launcher): {u32 LE nonce, u8 result, u32 LE HRESULT}.
   Header only and pure: launch.c parses with it, tests/session_ready_test.c checks it. */
#ifndef FI_SESSION_READY_H
#define FI_SESSION_READY_H

#define FI_SESSION_READY_PART 128u

/* SessionReadyResult's result. */
enum {
    FI_SESSION_READY_INVOKED = 0,      /* the kept GameSessionReady delegate was invoked and answered S_OK */
    FI_SESSION_READY_NO_DELEGATE = 1,  /* the game never added one (or removed it): nothing was invoked */
    FI_SESSION_READY_NO_REFERENCE = 2, /* the MultiplayerSessionReference could not be made: nothing was invoked */
    FI_SESSION_READY_FAILED = 3,       /* the delegate was invoked and answered a failure HRESULT */
    FI_SESSION_READY_RESULTS = 4
};

typedef struct {
    unsigned nonce;
    char scid[FI_SESSION_READY_PART + 1], template_name[FI_SESSION_READY_PART + 1], name[FI_SESSION_READY_PART + 1];
} FiSessionReadyRef;

static inline int fi_session_ready_char(unsigned char c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '_' || c == '.';
}

/* One str at `*at` (`*left` bytes remain) into `out`; both move past it. 0: short, empty, too long or another character. */
static inline int fi_session_ready_part(const unsigned char **at, size_t *left, char *out) {
    if (*left < 2) return 0;
    size_t n = (size_t)(*at)[0] | ((size_t)(*at)[1] << 8);
    if (!n || n > FI_SESSION_READY_PART || *left - 2 < n) return 0;
    for (size_t i = 0; i < n; ++i) {
        if (!fi_session_ready_char((*at)[2 + i])) return 0;
        out[i] = (char)(*at)[2 + i];
    }
    out[n] = 0;
    *at += 2 + n;
    *left -= 2 + n;
    return 1;
}

/* The payload after the type byte. 1: parsed into `out`, every byte used. 0: a protocol error (launch.c closes the pipe, as for
   any frame that does not parse). A nonce of 0 is refused: 0 means none on this pipe. */
static inline int fi_session_ready_parse(const unsigned char *payload, size_t size, FiSessionReadyRef *out) {
    if (!payload || size < 4 || !out) return 0;
    out->nonce = (unsigned)payload[0] | ((unsigned)payload[1] << 8) | ((unsigned)payload[2] << 16) | ((unsigned)payload[3] << 24);
    const unsigned char *at = payload + 4;
    size_t left = size - 4;
    return out->nonce && fi_session_ready_part(&at, &left, out->scid) && fi_session_ready_part(&at, &left, out->template_name) &&
           fi_session_ready_part(&at, &left, out->name) && !left;
}

/* SessionReadyResult's payload into `out` (9 bytes). */
static inline void fi_session_ready_result(unsigned char out[9], unsigned nonce, unsigned result, unsigned hresult) {
    for (unsigned i = 0; i < 4; ++i) {
        out[i] = (unsigned char)(nonce >> (8 * i));
        out[5 + i] = (unsigned char)(hresult >> (8 * i));
    }
    out[4] = (unsigned char)result;
}
#endif
