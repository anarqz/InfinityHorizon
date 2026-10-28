/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online free roam over the Internet: the joining player's side.

   The game talks NerdCore from its socket ([::]:5100) to the address the association gives it.
   For a world on another PC that address becomes this proxy's, [::1]:<port>, and the proxy's own
   dual-stack socket reaches the world:
   1. STUN (RFC 5389) to two public servers: this socket's public address and the NAT's kind.
   2. POST /infinity/v1/worlds/punch with those addresses: the server answers the world's
      addresses (its LAN on the same network, the host's address, its STUN and UPnP ones) and
      asks the host's relay to punch towards ours, so that the host's NAT lets us in.
   3. Every candidate is probed (STUN Binding requests, which the relay answers, and the game's
      own packets) until one answers; that one is the world. Packets from any other address
      never reach the game (the source filter), and only the game's packets (from loopback) go
      to the world.
   Nothing answering for PX_GIVE_UP_MS is logged; the game's packets keep going to the session's
   address, as before F3. [FreeRoam] Direct=1 turns the proxy off (the game then sends to the
   session's address itself). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define PX_AF_INET 2
#define PX_AF_INET6 23
#define PX_SOCK_DGRAM 2
#define PX_IPPROTO_UDP 17
#define PX_IPPROTO_IPV6 41
#define PX_IPV6_V6ONLY 27
#define PX_SOL_SOCKET 0xffff
#define PX_SO_RCVTIMEO 0x1006
#define PX_INVALID ((UINT_PTR)~(UINT_PTR)0)
#define PX_MAGIC 0x2112A442u
#define PX_CANDIDATES 8
#define PX_STUN_SERVERS 4
#define PX_PACKET 2048
#define PX_PROBE_MS 250u
#define PX_GIVE_UP_MS 15000u
/* Before a world answers, the game's packets go to every candidate for this long at most, then
   only to the session's address (a host naming someone else's address cannot aim the players'
   traffic at it for longer). */
#define PX_SPRAY_MS 30000u
#define PX_STUN_WAIT_MS 1500u
#define PX_KEEPALIVE_MS 15000u

typedef struct {
    unsigned short family, port; /* port in network order */
    unsigned flow;
    unsigned char address[16];
    unsigned scope;
} PxAddr;

typedef struct {
    int flags, family, socktype, protocol;
    size_t addrlen;
    wchar_t *canonname;
    void *addr;
    void *next;
} PxAddrInfo;

typedef UINT_PTR(WINAPI *PxSocketFn)(int, int, int);
typedef int(WINAPI *PxBindFn)(UINT_PTR, const void *, int);
typedef int(WINAPI *PxSetOptionFn)(UINT_PTR, int, int, const char *, int);
typedef int(WINAPI *PxRecvFromFn)(UINT_PTR, char *, int, int, void *, int *);
typedef int(WINAPI *PxSendToFn)(UINT_PTR, const char *, int, int, const void *, int);
typedef int(WINAPI *PxCloseFn)(UINT_PTR);
typedef int(WINAPI *PxNameFn)(UINT_PTR, void *, int *);
typedef int(WINAPI *PxStartupFn)(WORD, void *);
typedef int(WINAPI *PxGetAddrInfoFn)(const wchar_t *, const wchar_t *, const void *, PxAddrInfo **);
typedef void(WINAPI *PxFreeAddrInfoFn)(PxAddrInfo *);
typedef int(WINAPI *PxPtonFn)(int, const wchar_t *, void *);
typedef const wchar_t *(WINAPI *PxNtopFn)(int, const void *, wchar_t *, size_t);

static struct {
    PxSocketFn socket;
    PxBindFn bind;
    PxSetOptionFn setsockopt;
    PxRecvFromFn recvfrom;
    PxSendToFn sendto;
    PxCloseFn closesocket;
    PxNameFn getsockname;
    PxStartupFn startup;
    PxGetAddrInfoFn getaddrinfo;
    PxFreeAddrInfoFn freeaddrinfo;
    PxPtonFn pton;
    PxNtopFn ntop;
} px;

static int px_direct;
static SRWLOCK px_lock = SRWLOCK_INIT;
/* px_generation: each proxy thread stops once it changes. */
static volatile LONG px_generation, px_loaded;
static UINT_PTR px_socket = PX_INVALID;
static PxAddr px_candidates[PX_CANDIDATES];
static volatile LONG px_candidate_count;
static PxAddr px_world;
static volatile LONG px_have_world;
static PxAddr px_stun[PX_STUN_SERVERS];
static unsigned px_stun_count;
static PxAddr px_mapped[PX_STUN_SERVERS];
static unsigned char px_mapped_seen[PX_STUN_SERVERS];
static unsigned char px_stun_provider[PX_STUN_SERVERS],px_stun_attempts[PX_STUN_SERVERS];
static unsigned char px_stun_accepted[PX_STUN_SERVERS];
static unsigned long long px_mapped_at[PX_STUN_SERVERS],px_stun_round,px_stun_retry[PX_STUN_SERVERS];
static unsigned char px_txid[PX_STUN_SERVERS + 1][12]; /* the servers', then the probes' */
static unsigned short px_local_port;
typedef struct { unsigned char hello[104], response[147]; unsigned phase;
    unsigned long long started,next_send,proved; } PxProof;
static PxProof px_proofs[2][PX_CANDIDATES];
static volatile LONG px_game_proved,px_hook_proved;
static unsigned long px_proof_serial;
static unsigned long long px_game_at,px_hook_at;
static int px_proof_required(void) { return fi_launch_protocol(); }
/* Actor-owned, fixed-enum diagnostics. Never queue packet text or credential-derived ids. */
static unsigned char px_diag_attempt[16],px_diag_queue[64][3];
static unsigned px_diag_head,px_diag_size,px_diag_dropped;
static unsigned long px_diag_seen[3],px_diag_tx,px_diag_rx;
static unsigned long long px_diag_started,px_diag_last;
static int px_diag_enabled,px_diag_final;
static void px_diag(unsigned phase,unsigned kind,unsigned candidate) {
    if(!px_diag_enabled || phase<1 || phase>28 || kind>2 || candidate>4 || (px_diag_seen[kind]&(1UL<<phase)))return;
    px_diag_seen[kind]|=1UL<<phase;
    if(px_diag_size==64) {if(px_diag_dropped!=0xffffffffu)px_diag_dropped++;return;}
    unsigned at=(px_diag_head+px_diag_size++)%64;
    px_diag_queue[at][0]=(unsigned char)phase;px_diag_queue[at][1]=(unsigned char)kind;px_diag_queue[at][2]=(unsigned char)candidate;
}
static void px_diag_tick(int final) {
    unsigned long long now=GetTickCount64();
    if(!px_diag_enabled || (final ? px_diag_final : !px_diag_size || (px_diag_last && now-px_diag_last<1000)))return;
    unsigned char p[32]={1,23,2,0};memory_copy(p+4,px_diag_attempt,16);
    if(final)px_diag_final=1;
    else {memory_copy(p+1,px_diag_queue[px_diag_head],3);px_diag_head=(px_diag_head+1)%64;px_diag_size--;}
    unsigned long long elapsed=now-px_diag_started;
    unsigned long values[3]={elapsed>0xffffffffULL?0xffffffffUL:(unsigned long)elapsed,px_diag_tx,px_diag_rx};
    for(unsigned k=0;k<3;++k)for(unsigned b=0;b<4;++b)p[20+k*4+b]=(unsigned char)(values[k]>>(24-b*8));
    if(!fi_launch_udp_diagnostic(p,sizeof(p)) && px_diag_dropped!=0xffffffffu)px_diag_dropped++;
    px_diag_last=now;
}
static SRWLOCK px_hint_lock=SRWLOCK_INIT;
static unsigned char px_hint_payload[122],px_hint_grant[16];
static unsigned long px_hint_revision,px_hint_applied;
static size_t px_hint_size;
static unsigned long long px_hint_sent,px_hint_deadline;
static PxAddr px_initial;
static unsigned long px_hint_u32(const unsigned char *p) {return ((unsigned long)p[0]<<24)|((unsigned long)p[1]<<16)|((unsigned long)p[2]<<8)|p[3];}
int azimuth_proxy_relay_candidates(const unsigned char *p,size_t n) {
    if(n<22 || p[20]<1 || p[20]>30 || p[21]>5 || n!=22+(size_t)p[21]*20)return 0;
    unsigned pub=0,lan=0;
    for(unsigned i=0;i<p[21];++i) {
        const unsigned char *e=p+22+i*20;
        if(e[0]>1 || (e[1]!=4 && e[1]!=6) || !(e[18]|e[19]))return 0;
        pub+=e[0]==0;lan+=e[0]==1;if(pub>1 || lan>4)return 0;
        unsigned nonzero=0;
        for(unsigned k=0;k<16;++k) {if(e[1]==4 && k>=4 && e[2+k])return 0;nonzero|=e[2+k];}
        if(!nonzero || (e[1]==4 && (!e[2] || e[2]>=224)) || (e[1]==6 && e[2]==255))return 0;
        for(unsigned j=0;j<i;++j)if(e[1]==p[23+j*20] && memory_equal(e+2,p+24+j*20,18))return 0;
    }
    unsigned char context[89];unsigned long serial;
    int ready=fi_udp_context_get(context,&serial);
    int same=ready && memory_equal(context+1,p,16);fi_udp_zero(context,89);
    if(!same)return 1; /* An old asynchronous answer is harmless, never authority. */
    AcquireSRWLockExclusive(&px_hint_lock);
    if(memory_equal(px_hint_grant,p,16) && px_hint_revision==px_hint_u32(p+16) && px_hint_sent && !px_hint_size
        && px_hint_applied!=px_hint_revision && GetTickCount64()-px_hint_sent<(unsigned long long)p[20]*1000) {
        memory_copy(px_hint_payload,p,n);px_hint_size=n;px_hint_deadline=px_hint_sent+(unsigned long long)p[20]*1000;
    }
    ReleaseSRWLockExclusive(&px_hint_lock);return 1;
}

/* ------------------------------------------------------- relay fallback (CR-S57)

   MATCHMAKING_API.md 5 and 5b, the FIR1 wire of fi_relay::wire (RelayLegs.cs is the host's side).
   The launcher's RelayLeg (pipe frame 0x1d) names the grant's relay and the grant's exact signed
   claims: {u8 1, grant[16], u8 transport (0 direct, 1 relay), relay IPv4[4], u16 BE rendezvous
   port, u16 BE claims length (1-2048), claims, signature[64]}; {0} or {1, 16 zero bytes} clears
   it. A direct world's joiner whose game is not proved within 3 s of the first applied
   RelayCandidates says hello (role joiner) to that relay from the proxy's own socket, and once
   the relay says `relay ok` every datagram for the world travels as FIR1 `data` (the FIUP proof
   runs again, unchanged, over the relay); a relay-born world (transport 1) goes there at once.
   RelayFallback (0x1e) tells the launcher: 1 direct failed (renew the seat with transport
   "relay"), 2 relay ok, 3 relay unavailable for good, 4 direct ok. */
#define PX_LEG_CLAIMS 2048u
#define PX_RELAY_PAYLOAD 1200
#define PX_DIRECT_FALLBACK_MS 3000u
#define PX_RELAY_HELLO_MS 15000u
#define PX_RELAY_RETRY_MS 1000u
#define PX_RELAY_GIVE_UP_MS 10000u
#define PX_RELAY_PROOF_MS 15000u
static SRWLOCK px_leg_lock=SRWLOCK_INIT;
static struct {
    unsigned char grant[16],relay[4],claims[PX_LEG_CLAIMS],signature[64];
    size_t claims_size;unsigned short port;unsigned transport;int present;
} px_leg;
static volatile LONG px_leg_revision;
/* The proxy thread's leg (px_relay_phase is read by azimuth_proxy_send_world's callers too). */
enum {PXR_IDLE,PXR_TRYING,PXR_OK,PXR_GAVE_UP,PXR_DIRECT};
static struct {
    unsigned char grant[16],claims[PX_LEG_CLAIMS],signature[64],cookie[16];
    size_t claims_size;PxAddr relay;LONG revision;
    int have_leg,skip_direct,have_cookie,answered,want_relay,direct_told,unavailable;
    unsigned long long deadline,last_hello,last_relay,ok_at;
} px_relay={.revision=-1};
static volatile LONG px_relay_phase,px_relay_dropped;
/* Exclusive around every change into or out of PXR_OK (and of the relay and grant it uses). */
static SRWLOCK px_relay_send_lock=SRWLOCK_INIT;
static unsigned long long px_direct_clock; /* the first applied RelayCandidates; 0: none yet */
static void px_relay_reset(void);

static int px_leg_grant_current(const unsigned char *grant) {
    unsigned char context[89];unsigned long serial;
    int ready=fi_udp_context_get(context,&serial);
    int same=!ready || memory_equal(context+1,grant,16);fi_udp_zero(context,89);return same;
}
void azimuth_proxy_relay_leg_clear(void) {
    AcquireSRWLockExclusive(&px_leg_lock);
    fi_udp_zero(&px_leg,sizeof(px_leg));InterlockedIncrement(&px_leg_revision);
    ReleaseSRWLockExclusive(&px_leg_lock);
}
/* RelayLeg (0x1d): 1 stored or cleared, 0 refused (logged; the session goes on). */
int azimuth_proxy_relay_leg(const unsigned char *p,size_t n) {
    static const unsigned char zero[16]={0};
    if((n==1 && p[0]==0) || (n==17 && p[0]==1 && memory_equal(p+1,zero,16))) {
        azimuth_proxy_relay_leg_clear();log_append("RELAY: leg cleared.\r\n");return 1;
    }
    size_t claims=n>=26 ? ((size_t)p[24]<<8)|p[25] : 0;
    if(n<26 || p[0]!=1 || memory_equal(p+1,zero,16) || p[17]>1 || !p[18] || p[18]>=224 || !(p[22]|p[23])
        || !claims || claims>PX_LEG_CLAIMS || n!=26+claims+64) {
        log_append("RELAY: a RelayLeg frame that does not parse; ignored.\r\n");return 0;
    }
    /* Without a context yet it is kept; the proxy uses it only once the context names its grant. */
    if(!px_leg_grant_current(p+1)) {log_append("RELAY: a RelayLeg for another grant; ignored.\r\n");return 0;}
    AcquireSRWLockExclusive(&px_leg_lock);
    memory_copy(px_leg.grant,p+1,16);px_leg.transport=p[17];memory_copy(px_leg.relay,p+18,4);
    px_leg.port=(unsigned short)((p[22]<<8)|p[23]);px_leg.claims_size=claims;memory_copy(px_leg.claims,p+26,claims);
    memory_copy(px_leg.signature,p+26+claims,64);px_leg.present=1;InterlockedIncrement(&px_leg_revision);
    ReleaseSRWLockExclusive(&px_leg_lock);
    log_append(p[17] ? "RELAY: leg stored (relay-born world).\r\n" : "RELAY: leg stored (direct first, relay fallback).\r\n");
    return 1;
}

/* ------------------------------------------------------------------ helpers */

typedef struct {
    char text[256];
    size_t size;
} PxLine;

static void px_text(PxLine *l, const char *t) {
    while (t && *t && l->size + 1 < sizeof(l->text)) {
        l->text[l->size++] = *t++;
    }
    l->text[l->size] = 0;
}

static void px_number(PxLine *l, unsigned long long v) {
    char d[24];
    unsigned n = 0;
    do {
        d[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v && n < sizeof(d));
    char t[24];
    for (unsigned i = 0; i < n; ++i) {
        t[i] = d[n - 1 - i];
    }
    t[n] = 0;
    px_text(l, t);
}

/* `a` as text: a.b.c.d:port or [v6]:port (out: at least 64 bytes). */
static void px_format(const PxAddr *a, char *out, size_t capacity) {
    wchar_t wide[64];
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    unsigned port = (unsigned)((a->port >> 8) | ((a->port & 0xff) << 8));
    int v4 = 1;
    for (int i = 0; i < 10; ++i) {
        v4 &= a->address[i] == 0;
    }
    v4 &= a->address[10] == 0xff && a->address[11] == 0xff;
    wide[0] = 0;
    if (v4) {
        px.ntop(PX_AF_INET, a->address + 12, wide, ARRAY_COUNT(wide));
    } else {
        px_text(&l, "[");
        px.ntop(PX_AF_INET6, a->address, wide, ARRAY_COUNT(wide));
    }
    for (size_t i = 0; wide[i] && i < ARRAY_COUNT(wide); ++i) {
        char c[2] = {(char)wide[i], 0};
        px_text(&l, c);
    }
    px_text(&l, v4 ? ":" : "]:");
    px_number(&l, port);
    size_t n = l.size < capacity - 1 ? l.size : capacity - 1;
    memory_copy(out, l.text, n);
    out[n] = 0;
}

/* "a.b.c.d:port" or "[v6]:port" (n characters) -> `out` (IPv4 as ::ffff:a.b.c.d). */
static int px_parse(const char *text, size_t n, PxAddr *out) {
    wchar_t host[64];
    size_t start = 0, end = 0, colon = 0;
    if (n < 4 || n >= ARRAY_COUNT(host)) {
        return 0;
    }
    if (text[0] == '[') {
        start = 1;
        for (end = 1; end < n && text[end] != ']'; ++end) {
        }
        colon = end + 1;
    } else {
        for (end = 0; end < n && text[end] != ':'; ++end) {
        }
        colon = end;
    }
    if (colon >= n || text[colon] != ':') {
        return 0;
    }
    unsigned port = 0;
    for (size_t i = colon + 1; i < n; ++i) {
        if (text[i] < '0' || text[i] > '9' || port > 65535) {
            return 0;
        }
        port = port * 10 + (unsigned)(text[i] - '0');
    }
    if (!port || port > 65535) {
        return 0;
    }
    for (size_t i = start; i < end; ++i) {
        host[i - start] = (wchar_t)(unsigned char)text[i];
    }
    host[end - start] = 0;
    PxAddr a;
    a.family = PX_AF_INET6;
    a.port = (unsigned short)(((port & 0xff) << 8) | (port >> 8));
    a.flow = 0;
    a.scope = 0;
    for (int i = 0; i < 16; ++i) {
        ((volatile unsigned char *)a.address)[i] = 0;
    }
    unsigned char v4[4];
    if (text[0] != '[' && px.pton(PX_AF_INET, host, v4) == 1) {
        a.address[10] = a.address[11] = 0xff;
        memory_copy(a.address + 12, v4, 4);
    } else if (text[0] != '[' || px.pton(PX_AF_INET6, host, a.address) != 1) {
        return 0;
    }
    *out = a;
    return 1;
}

/* A sockaddr from recvfrom (IPv6, or IPv4 on a dual-stack socket's IPv4 path) as PxAddr. */
static void px_from(const unsigned char *raw, PxAddr *out) {
    memory_copy(out, raw, sizeof(*out));
    if (out->family == PX_AF_INET) {
        unsigned char v4[4];
        memory_copy(v4, raw + 4, 4);
        out->family = PX_AF_INET6;
        out->flow = 0;
        out->scope = 0;
        for (int i = 0; i < 16; ++i) {
            ((volatile unsigned char *)out->address)[i] = 0;
        }
        out->address[10] = out->address[11] = 0xff;
        memory_copy(out->address + 12, v4, 4);
    }
    out->flow = 0;
    out->scope = 0;
}

static int px_same(const PxAddr *a, const PxAddr *b) {
    return a->port == b->port && memory_equal(a->address, b->address, 16);
}

static int px_loopback(const PxAddr *a) {
    static const unsigned char v6[16] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1};
    return memory_equal(a->address, v6, 16) ||
           (a->address[10] == 0xff && a->address[11] == 0xff && a->address[12] == 127);
}

static void px_new_txid(unsigned char *t) {
    static volatile LONG counter;
    unsigned long long x = GetTickCount64() * 0x9e3779b97f4a7c15ull ^
                           ((unsigned long long)InterlockedIncrement(&counter) << 40) ^ (unsigned long long)(uintptr_t)t ^
                           (unsigned long long)(uintptr_t)&x;
    for (int i = 0; i < 12; ++i) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        t[i] = (unsigned char)(x >> 24);
    }
}

/* FIR1 ‖ kind ‖ grant ‖ payload (relay 0x03: no payload; data 0x05). Its size. */
static size_t px_fir1_frame(unsigned char *out,unsigned kind,const unsigned char *grant,const void *payload,size_t size) {
    memory_copy(out,"FIR1",4);out[4]=(unsigned char)kind;memory_copy(out+5,grant,16);
    if(size)memory_copy(out+21,payload,size);
    return 21+size;
}
/* FIR1 hello: role, u16 BE claims length, claims, signature[64], then the last challenge's cookie. */
static size_t px_fir1_hello(unsigned char *out,unsigned role,const unsigned char *claims,size_t claims_size,
                            const unsigned char *signature,const unsigned char *cookie) {
    memory_copy(out,"FIR1",4);out[4]=1;out[5]=(unsigned char)role;
    out[6]=(unsigned char)(claims_size>>8);out[7]=(unsigned char)claims_size;
    memory_copy(out+8,claims,claims_size);memory_copy(out+8+claims_size,signature,64);
    if(cookie)memory_copy(out+72+claims_size,cookie,16);
    return 72+claims_size+(cookie ? 16 : 0);
}
/* Every datagram for the world: in relay mode the world is the relay, and what goes there is
   FIR1 data for our grant (larger payloads are counted and dropped). */
static int px_send_on(UINT_PTR s,const void *data,int size,const PxAddr *to) {
    if(!fi_launch_network_allowed())return 0;
    /* The game's threads send too (azimuth_proxy_send_world): phase, relay and grant are read
       together, never from a half-reset leg. */
    AcquireSRWLockShared(&px_relay_send_lock);
    int relayed=px_relay_phase==PXR_OK && px_same(to,&px_relay.relay);
    unsigned char grant[16];
    if(relayed)memory_copy(grant,px_relay.grant,16);
    ReleaseSRWLockShared(&px_relay_send_lock);
    if(relayed) {
        if(size<=0 || size>PX_RELAY_PAYLOAD) {InterlockedIncrement(&px_relay_dropped);return 0;}
        unsigned char frame[21+PX_RELAY_PAYLOAD];
        int n=(int)px_fir1_frame(frame,5,grant,data,(size_t)size);
        return px.sendto(s,(const char *)frame,n,0,to,(int)sizeof(*to))==n;
    }
    return px.sendto(s,(const char *)data,size,0,to,(int)sizeof(*to))==size;
}
static void px_send(const void *data, int size, const PxAddr *to) {
    px_send_on(px_socket,data,size,to);
}
/* FIR1 control to the relay itself, never wrapped. */
static void px_send_raw(const void *data,int size,const PxAddr *to) {
    if(!fi_launch_network_allowed())return;
    px.sendto(px_socket,(const char *)data,size,0,to,(int)sizeof(*to));
}

static void px_binding_request(const unsigned char *txid, const PxAddr *to) {
    unsigned char m[20];
    m[0] = 0, m[1] = 1, m[2] = 0, m[3] = 0;
    m[4] = 0x21, m[5] = 0x12, m[6] = 0xa4, m[7] = 0x42;
    memory_copy(m + 8, txid, 12);
    px_send(m, 20, to);
}

static void px_binding_success(const unsigned char *txid, const PxAddr *seen) {
    unsigned char m[44];
    int v4 = seen->address[10] == 0xff && seen->address[11] == 0xff;
    int value = v4 ? 8 : 20;
    m[0] = 1, m[1] = 1, m[2] = 0, m[3] = (unsigned char)(4 + value);
    m[4] = 0x21, m[5] = 0x12, m[6] = 0xa4, m[7] = 0x42;
    memory_copy(m + 8, txid, 12);
    m[20] = 0, m[21] = 0x20, m[22] = 0, m[23] = (unsigned char)value;
    m[24] = 0, m[25] = v4 ? 1 : 2;
    unsigned char *port = (unsigned char *)&seen->port; /* network order */
    m[26] = (unsigned char)(port[0] ^ 0x21), m[27] = (unsigned char)(port[1] ^ 0x12);
    if (v4) {
        for (int i = 0; i < 4; ++i) m[28 + i] = (unsigned char)(seen->address[12 + i] ^ m[4 + i]);
    } else {
        for (int i = 0; i < 16; ++i) m[28 + i] = (unsigned char)(seen->address[i] ^ m[4 + i]);
    }
    px_send(m, 24 + value, seen);
}

/* A STUN message (RFC 5389): kind, txid and its (XOR-)MAPPED-ADDRESS. 0: not STUN. */
static int px_stun_parse(const unsigned char *d, int n, unsigned *kind, const unsigned char **txid, PxAddr *mapped,
                         int *has_mapped) {
    if (n < 20 || (d[0] & 0xc0) || d[4] != 0x21 || d[5] != 0x12 || d[6] != 0xa4 || d[7] != 0x42 ||
        ((d[2] << 8) | d[3]) + 20 != n || (n & 3)) {
        return 0;
    }
    *kind = (unsigned)((d[0] << 8) | d[1]);
    *txid = d + 8;
    *has_mapped = 0;
    for (int at = 20; at + 4 <= n;) {
        unsigned type = (unsigned)((d[at] << 8) | d[at + 1]);
        int len = (d[at + 2] << 8) | d[at + 3];
        if (at + 4 + len > n) {
            break;
        }
        const unsigned char *v = d + at + 4;
        int x = type == 0x0020 || type == 0x8020;
        if ((x || (type == 0x0001 && !*has_mapped)) && len >= 8) {
            PxAddr a;
            a.family = PX_AF_INET6;
            a.flow = 0;
            a.scope = 0;
            for (int i = 0; i < 16; ++i) ((volatile unsigned char *)a.address)[i] = 0;
            unsigned char p0 = v[2], p1 = v[3];
            if (x) p0 ^= 0x21, p1 ^= 0x12;
            ((unsigned char *)&a.port)[0] = p0;
            ((unsigned char *)&a.port)[1] = p1;
            if (v[1] == 1) {
                a.address[10] = a.address[11] = 0xff;
                for (int i = 0; i < 4; ++i) a.address[12 + i] = (unsigned char)(v[4 + i] ^ (x ? d[4 + i] : 0));
                *mapped = a, *has_mapped = 1;
            } else if (v[1] == 2 && len >= 20) {
                for (int i = 0; i < 16; ++i) a.address[i] = (unsigned char)(v[4 + i] ^ (x ? d[4 + i] : 0));
                *mapped = a, *has_mapped = 1;
            }
        }
        at += 4 + ((len + 3) & ~3);
    }
    return 1;
}

/* --------------------------------------------------------------- candidates */

static void px_add_candidate(const PxAddr *a) {
    AcquireSRWLockExclusive(&px_lock);
    int known = 0;
    for (LONG i = 0; i < px_candidate_count; ++i) {
        known |= px_same(&px_candidates[i], a);
    }
    if (!known && px_candidate_count < PX_CANDIDATES) {
        px_candidates[px_candidate_count] = *a;
        InterlockedIncrement(&px_candidate_count);
    }
    ReleaseSRWLockExclusive(&px_lock);
}

static int px_is_candidate(const PxAddr *a) {
    int found = 0;
    AcquireSRWLockExclusive(&px_lock);
    for (LONG i = 0; i < px_candidate_count; ++i) {
        found |= px_same(&px_candidates[i], a);
    }
    ReleaseSRWLockExclusive(&px_lock);
    return found;
}

/* The server's {"world":["ip:port",...]} -> candidates. */
static void px_take_world(const char *reply) {
    const char *at = text_find(reply, text_length(reply), "\"world\":[");
    if (!at) {
        return;
    }
    for (at += 9; *at && *at != ']';) {
        if (*at != '"') {
            ++at;
            continue;
        }
        const char *start = ++at;
        while (*at && *at != '"') ++at;
        PxAddr a;
        if (px_parse(start, (size_t)(at - start), &a)) {
            px_add_candidate(&a);
        }
        if (*at == '"') ++at;
    }
}

/* POST worlds/punch with this socket's public addresses (a thread: the proxy keeps relaying). */
static DWORD WINAPI px_signal(void *argument) {
    LONG generation = (LONG)(intptr_t)argument;
    if (!fi_launch_network_allowed() || px_generation != generation) return 0;
    static char body[512], reply[2048];
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    px_text(&l, "{\"candidates\":[");
    unsigned listed = 0;
    for (unsigned i = 0; i < px_stun_count && listed < 3; ++i) {
        if (!px_mapped_seen[i]) continue;
        int dup = 0;
        for (unsigned j = 0; j < i; ++j) dup |= px_mapped_seen[j] && px_same(&px_mapped[j], &px_mapped[i]);
        if (dup) continue;
        char a[72];
        px_format(&px_mapped[i], a, sizeof(a));
        px_text(&l, listed++ ? ",\"" : "\"");
        px_text(&l, a);
        px_text(&l, "\"");
    }
    px_text(&l, "],\"port\":");
    px_number(&l, px_local_port);
    px_text(&l, "}");
    memory_copy(body, l.text, l.size + 1);
    static wchar_t header[1200];
    azimuth_account_auth_header(header, ARRAY_COUNT(header));
    if (!fi_launch_network_allowed() || px_generation != generation) return 0;
    DWORD status = azimuth_net_call(L"POST", L"/infinity/v1/worlds/punch", header, body, reply, sizeof(reply));
    if (px_generation != generation || !fi_launch_network_allowed()) {
        return 0;
    }
    PxLine m;
    m.size = 0;
    m.text[0] = 0;
    px_text(&m, "PROXY: punch exchange answered ");
    px_number(&m, status);
    px_text(&m, status == 200 ? "; probing the world's addresses.\r\n" : "; probing the session's address only.\r\n");
    log_append(m.text);
    if (status == 200) {
        px_take_world(reply);
    }
    return 0;
}

/* ----------------------------------------------------------------- the loop */

static void px_log_world(const PxAddr *a, const char *how, DWORD after) {
    char text[72];
    px_format(a, text, sizeof(text));
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    px_text(&l, "PROXY: the world answered at ");
    px_text(&l, text);
    px_text(&l, " (");
    px_text(&l, how);
    px_text(&l, ", ");
    px_number(&l, after);
    px_text(&l, " ms).\r\n");
    log_append(l.text);
}

static void px_pick(const PxAddr *a, const char *how, DWORD started) {
    if (!px_have_world) {
        px_world = *a;
        InterlockedExchange(&px_have_world,1);
        if(px_proof_required())log_append("PROXY: authenticated UDP route selected.\r\n");
        else px_log_world(a, how, (DWORD)GetTickCount64() - started);
    }
}

static void px_proof_reset(void) {
    fi_udp_zero(px_proofs,sizeof(px_proofs));
    InterlockedExchange(&px_game_proved,0);InterlockedExchange(&px_hook_proved,0);px_game_at=px_hook_at=0;
}
/* IPv4 hints must agree across independent providers, never IPv4 versus IPv6. */
static unsigned short px_external_port(unsigned long long now) {
    for(unsigned i=0;i<px_stun_count;++i) {
        const unsigned char *a=px_mapped[i].address;
        if(!px_mapped_seen[i] || now-px_mapped_at[i]>=30000 || a[10]!=255 || a[11]!=255)continue;
        int v4=1;for(unsigned b=0;b<10;++b)v4&=a[b]==0;if(!v4)continue;
        for(unsigned j=i+1;j<px_stun_count;++j)
            if(px_stun_provider[i]!=px_stun_provider[j] && px_mapped_seen[j] && now-px_mapped_at[j]<30000
                && px_same(&px_mapped[i],&px_mapped[j]))
                return (unsigned short)((px_mapped[i].port>>8)|((px_mapped[i].port&255)<<8));
    }return 0;
}
/* At most two sends/server/15s round. Reuse the transaction on retry; reject old-round replies. */
static void px_stun_tick(unsigned long long now) {
    if(!px_stun_round || now-px_stun_round>=15000) {
        px_stun_round=now;
        for(unsigned i=0;i<px_stun_count;++i) {px_new_txid(px_txid[i]);px_stun_attempts[i]=0;px_stun_accepted[i]=0;px_stun_retry[i]=now;}
    }
    for(unsigned i=0;i<px_stun_count;++i)if(px_stun_attempts[i]<2 && now>=px_stun_retry[i]) {
        px_binding_request(px_txid[i],&px_stun[i]);px_stun_attempts[i]++;px_stun_retry[i]=now+750;
        if(px_diag_tx!=0xffffffffUL)px_diag_tx++;px_diag(7,2,0);
    }
}
static int px_proof_context(unsigned char *context) {
    unsigned long serial=0;if(!fi_udp_context_get(context,&serial))return 0;
    if(serial!=px_proof_serial) {
        px_relay_reset();px_direct_clock=0;
        px_proof_reset();px_have_world=0;px_proof_serial=serial;
        px_candidate_count=0;px_add_candidate(&px_initial);
        AcquireSRWLockExclusive(&px_hint_lock);px_hint_sent=px_hint_deadline=0;px_hint_size=0;
        ReleaseSRWLockExclusive(&px_hint_lock);
    }return 1;
}
/* recvfrom blocks: a pipe revocation may replace the context while it sleeps. */
static int px_proof_current(void) {
    unsigned char context[89]; unsigned long serial=0;
    int ready=fi_udp_context_get(context,&serial);fi_udp_zero(context,sizeof(context));
    if(!ready || serial!=px_proof_serial) {px_proof_reset();px_have_world=0;return 0;}
    unsigned long long now=GetTickCount64();
    if(now-px_game_at>=45000)InterlockedExchange(&px_game_proved,0);
    if(now-px_hook_at>=45000)InterlockedExchange(&px_hook_proved,0);
    return 1;
}
static void px_network_tick(const unsigned char *context,int publish) {
    unsigned char payload[122];size_t size=0;unsigned long long now=GetTickCount64();
    AcquireSRWLockExclusive(&px_hint_lock);
    if(!memory_equal(px_hint_grant,context+1,16)) {
        memory_copy(px_hint_grant,context+1,16);px_hint_revision=px_hint_applied=0;
        px_hint_sent=px_hint_deadline=0;px_hint_size=0;
    }
    if(px_hint_size && now<px_hint_deadline) {
        size=px_hint_size;memory_copy(payload,px_hint_payload,size);px_hint_size=0;px_hint_applied=px_hint_revision;
    }
    if(publish && (!px_hint_sent || now-px_hint_sent>=15000) && px_hint_revision!=0xffffffffUL) {
        unsigned char hint[25];memory_copy(hint,context+1,16);
        unsigned long revision=px_hint_revision+1;
        for(unsigned i=0;i<4;++i)hint[16+i]=(unsigned char)(revision>>(24-8*i));
        hint[20]=(unsigned char)(px_local_port>>8);hint[21]=(unsigned char)px_local_port;hint[22]=hint[23]=hint[24]=0;
        unsigned port=px_external_port(now);
        if(port) {
            hint[22]=1;hint[23]=(unsigned char)(port>>8);hint[24]=(unsigned char)port;
            px_diag(9,2,1);
        }
        if(fi_launch_relay_network_hint(hint,sizeof(hint))) {
            px_hint_revision=revision;px_hint_sent=now;px_hint_size=0;
            px_diag(11,2,0);
        } else px_diag(12,2,0);
    }
    int expired=px_hint_deadline && now>=px_hint_deadline;
    if(expired)px_hint_deadline=0;
    ReleaseSRWLockExclusive(&px_hint_lock);
    if(!size && !expired)return;
    if(size && !px_direct_clock)px_direct_clock=now?now:1; /* CR-S57: the direct path's 3 s start here */
    if(px_relay_phase==PXR_OK)return; /* relay mode: the relay is the only candidate */
    px_diag(size?13:14,2,0);
    PxAddr old[PX_CANDIDATES];static PxProof proofs[2][PX_CANDIDATES];LONG old_count=px_candidate_count;
    memory_copy(old,px_candidates,sizeof(old));memory_copy(proofs,px_proofs,sizeof(proofs));
    px_candidate_count=0;px_add_candidate(&px_initial);
    if(px_have_world)px_add_candidate(&px_world);
    if(size)for(unsigned i=0;i<payload[21];++i) {
        const unsigned char *e=payload+22+i*20;PxAddr a={0};a.family=PX_AF_INET6;
        memory_copy(&a.port,e+18,2);
        if(e[1]==4) {a.address[10]=a.address[11]=255;memory_copy(a.address+12,e+2,4);}
        else memory_copy(a.address,e+2,16);
        px_add_candidate(&a);
    }
    fi_udp_zero(px_proofs,sizeof(px_proofs));
    for(LONG i=0;i<px_candidate_count;++i)for(LONG j=0;j<old_count;++j)if(px_same(&px_candidates[i],&old[j]))
        for(unsigned kind=0;kind<2;++kind)memory_copy(&px_proofs[kind][i],&proofs[kind][j],sizeof(PxProof));
    fi_udp_zero(proofs,sizeof(proofs));
}
static void px_proof_tick(const unsigned char *context) {
    unsigned long long now=GetTickCount64();
    if(px_game_proved && now-px_game_at>=45000)InterlockedExchange(&px_game_proved,0);
    if(px_hook_proved && now-px_hook_at>=45000)InterlockedExchange(&px_hook_proved,0);
    for(LONG i=0;i<px_candidate_count && i<PX_CANDIDATES;++i) {
        if(px_have_world && !px_same(&px_candidates[i],&px_world))continue;
        for(unsigned kind=0;kind<2;++kind) {
            PxProof *proof=&px_proofs[kind][i];
            if(proof->phase==3 && now-proof->proved<15000)continue;
            if(!proof->phase || proof->phase==3 || now-proof->started>=5000) {
                fi_udp_zero(proof->hello,sizeof(proof->hello));memory_copy(proof->hello,"FIUP",4);
                proof->hello[4]=1;proof->hello[5]=1;proof->hello[6]=(unsigned char)kind;memory_copy(proof->hello+8,context+1,48);
                if(!fi_udp_random(proof->hello+56,16))continue;
                fi_udp_sign(proof->hello,104,context+57);proof->phase=1;proof->started=now;proof->next_send=0;
            }
            if(now<proof->next_send)continue;
            proof->next_send=now+500;px_send(proof->phase==2 ? proof->response : proof->hello,proof->phase==2 ? 147 : 104,&px_candidates[i]);
            if(px_diag_tx!=0xffffffffUL)px_diag_tx++;px_diag(proof->phase==2?17:15,kind,i==0?3:0);
        }
    }
}
static int px_proof_received(const unsigned char *packet,int n,const PxAddr *from,const unsigned char *context,DWORD started) {
    if(n<4 || !memory_equal(packet,"FIUP",4))return 0;
    if(n!=147 || (packet[5]!=2 && packet[5]!=4) || !fi_udp_verify(packet,(size_t)n,context+57)
        || !memory_equal(packet+8,context+1,48) || fi_udp_u64(packet+107)>fi_udp_u64(context+49)
        || fi_udp_u64(packet+107)<=fi_launch_server_unix()) {px_diag(19,2,0);return 1;}
    LONG candidate=-1;
    for(LONG i=0;i<px_candidate_count && i<PX_CANDIDATES;++i)if(px_same(from,&px_candidates[i])) {candidate=i;break;}
    if(candidate<0 || (px_have_world && !px_same(from,&px_world)))return 1;
    PxProof *proof=&px_proofs[packet[6]][candidate];unsigned long long now=GetTickCount64();
    if((proof->phase!=1 && proof->phase!=2) || now-proof->started>=5000 || !memory_equal(packet+56,proof->hello+56,16))return 1;
    if(packet[5]==2) {
        px_diag(16,packet[6],candidate==0?3:0);
        if(proof->phase==2 && !memory_equal(packet+6,proof->response+6,109))return 1;
        memory_copy(proof->response,packet,147);proof->response[5]=3;fi_udp_sign(proof->response,147,context+57);
        proof->phase=2;proof->next_send=now+500;px_send(proof->response,147,from);
        if(px_diag_tx!=0xffffffffUL)px_diag_tx++;px_diag(17,packet[6],candidate==0?3:0);return 1;
    }
    if(proof->phase!=2 || !memory_equal(packet+6,proof->response+6,109))return 1;
    proof->phase=3;proof->proved=now;
    px_diag(18,packet[6],candidate==0?3:0);
    if(packet[6]==0) {
        px_game_at=now;InterlockedExchange(&px_game_proved,1);px_pick(from,"authenticated UDP",started);px_diag(21,0,candidate==0?3:0);
        PxProof *hook=&px_proofs[1][candidate];
        if(hook->proved && now-hook->proved<45000) {px_hook_at=hook->proved;InterlockedExchange(&px_hook_proved,1);}
        else {px_hook_at=0;InterlockedExchange(&px_hook_proved,0);}
    } else if(px_have_world && px_same(from,&px_world)) { px_hook_at=now;InterlockedExchange(&px_hook_proved,1); }return 1;
}

/* ------------------------------------------------- relay leg (proxy thread only) */

static void px_relay_log(const char *text,const char *tail) {
    char at[72];px_format(&px_relay.relay,at,sizeof(at));
    PxLine l;l.size=0;l.text[0]=0;px_text(&l,text);px_text(&l,at);px_text(&l,tail);log_append(l.text);
}
/* Back to the direct path's state; a relay mode's world and proofs were the relay's. */
static void px_relay_reset(void) {
    AcquireSRWLockExclusive(&px_relay_send_lock);
    LONG phase=InterlockedExchange(&px_relay_phase,PXR_IDLE);
    fi_udp_zero(&px_relay,sizeof(px_relay));px_relay.revision=-1;
    ReleaseSRWLockExclusive(&px_relay_send_lock);
    LONG dropped=InterlockedExchange(&px_relay_dropped,0);
    if(dropped) {
        PxLine l;l.size=0;l.text[0]=0;px_text(&l,"RELAY: ");px_number(&l,(unsigned long long)dropped);
        px_text(&l," datagram(s) over 1200 bytes were not relayed.\r\n");log_append(l.text);
    }
    if(phase==PXR_OK) {
        px_proof_reset();InterlockedExchange(&px_have_world,0);
        px_candidate_count=0;px_add_candidate(&px_initial);
    }
}
static void px_relay_hello(unsigned long long now) {
    static unsigned char m[72+PX_LEG_CLAIMS+16];
    size_t n=px_fir1_hello(m,1,px_relay.claims,px_relay.claims_size,px_relay.signature,px_relay.have_cookie ? px_relay.cookie : 0);
    px_send_raw(m,(int)n,&px_relay.relay);px_relay.last_hello=now;
}
static void px_relay_send_relay(unsigned long long now) {
    unsigned char m[21];px_fir1_frame(m,3,px_relay.grant,0,0);
    px_send_raw(m,21,&px_relay.relay);px_relay.last_relay=now;
}
/* A relay-born world skips the direct path only before anything happened: no world yet and the
   leg idle. A launcher's later transport "relay" (its renew after state 1) never stops a leg in
   progress, a relayed world or a direct one that already proved (state 4). */
static void px_relay_transport(unsigned transport) {
    if(px_relay_phase==PXR_IDLE && !px_have_world && !px_relay.direct_told)px_relay.skip_direct=transport==1;
}
/* Whether the direct path's probes, proofs and keep-alives run (relay mode: the relay's proof). */
static int px_direct_allowed(void) {
    LONG phase=px_relay_phase;
    return phase!=PXR_OK && (!px_relay.skip_direct || phase==PXR_DIRECT);
}
/* The launcher's leg for the context's grant, when it changed. A copy for the same grant and
   relay, re-signed or with the other transport, keeps the leg's progress (cookie, relay request,
   relay mode and its proof), like RelayLegs.Update. */
static void px_relay_sync(const unsigned char *context) {
    LONG revision=px_leg_revision;
    if(revision==px_relay.revision)return;
    AcquireSRWLockShared(&px_leg_lock);
    int usable=px_leg.present && memory_equal(px_leg.grant,context+1,16);
    PxAddr relay={0};relay.family=PX_AF_INET6;relay.address[10]=relay.address[11]=0xff;
    memory_copy(relay.address+12,px_leg.relay,4);relay.port=(unsigned short)((px_leg.port>>8)|((px_leg.port&255)<<8));
    int same=usable && px_relay.have_leg && memory_equal(px_relay.grant,px_leg.grant,16) && px_same(&relay,&px_relay.relay);
    if(same)px_relay_transport(px_leg.transport);
    if(!same) {
        ReleaseSRWLockShared(&px_leg_lock);
        if(px_relay.have_leg)log_append("RELAY: leg changed or cleared; the relay leg stops.\r\n");
        px_relay_reset();
        AcquireSRWLockShared(&px_leg_lock);
        usable=px_leg.present && memory_equal(px_leg.grant,context+1,16);
        if(usable) {
            memory_copy(px_relay.grant,px_leg.grant,16);px_relay.relay=relay;
            px_relay_transport(px_leg.transport);px_relay.have_leg=1;
        }
    }
    if(usable) {
        memory_copy(px_relay.claims,px_leg.claims,px_leg.claims_size);px_relay.claims_size=px_leg.claims_size;
        memory_copy(px_relay.signature,px_leg.signature,64);
    }
    px_relay.revision=revision;
    ReleaseSRWLockShared(&px_leg_lock);
}
static void px_relay_start(unsigned long long now) {
    px_relay.deadline=now+PX_RELAY_GIVE_UP_MS;px_relay.answered=px_relay.want_relay=px_relay.unavailable=0;
    InterlockedExchange(&px_relay_phase,PXR_TRYING);
    px_relay_hello(now);
}
static void px_relay_give_up(const char *line) {
    log_append(line);
    fi_launch_relay_fallback(px_relay.grant,3);
    unsigned char grant[16];memory_copy(grant,px_relay.grant,16);
    LONG revision=px_relay.revision;PxAddr relay=px_relay.relay;int skip=px_relay.skip_direct;
    px_relay_reset();
    /* the leg stays known (no second try for this grant) */
    memory_copy(px_relay.grant,grant,16);px_relay.relay=relay;px_relay.skip_direct=skip;px_relay.have_leg=1;
    px_relay.revision=revision;px_relay.direct_told=1;
    InterlockedExchange(&px_relay_phase,PXR_GAVE_UP);
}
/* 0x04 relay ok: the relay becomes the world's only candidate and the FIUP proof runs over it. */
static void px_relay_ok(unsigned long long now) {
    px_proof_reset();InterlockedExchange(&px_have_world,0);
    AcquireSRWLockExclusive(&px_lock);px_candidates[0]=px_relay.relay;px_candidate_count=1;ReleaseSRWLockExclusive(&px_lock);
    px_relay.ok_at=now;
    AcquireSRWLockExclusive(&px_relay_send_lock);InterlockedExchange(&px_relay_phase,PXR_OK);ReleaseSRWLockExclusive(&px_relay_send_lock);
    fi_launch_relay_fallback(px_relay.grant,2);
    log_append("RELAY: relay ok; the world's datagrams go through the relay.\r\n");
}
/* Once per loop with the current context and the time (the tests drive `now`). */
static void px_relay_tick(const unsigned char *context,unsigned long long now) {
    px_relay_sync(context);
    if(!px_relay.have_leg)return;
    LONG phase=px_relay_phase;
    if(px_game_proved && phase!=PXR_OK) {
        if(!px_relay.direct_told) {
            px_relay.direct_told=1;fi_launch_relay_fallback(px_relay.grant,4);
            if(phase==PXR_TRYING) {
                log_append("RELAY: the direct path answered first; the relay leg stops.\r\n");
                InterlockedExchange(&px_relay_phase,PXR_DIRECT);
            }
        }
        return;
    }
    if(phase==PXR_IDLE) {
        if(px_relay.skip_direct) px_relay_log("RELAY: relay-born world; trying ",".\r\n");
        else if(px_direct_clock && now-px_direct_clock>=PX_DIRECT_FALLBACK_MS && !px_have_world) {
            fi_launch_relay_fallback(px_relay.grant,1);
            px_relay_log("RELAY: direct failed after 3 s; trying ",".\r\n");
        } else return;
        px_relay_start(now);return;
    }
    if(phase==PXR_TRYING) {
        if(now>=px_relay.deadline) {px_relay_give_up("RELAY: relay unavailable, giving up.\r\n");return;}
        if(now-px_relay.last_hello>=(px_relay.answered ? PX_RELAY_HELLO_MS : PX_RELAY_RETRY_MS))px_relay_hello(now);
        if(px_relay.want_relay && now-px_relay.last_relay>=PX_RELAY_RETRY_MS)px_relay_send_relay(now);
    } else if(phase==PXR_OK) {
        if(now-px_relay.last_hello>=PX_RELAY_HELLO_MS)px_relay_hello(now);
        if(!px_have_world && now-px_relay.ok_at>=PX_RELAY_PROOF_MS)
            px_relay_give_up("RELAY: the world did not answer through the relay; giving up.\r\n");
    }
}
enum {PXR_RX_NOT_OURS,PXR_RX_HANDLED,PXR_RX_DATA};
/* A datagram from the relay: FIR1 control is handled here, our grant's data is unwrapped in place
   (*n becomes the payload's size) and then read exactly as one from the world. Anything else
   from the relay's address is dropped; other senders are not ours. */
static int px_relay_received(unsigned char *packet,int *n,const PxAddr *from,unsigned long long now) {
    LONG phase=px_relay_phase;
    if((phase!=PXR_TRYING && phase!=PXR_OK) || !px_same(from,&px_relay.relay))return PXR_RX_NOT_OURS;
    int size=*n;
    if(size<21 || !memory_equal(packet,"FIR1",4) || !memory_equal(packet+5,px_relay.grant,16))return PXR_RX_HANDLED;
    switch(packet[4]) {
    case 0x07: /* challenge: hello again with its cookie */
        if(size!=37)break;
        /* until peer, relay ok or relay unavailable registers us, the cookie hello repeats every 1 s */
        memory_copy(px_relay.cookie,packet+21,16);px_relay.have_cookie=1;px_relay_hello(now);break;
    case 0x02: /* peer: both sides registered; ask to forward */
        if(size!=29 && size!=41)break;
        px_relay.answered=1;
        if(phase==PXR_TRYING && !px_relay.want_relay) {px_relay.want_relay=1;px_relay_send_relay(now);}
        break;
    case 0x04:
        if(size==21 && phase==PXR_TRYING)px_relay_ok(now);
        break;
    case 0x06: /* the host's leg may not be up yet: retried until the deadline */
        if(size!=21 || phase!=PXR_TRYING)break;
        px_relay.answered=px_relay.want_relay=1;
        if(!px_relay.unavailable) {
            px_relay.unavailable=1;
            if(px_relay.deadline<now+PX_RELAY_GIVE_UP_MS)px_relay.deadline=now+PX_RELAY_GIVE_UP_MS;
            log_append("RELAY: relay unavailable for now; retrying every second.\r\n");
        }
        break;
    case 0x05:
        if(phase!=PXR_OK || size<=21 || size-21>PX_RELAY_PAYLOAD)break;
        for(int i=21;i<size;++i)packet[i-21]=packet[i];
        *n=size-21;return PXR_RX_DATA;
    default:break;
    }
    return PXR_RX_HANDLED;
}

static DWORD WINAPI px_thread(void *argument) {
    LONG generation = (LONG)(intptr_t)argument;
    /* this thread's own socket: a restart (azimuth_proxy_stop, then a new one) may set
       px_socket to the next one before this thread is done, and closing that one too would
       close it twice (on Wine a socket's number can be a file of the game's by then) */
    UINT_PTR own = px_socket;
    static unsigned char packet[PX_PACKET], from_raw[64];
    PxAddr game, from;
    memory_copy(&game, &px_candidates[0], sizeof(game));
    int have_game = 0, signalled = 0, gave_up = 0;
    DWORD started = (DWORD)GetTickCount64(), last_probe = 0, last_keepalive = (DWORD)GetTickCount64();
    px_diag_enabled=px_proof_required() && fi_udp_random(px_diag_attempt,16);px_diag_started=GetTickCount64();
    px_diag_last=0;px_diag_head=px_diag_size=px_diag_dropped=0;px_diag_tx=px_diag_rx=0;px_diag_final=0;
    for(unsigned i=0;i<3;++i)px_diag_seen[i]=0;
    px_diag(5,2,0);px_diag(fi_launch_server_unix()?3:4,2,0);
    px_stun_round=0;
    px_new_txid(px_txid[PX_STUN_SERVERS]);
    px_relay_reset();px_direct_clock=0;
    while (px_generation == generation && fi_launch_network_allowed()) {
        DWORD now = (DWORD)GetTickCount64();
        px_diag_tick(0);px_stun_tick(GetTickCount64());
        LONG relay_phase=px_relay_phase;
        /* a leg that is still trying, or relaying, ends by its own deadlines */
        if(px_proof_required() && !px_have_world && now-started>=30000 && relay_phase!=PXR_TRYING && relay_phase!=PXR_OK) {
            log_append("PROXY: direct connection could not be established within 30 seconds.\r\n");
            fi_launch_event(6,"2|connection.direct_failed");break;
        }
        unsigned char proof_context[89];
        if(px_proof_required()) {
            if(!px_proof_context(proof_context)) {px_diag(2,2,0);fi_udp_zero(proof_context,89);px_proof_reset();break;}
            px_diag(1,2,0);
            px_network_tick(proof_context,signalled);
            px_relay_tick(proof_context,GetTickCount64());
            relay_phase=px_relay_phase;
            /* a relay-born world never tries the direct path */
            if(px_direct_allowed() || relay_phase==PXR_OK)px_proof_tick(proof_context);
        }
        int direct_probes=px_direct_allowed();
        /* the punch exchange once STUN answered (or gave up waiting) */
        if (!signalled) {
            unsigned answered = 0;
            for (unsigned i = 0; i < px_stun_count; ++i) answered += px_mapped_seen[i];
            if (answered == px_stun_count || now - started >= PX_STUN_WAIT_MS) {
                if(answered<px_stun_count)px_diag(8,2,0);
                if(!px_external_port(GetTickCount64()))px_diag(10,2,0);
                signalled = 1;
                HANDLE t = px_proof_required() ? 0 : CreateThread(0, 0, px_signal, (void *)(intptr_t)generation, 0, 0);
                if (t) CloseHandle(t);
            }
        }
        if (direct_probes && !px_have_world && now - last_probe >= PX_PROBE_MS && now - started < PX_SPRAY_MS) {
            last_probe = now;
            for (LONG i = 0; i < px_candidate_count && i < PX_CANDIDATES; ++i) {
                px_binding_request(px_txid[PX_STUN_SERVERS], &px_candidates[i]);
            }
            if (!gave_up && now - started >= PX_GIVE_UP_MS) {
                gave_up = 1;
                log_append("PROXY: no answer from the world's addresses yet; the host's network may block it "
                           "(INFINITY > TEST CONNECTION). Still trying.\r\n");
            }
        }
        if (direct_probes && px_have_world && now - last_keepalive >= PX_KEEPALIVE_MS) {
            last_keepalive = now;
            px_binding_request(px_txid[PX_STUN_SERVERS], &px_world);
        }
        int from_size = (int)sizeof(from_raw);
        int n = px.recvfrom(own, (char *)packet, sizeof(packet), 0, from_raw, &from_size);
        if (px_generation != generation || !fi_launch_network_allowed()) {fi_udp_zero(proof_context,89);break;}
        if(px_proof_required() && !px_proof_current()) {fi_udp_zero(proof_context,89);continue;}
        if (n <= 0) {
            fi_udp_zero(proof_context,89);
            continue;
        }
        px_from(from_raw, &from);
        if(px_diag_rx!=0xffffffffUL)px_diag_rx++;
        /* the relay: its control here, its data on as the world's own datagram */
        if(px_proof_required() && px_relay_received(packet,&n,&from,GetTickCount64())==PXR_RX_HANDLED) {
            fi_udp_zero(proof_context,89);continue;
        }
        if(px_proof_required() && px_proof_received(packet,n,&from,proof_context,started)) {fi_udp_zero(proof_context,89);continue;}
        fi_udp_zero(proof_context,89);
        unsigned kind = 0;
        const unsigned char *txid = 0;
        PxAddr mapped;
        int has_mapped = 0;
        if (px_stun_parse(packet, n, &kind, &txid, &mapped, &has_mapped)) {
            if (kind == 0x3E91) {
                /* the relay passing another player's headlights and roof (azimuth_carstate.c) */
                if (px_have_world && (!px_proof_required() || px_hook_proved) && px_same(&from, &px_world)) {
                    azimuth_carstate_received(packet, n);
                }
            } else if (kind == 0x3E92) {
                /* the retired livery channel (OD-66, CR-S59): an older relay's message, dropped unread */
            } else if (kind == 0x0101) {
                for (unsigned i = 0; i < px_stun_count; ++i) {
                    if (!px_stun_accepted[i] && memory_equal(txid, px_txid[i], 12) && px_same(&from, &px_stun[i]) && has_mapped) {
                        px_mapped[i] = mapped;
                        px_mapped_seen[i] = 1;
                        px_mapped_at[i]=GetTickCount64();px_stun_attempts[i]=2;px_stun_accepted[i]=1;px_diag(28,2,0);
                    }
                }
                if (memory_equal(txid, px_txid[PX_STUN_SERVERS], 12) && px_is_candidate(&from)) {
                    if(!px_proof_required())px_pick(&from, "STUN", started);
                }
            } else if (kind == 0x0001 && px_is_candidate(&from)) {
                /* the host's relay punching towards us */
                px_binding_success(txid, &from);
            }
            continue;
        }
        if (px_loopback(&from)) {
            /* the game (the first loopback sender is it: nobody else knows this port) */
            if (!have_game) {
                game = from;
                have_game = 1;
            } else if (!px_same(&from, &game)) {
                continue;
            }
            if(px_proof_required() && (!px_proof_current() || !px_game_proved))continue;
            if (px_have_world) {
                px_send(packet, n, &px_world);
            } else if(!px_proof_required()) {
                LONG targets = now - started < PX_SPRAY_MS ? px_candidate_count : 1;
                for (LONG i = 0; i < targets && i < PX_CANDIDATES; ++i) {
                    px_send(packet, n, &px_candidates[i]);
                }
            }
            continue;
        }
        /* from the Internet: only the world reaches the game */
        if (!px_proof_required() && !px_have_world && px_is_candidate(&from)) {
            px_pick(&from, "NerdCore", started);
        }
        if (px_have_world && (!px_proof_required() || (px_proof_current() && px_game_proved)) && px_same(&from, &px_world) && have_game) {
            px_send(packet, n, &game);
        }
    }
    if(!px_game_proved)px_diag(20,0,0);
    px_diag_tick(1);px_diag_enabled=0;
    if(px_proof_required()) {
        /* Terminal-only file write: preserve all coalesced transitions after network work ends. */
        PxLine summary;summary.size=0;summary.text[0]=0;
        px_text(&summary,"PROXY: UDP final game_ack=");px_number(&summary,(px_diag_seen[0]&(1UL<<18))!=0);
        px_text(&summary," hook_ack=");px_number(&summary,(px_diag_seen[1]&(1UL<<18))!=0);
        px_text(&summary," game_phases=");px_number(&summary,px_diag_seen[0]);
        px_text(&summary," hook_phases=");px_number(&summary,px_diag_seen[1]);
        px_text(&summary," shared_phases=");px_number(&summary,px_diag_seen[2]);
        px_text(&summary," tx=");px_number(&summary,px_diag_tx);px_text(&summary," rx=");px_number(&summary,px_diag_rx);
        px_text(&summary," dropped=");px_number(&summary,px_diag_dropped);px_text(&summary,"\r\n");log_append(summary.text);
    }
    px_relay_reset();px_direct_clock=0;
    px_proof_reset();
    InterlockedCompareExchangePointer((void *volatile *)&px_socket, (void *)PX_INVALID, (void *)own);
    px.closesocket(own);
    return 0;
}

int azimuth_proxy_send_world(const void *data, int size) {
    UINT_PTR s = px_socket;
    if(px_proof_required()) {
        unsigned char context[89];unsigned long serial;
        int ready=fi_udp_context_get(context,&serial);fi_udp_zero(context,89);
        if(!ready || serial!=px_proof_serial)return 0;
    }
    if (!fi_launch_network_allowed() || s == PX_INVALID || !px_have_world || (px_direct && !px_proof_required())
        || (px_proof_required() && (!px_hook_proved || GetTickCount64()-px_hook_at>=45000))) {
        return 0;
    }
    PxAddr world = px_world;
    return px_send_on(s, data, size, &world); /* FIR1 data in relay mode */
}
int azimuth_proxy_proof_mode(void) {return px_proof_required() && px_socket!=PX_INVALID;}

/* --------------------------------------------------------------------- setup */

static int px_load(void) {
    if (px_loaded) {
        return px_loaded > 0;
    }
    HMODULE ws2 = LoadLibraryExW(L"ws2_32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (ws2) {
        px.socket = (PxSocketFn)(uintptr_t)GetProcAddress(ws2, "socket");
        px.bind = (PxBindFn)(uintptr_t)GetProcAddress(ws2, "bind");
        px.setsockopt = (PxSetOptionFn)(uintptr_t)GetProcAddress(ws2, "setsockopt");
        px.recvfrom = (PxRecvFromFn)(uintptr_t)GetProcAddress(ws2, "recvfrom");
        px.sendto = (PxSendToFn)(uintptr_t)GetProcAddress(ws2, "sendto");
        px.closesocket = (PxCloseFn)(uintptr_t)GetProcAddress(ws2, "closesocket");
        px.getsockname = (PxNameFn)(uintptr_t)GetProcAddress(ws2, "getsockname");
        px.startup = (PxStartupFn)(uintptr_t)GetProcAddress(ws2, "WSAStartup");
        px.getaddrinfo = (PxGetAddrInfoFn)(uintptr_t)GetProcAddress(ws2, "GetAddrInfoW");
        px.freeaddrinfo = (PxFreeAddrInfoFn)(uintptr_t)GetProcAddress(ws2, "FreeAddrInfoW");
        px.pton = (PxPtonFn)(uintptr_t)GetProcAddress(ws2, "InetPtonW");
        px.ntop = (PxNtopFn)(uintptr_t)GetProcAddress(ws2, "InetNtopW");
    }
    int ok = ws2 && px.socket && px.bind && px.setsockopt && px.recvfrom && px.sendto && px.closesocket &&
             px.getsockname && px.startup && px.getaddrinfo && px.freeaddrinfo && px.pton && px.ntop;
    if (ok) {
        static unsigned char data[512]; /* WSADATA */
        ok = px.startup(0x0202, data) == 0;
    }
    px_loaded = ok ? 1 : -1;
    return ok;
}

/* The public STUN servers (resolved once; IPv4 and IPv6 of each). */
static void px_resolve_stun(void) {
    static const wchar_t *const names[2][2] = {{L"stun.l.google.com", L"19302"}, {L"stun.cloudflare.com", L"3478"}};
    if (px_stun_count) {
        return;
    }
    for (int k = 0; k < 2; ++k) {
        PxAddrInfo hints, *list = 0;
        unsigned char *h = (unsigned char *)&hints;
        for (size_t i = 0; i < sizeof(hints); ++i) h[i] = 0;
        hints.socktype = PX_SOCK_DGRAM;
        hints.protocol = PX_IPPROTO_UDP;
        if (px.getaddrinfo(names[k][0], names[k][1], &hints, &list) != 0) {
            continue;
        }
        int have4 = 0, have6 = 0;
        for (PxAddrInfo *i = list; i && px_stun_count < PX_STUN_SERVERS; i = (PxAddrInfo *)i->next) {
            if ((i->family == PX_AF_INET && !have4) || (i->family == PX_AF_INET6 && !have6)) {
                px_stun_provider[px_stun_count]=(unsigned char)k;
                px_from((const unsigned char *)i->addr, &px_stun[px_stun_count++]);
                have4 |= i->family == PX_AF_INET;
                have6 |= i->family == PX_AF_INET6;
            }
        }
        px.freeaddrinfo(list);
    }
}

/* A world on this network: an IPv4-mapped RFC 1918 address (10/8, 172.16/12, 192.168/16). The
   server hands a joiner the host's LAN address when both share a public address
   (MATCHMAKING_API.md 4a): no NAT between them, so the game goes there directly. */
static int px_private_v4(const PxAddr *a) {
    const unsigned char *b = a->address;
    for (int i = 0; i < 10; ++i) {
        if (b[i]) {
            return 0;
        }
    }
    return b[10] == 0xff && b[11] == 0xff &&
           (b[12] == 10 || (b[12] == 172 && b[13] >= 16 && b[13] <= 31) || (b[12] == 192 && b[13] == 168));
}

void azimuth_proxy_configure(int direct) {
    px_direct = direct;
}

/* Called with the world the session named (CreateAssociationAsync). For a world on another PC
   it starts the proxy and returns 1 with `local` = the address the game should use. */
int azimuth_proxy_start(const void *world, void *local) {
    if (!fi_launch_network_allowed()) return px_proof_required() ? -1 : 0;
    PxAddr target, here;
    memory_copy(&target, world, sizeof(target));
    int proof_required=px_proof_required();
    if(proof_required && !px_loopback(&target)) {
        unsigned char context[89];unsigned long serial;
        int ready=fi_udp_context_get(context,&serial);fi_udp_zero(context,sizeof(context));
        if(!ready)return -1;
        px_direct=0;
    }
    if (px_direct && fi_tier_free() && !fi_launch_protocol()) {
        /* the Beta protocol: [hook] FreeRoamDirect is a diagnostic for Supporters, a free profile
           always goes through the proxy, to the world the server named. A launcher session has
           no tier gate (CR-S69, OD-129). */
        px_direct = 0;
        log_append("PROXY: free profile: [hook] FreeRoamDirect ignored; the world is reached through the proxy.\r\n");
    }
    if (px_direct || px_loopback(&target)) {
        return 0;
    }
    if(!px_load())return proof_required ? -1 : 0;
    if (!proof_required && fi_launch_protocol() && px_private_v4(&target)) {
        /* OD-54, local network first: the launcher's host relay on this LAN, reached directly */
        log_append("PROXY: the world is on this network; the game goes to it directly.\r\n");
        return 0;
    }
    azimuth_proxy_stop();
    if(proof_required && px_socket!=PX_INVALID)return -1; /* No overlapping actor may overwrite proof state. */
    px_proof_reset();px_proof_serial=0;
    px_resolve_stun();
    if (!fi_launch_network_allowed()) return proof_required ? -1 : 0;
    UINT_PTR s = px.socket(PX_AF_INET6, PX_SOCK_DGRAM, PX_IPPROTO_UDP);
    if (s == PX_INVALID) {
        log_append("PROXY: could not create the world socket.\r\n");
        return proof_required ? -1 : 0;
    }
    DWORD off = 0, timeout = 50;
    px.setsockopt(s, PX_IPPROTO_IPV6, PX_IPV6_V6ONLY, (const char *)&off, sizeof(off));
    px.setsockopt(s, PX_SOL_SOCKET, PX_SO_RCVTIMEO, (const char *)&timeout, sizeof(timeout));
    PxAddr any;
    unsigned char *z = (unsigned char *)&any;
    for (size_t i = 0; i < sizeof(any); ++i) z[i] = 0;
    any.family = PX_AF_INET6;
    int size = (int)sizeof(here);
    if (px.bind(s, &any, (int)sizeof(any)) != 0 || px.getsockname(s, &here, &size) != 0) {
        px.closesocket(s);
        log_append("PROXY: could not bind the world socket.\r\n");
        return proof_required ? -1 : 0;
    }
    px_local_port = (unsigned short)((here.port >> 8) | ((here.port & 0xff) << 8));
    px_socket = s;
    px_candidate_count = 0;
    px_initial=target;
    px_have_world = 0;
    for (unsigned i = 0; i < PX_STUN_SERVERS; ++i) px_mapped_seen[i] = 0;
    px_add_candidate(&target);
    if (!fi_launch_network_allowed()) {
        px_socket = PX_INVALID;
        px.closesocket(s);
        return proof_required ? -1 : 0;
    }
    LONG generation = InterlockedIncrement(&px_generation);
    HANDLE t = CreateThread(0, 0, px_thread, (void *)(intptr_t)generation, 0, 0);
    if (!t) {
        px.closesocket(s);
        px_socket = PX_INVALID;
        return proof_required ? -1 : 0;
    }
    CloseHandle(t);
    /* the game sends to [::1]:<our port> */
    PxAddr out;
    for (size_t i = 0; i < sizeof(out); ++i) ((unsigned char *)&out)[i] = 0;
    out.family = PX_AF_INET6;
    out.port = here.port;
    out.address[15] = 1;
    memory_copy(local, &out, sizeof(out));
    PxLine l;
    l.size = 0;
    l.text[0] = 0;
    px_text(&l, "PROXY: on, port ");
    px_number(&l, px_local_port);
    px_text(&l, "; ");
    px_number(&l, px_stun_count);
    px_text(&l, " STUN server address(es).\r\n");
    log_append(l.text);
    return 1;
}

/* The association ended: the proxy's thread closes its socket and ends. */
void azimuth_proxy_stop(void) {
    if (px_socket != PX_INVALID) {
        InterlockedIncrement(&px_generation);
        for (int i = 0; i < 40 && px_socket != PX_INVALID; ++i) {
            Sleep(10);
        }
    }
}

/* ------------------------------------------------------- TEST CONNECTION */

static void px_report(wchar_t *out, size_t cap, const wchar_t *text) {
    size_t n = 0;
    while (out[n] && n + 1 < cap) ++n;
    text = azimuth_tr(text);
    while (text && *text && n + 1 < cap) out[n++] = *text++;
    out[n] = 0;
}

static void px_report_raw(wchar_t *out, size_t cap, const char *text) {
    size_t n = 0;
    while (out[n] && n + 1 < cap) ++n;
    while (text && *text && n + 1 < cap) out[n++] = (wchar_t)(unsigned char)*text++;
    out[n] = 0;
}

/* The line of dedicated_server.net that starts with `key ` ("" when there is none). */
static int px_net_line(const char *text, const char *key, char *out, size_t cap) {
    size_t k = text_length(key);
    for (const char *line = text; *line;) {
        const char *end = line;
        while (*end && *end != '\n' && *end != '\r') ++end;
        if ((size_t)(end - line) > k && memory_equal(line, key, k) && line[k] == ' ') {
            size_t n = (size_t)(end - line) - k - 1;
            n = n < cap - 1 ? n : cap - 1;
            memory_copy(out, line + k + 1, n);
            out[n] = 0;
            return 1;
        }
        line = *end ? end + 1 : end;
    }
    out[0] = 0;
    return 0;
}

/* Settings > Infinity > TEST CONNECTION, free roam part: STUN from a socket of its own (this
   network's public address, IPv6, the NAT's kind) and, while hosting, what the relay found
   (dedicated_server.net). Takes about 1.5 s. */
void azimuth_proxy_nat_test(wchar_t *out, size_t cap) {
    if (!fi_launch_network_allowed()) return;
    px_report(out, cap, L"\n\nFREE ROAM (worlds over the Internet)");
    if (!px_load()) {
        return;
    }
    px_resolve_stun();
    UINT_PTR s = px.socket(PX_AF_INET6, PX_SOCK_DGRAM, PX_IPPROTO_UDP);
    PxAddr seen[PX_STUN_SERVERS];
    int got[PX_STUN_SERVERS] = {0};
    static unsigned char tx[PX_STUN_SERVERS][12], packet[PX_PACKET], from_raw[64];
    if (s != PX_INVALID) {
        DWORD off = 0, timeout = 100;
        px.setsockopt(s, PX_IPPROTO_IPV6, PX_IPV6_V6ONLY, (const char *)&off, sizeof(off));
        px.setsockopt(s, PX_SOL_SOCKET, PX_SO_RCVTIMEO, (const char *)&timeout, sizeof(timeout));
        PxAddr any;
        for (size_t i = 0; i < sizeof(any); ++i) ((unsigned char *)&any)[i] = 0;
        any.family = PX_AF_INET6;
        if (px.bind(s, &any, (int)sizeof(any)) == 0) {
            for (unsigned i = 0; i < px_stun_count; ++i) {
                if (!fi_launch_network_allowed()) break;
                unsigned char m[20] = {0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42};
                px_new_txid(tx[i]);
                memory_copy(m + 8, tx[i], 12);
                px.sendto(s, (const char *)m, 20, 0, &px_stun[i], (int)sizeof(px_stun[i]));
            }
            DWORD started = (DWORD)GetTickCount64();
            unsigned answered = 0;
            while (fi_launch_network_allowed() && answered < px_stun_count &&
                   (DWORD)GetTickCount64() - started < PX_STUN_WAIT_MS) {
                int from_size = (int)sizeof(from_raw);
                int n = px.recvfrom(s, (char *)packet, sizeof(packet), 0, from_raw, &from_size);
                if (n <= 0) continue;
                PxAddr from, mapped;
                px_from(from_raw, &from);
                unsigned kind = 0;
                const unsigned char *txid = 0;
                int has = 0;
                if (!px_stun_parse(packet, n, &kind, &txid, &mapped, &has) || kind != 0x0101 || !has) continue;
                for (unsigned i = 0; i < px_stun_count; ++i) {
                    if (!got[i] && memory_equal(txid, tx[i], 12) && px_same(&from, &px_stun[i])) {
                        got[i] = 1;
                        seen[i] = mapped;
                        ++answered;
                    }
                }
            }
        }
        px.closesocket(s);
    }
    /* IPv4 answers decide the NAT; any IPv6 answer means IPv6 works */
    int v4 = 0, v6 = 0, same = 1;
    PxAddr first4;
    for (unsigned i = 0; i < px_stun_count; ++i) {
        int server_v4 = px_stun[i].address[10] == 0xff && px_stun[i].address[11] == 0xff;
        if (!got[i]) continue;
        if (server_v4) {
            if (!v4) first4 = seen[i];
            else same &= px_same(&first4, &seen[i]);
            ++v4;
        } else {
            ++v6;
        }
    }
    if (!v4 && !v6) {
        px_report(out, cap, L"\nNo answer from the STUN servers: this network seems to block UDP. Playing with players outside "
                            L"your network will not work until it allows it (a firewall, or a school or work network).");
    } else {
        if (v4) {
            char text[72];
            px_format(&first4, text, sizeof(text));
            /* the port is this test's own: only the address means something to the player */
            for (size_t i = 0; text[i]; ++i) {
                if (text[i] == ':') {
                    text[i] = 0;
                    break;
                }
            }
            px_report(out, cap, L"\nYour address on the Internet: ");
            px_report_raw(out, cap, text);
        }
        px_report(out, cap, v6 ? L"\nIPv6: yes." : L"\nIPv6: no.");
        if (v4 >= 2 && same) {
            px_report(out, cap, L"\nNAT: moderate (cone). You can join most players' worlds and most can join yours, with no "
                                L"port to open.");
        } else if (v4 >= 2) {
            px_report(out, cap, L"\nNAT: strict (symmetric). You can join worlds whose host has a moderate or open NAT, but two "
                                L"strict NATs cannot meet. Fixes: turn UPnP on in the router, ask your operator for a public IP "
                                L"(CGNAT), or forward UDP 5101 to this PC when hosting.");
        } else {
            px_report(out, cap, L"\nNAT: not measured (only one STUN server answered).");
        }
    }
    /* the world this PC hosts */
    static char net[1024], value[160];
    if (azimuth_world_net_text(net, sizeof(net))) {
        px_report(out, cap, L"\n\nYOUR WORLD");
        if (px_net_line(net, "public", value, sizeof(value))) {
            px_report(out, cap, L"\nPlayers reach it at ");
            px_report_raw(out, cap, value);
            px_report(out, cap, L".");
        }
        px_net_line(net, "upnp", value, sizeof(value));
        if (!value[0] || memory_equal(value, "pending", 7)) {
            px_report(out, cap, L"\nUPnP: still asking the router.");
        } else if (memory_equal(value, "none", 4) || memory_equal(value, "off", 3)) {
            px_report(out, cap, L"\nUPnP: not available on this router (players still get in by hole punching).");
        } else if (memory_equal(value, "failed", 6)) {
            px_report(out, cap, L"\nUPnP: the router refused (");
            px_report_raw(out, cap, value + (text_length(value) > 7 ? 7 : text_length(value)));
            px_report(out, cap, L"); players still get in by hole punching.");
        } else {
            px_report(out, cap, L"\nUPnP: the router forwarded the world's port.");
        }
        if (px_net_line(net, "othernat", value, sizeof(value))) {
            px_report(out, cap, L"\nYour router is behind another router or the operator's CGNAT: players outside may not get "
                                L"in. Put the operator's modem in bridge mode, or ask for a public IP.");
        }
    }
    px_report(out, cap, L"\nGuide: github.com/anarqz/ForzaInfinity/blob/main/docs/FREEROAM.md");
}
