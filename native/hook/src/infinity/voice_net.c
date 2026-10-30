/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   Voice chat's transport (OD-147, VC-2; docs/launcher/VOICE_CHAT.md 3.1, 12.3 and 13 in the
   monorepo; the relay's side is Infinity.Core/WorldServer/VoiceCodec.cs). The overlay builds
   whole voice side-channel datagrams, STUN indications (big-endian, magic 0x2112A442, at most 548
   bytes) of kind 0x3E93 (voice, attribute 0xC0F3 up / 0xC0F4 down) or 0x3E94 (control up 0xC0F5 /
   state down 0xC0F6), and hands them to FiOverlayHost.voice_send. This file only checks their
   shape and moves them, exactly as car state (carstate.c, kind 0x3E91) travels:

   - sent from the socket the world's relay knows this player by: the host's loopback socket of
     carstate.c to its own relay; a joiner's proxy socket to the world (azimuth_proxy_send_world,
     FIR1 data through a fallback relay, and only once the hook's kind is proved under FIUP); a
     world on this network the game reached directly, the hook's own LAN socket (livery_world.c);
   - received on those same sockets, with the source checks car state has there (proxy.c: from
     the world, the hook's kind proved; carstate.c: the host's loopback socket, from its relay),
     and handed to the overlay's optional export fi_overlay_voice. Nothing else goes there, and
     without that export (an older overlay) voice is dropped.

   Any thread, never waits: one sendto on a UDP socket (as car state does from the game's
   threads), after a token bucket of 60 datagrams a second (20 at once). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define VN_VOICE_KIND 0x3E93u
#define VN_CONTROL_KIND 0x3E94u
#define VN_VOICE_UP 0xC0F3u
#define VN_VOICE_DOWN 0xC0F4u
#define VN_CONTROL_UP 0xC0F5u
#define VN_STATE_DOWN 0xC0F6u
#define VN_MAX 548u
#define VN_RATE_PER_S 60u
#define VN_RATE_BURST 20u

/* FI_VOICE_* of fi_overlay.h (FiOverlayHost.voice_send). VN_E_RATE is this hook's own: the overlay
   takes any value < 0 as "dropped". */
#define VN_SENT 0
#define VN_E_NO_WORLD (-1)
#define VN_E_SHAPE (-2)
#define VN_E_OFF (-3)
#define VN_E_RATE (-4)

static unsigned vn_be16(const unsigned char *p) { return (unsigned)(p[0] << 8 | p[1]); }

/* A well-formed STUN message of `kind` (no FINGERPRINT or integrity needed) that carries
   `attribute`: as StunCodec.IsStun and TryAttribute read it. */
int fi_voice_shape(const unsigned char *d, size_t n, unsigned kind, unsigned attribute) {
    if (!d || n < 20 || n > VN_MAX || (n & 3) || (d[0] & 0xc0) || vn_be16(d) != kind || d[4] != 0x21 ||
        d[5] != 0x12 || d[6] != 0xa4 || d[7] != 0x42 || (size_t)vn_be16(d + 2) + 20 != n) {
        return 0;
    }
    for (size_t at = 20; at + 4 <= n;) {
        unsigned tag = vn_be16(d + at), count = vn_be16(d + at + 2);
        if (count > n - at - 4) return 0;
        if (tag == attribute) return 1;
        at += 4 + ((count + 3u) & ~3u);
    }
    return 0;
}

/* 1: an up datagram the overlay may send (voice 0x3E93/0xC0F3, control 0x3E94/0xC0F5). */
static int vn_up(const unsigned char *d, size_t n) {
    return fi_voice_shape(d, n, VN_VOICE_KIND, VN_VOICE_UP) || fi_voice_shape(d, n, VN_CONTROL_KIND, VN_CONTROL_UP);
}

/* 1: a down datagram for the overlay (voice 0x3E93/0xC0F4, state 0x3E94/0xC0F6). */
static int vn_down(const unsigned char *d, size_t n) {
    return fi_voice_shape(d, n, VN_VOICE_KIND, VN_VOICE_DOWN) || fi_voice_shape(d, n, VN_CONTROL_KIND, VN_STATE_DOWN);
}

/* ---------------------------------------------------------------------- rate cap */

static volatile LONG vn_rate_busy; /* a try-lock: a second caller at the same moment gives up */
static unsigned long long vn_rate_at;  /* ms of the last refill */
static unsigned vn_rate_milli = VN_RATE_BURST * 1000u; /* tokens x 1000 */

/* 1 when a datagram may go at `now` (ms). A caller that would wait gives up instead. */
static int vn_rate_take(unsigned long long now) {
    if (InterlockedCompareExchange(&vn_rate_busy, 1, 0) != 0) return 0;
    if (vn_rate_at) {
        unsigned long long elapsed = now - vn_rate_at;
        unsigned long long refill = elapsed * VN_RATE_PER_S; /* tokens x 1000 */
        unsigned long long tokens = vn_rate_milli + (elapsed > 100000ull ? VN_RATE_BURST * 1000ull : refill);
        vn_rate_milli = tokens > VN_RATE_BURST * 1000ull ? VN_RATE_BURST * 1000u : (unsigned)tokens;
    }
    vn_rate_at = now;
    int ok = vn_rate_milli >= 1000u;
    if (ok) vn_rate_milli -= 1000u;
    InterlockedExchange(&vn_rate_busy, 0);
    return ok;
}

/* ------------------------------------------------------------------------ send */

/* FiOverlayHost.voice_send (launch.c). */
int fi_voice_send(const uint8_t *datagram, uint32_t size) {
    if (!vn_up(datagram, size)) return VN_E_SHAPE;
    if (fi_cfg()->launch == FI_LAUNCH_OFFLINE || !fi_launch_network_allowed()) return VN_E_OFF;
    int hosting = azimuth_world_hosting();
    int joined = azimuth_proxy_world_ready() || azimuth_world_lan_ready();
    if (!hosting && !joined) {
        /* FreeRoamDirect (outside FIUP) leaves the game on its own socket: no hook socket the relay knows */
        return azimuth_proxy_passthrough() ? VN_E_OFF : VN_E_NO_WORLD;
    }
    if (!vn_rate_take(GetTickCount64())) return VN_E_RATE;
    int sent = hosting ? azimuth_carstate_host_send(datagram, (int)size)
                       : azimuth_proxy_send_world(datagram, (int)size) || azimuth_world_lan_send(datagram, (int)size);
    return sent ? VN_SENT : VN_E_NO_WORLD;
}

/* --------------------------------------------------------------------- receive */

typedef void (*VnOverlayVoiceFn)(const uint8_t *datagram, uint32_t size);
static VnOverlayVoiceFn volatile vn_overlay_voice;
static volatile LONG vn_resolved;

static VnOverlayVoiceFn vn_overlay(void) {
    if (!vn_resolved) {
        if (!fi_launch_overlay_export("fi_overlay_start")) return 0; /* not started (yet) */
        vn_overlay_voice = (VnOverlayVoiceFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_voice");
        InterlockedExchange(&vn_resolved, 1);
    }
    return vn_overlay_voice;
}

/* A datagram that arrived from the world's relay on one of the sockets above, its source already
   checked by the caller: 1 when it is of a voice kind (0x3E93, 0x3E94), passed to the overlay
   when it is a well-formed down datagram and dropped otherwise; 0 for any other kind. */
int fi_voice_received(const unsigned char *d, int n) {
    if (!d || n < 2) return 0;
    unsigned kind = vn_be16(d);
    if (kind != VN_VOICE_KIND && kind != VN_CONTROL_KIND) return 0;
    if (!fi_launch_network_allowed() || !vn_down(d, (size_t)n)) return 1;
    VnOverlayVoiceFn voice = vn_overlay();
    if (voice) voice(d, (uint32_t)n);
    return 1;
}
