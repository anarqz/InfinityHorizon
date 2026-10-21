/* FIUP v1: grant-scoped control context and fixed HMAC codec. Never logs credentials. */
#include "infinity.h"
#include "../core/sha256.h"
#include "../core/support.h"

static SRWLOCK up_lock = SRWLOCK_INIT;
static unsigned char up_context[89];
static unsigned long up_serial;
static unsigned long long up_deadline;

void fi_udp_zero(void *memory, size_t size) {
    volatile unsigned char *p = memory;
    while (size--) *p++ = 0;
}
unsigned long long fi_udp_u64(const unsigned char *p) {
    unsigned long long n = 0;
    for (int i = 0; i < 8; ++i) n = (n << 8) | p[i];
    return n;
}
static int up_nonzero(const unsigned char *p, size_t n) {
    unsigned value = 0;
    for (size_t i = 0; i < n; ++i) value |= p[i];
    return value != 0;
}
void fi_udp_context_clear(void) {
    AcquireSRWLockExclusive(&up_lock);
    fi_udp_zero(up_context, sizeof(up_context)); up_deadline = 0; ++up_serial;
    ReleaseSRWLockExclusive(&up_lock);
}
int fi_udp_context_set(const unsigned char *p, size_t n) {
    if (n == 1 && p[0] == 0) { fi_udp_context_clear(); return 1; }
    unsigned long long now = fi_launch_server_unix();
    if (n != 89 || p[0] != 1 || !now || !up_nonzero(p+1,16) || !up_nonzero(p+17,16)
        || !fi_udp_u64(p+33) || !fi_udp_u64(p+41) || fi_udp_u64(p+41) != azimuth_account_xuid_value()
        || fi_udp_u64(p+49) <= now || fi_udp_u64(p+49)-now > 90) {
        fi_udp_context_clear(); return 0;
    }
    AcquireSRWLockExclusive(&up_lock);
    if (!memory_equal(up_context+1,p+1,48) || !memory_equal(up_context+57,p+57,32) || !up_context[0]) ++up_serial;
    memory_copy(up_context,p,89);
    unsigned long long left = fi_udp_u64(p+49)-now;
    if (left > 60) left = 60;
    up_deadline = GetTickCount64()+left*1000;
    ReleaseSRWLockExclusive(&up_lock); return 1;
}
int fi_udp_context_get(unsigned char *out, unsigned long *serial) {
    if (!fi_launch_network_allowed()) return 0;
    AcquireSRWLockExclusive(&up_lock);
    int ok = up_context[0] == 1 && GetTickCount64() < up_deadline;
    if (!ok && up_context[0]) { fi_udp_zero(up_context,89); ++up_serial; up_deadline=0; }
    if (ok) memory_copy(out,up_context,89);
    if (serial) *serial=up_serial;
    ReleaseSRWLockExclusive(&up_lock); return ok;
}
int fi_udp_random(unsigned char *out, size_t n) {
    typedef LONG(WINAPI *GenFn)(void *, unsigned char *, ULONG, ULONG);
    HMODULE module=LoadLibraryExW(L"bcrypt.dll",0,LOAD_LIBRARY_SEARCH_SYSTEM32);
    GenFn generate=module ? (GenFn)(uintptr_t)GetProcAddress(module,"BCryptGenRandom") : 0;
    int ok=generate && n<=1024 && generate(0,out,(ULONG)n,2)==0;
    if(module) FreeLibrary(module);
    return ok;
}
static const char *up_domain(unsigned type) {
    switch(type) {
    case 1:return "fi-udp-proof-v1/hello\n";
    case 2:return "fi-udp-proof-v1/challenge\n";
    case 3:return "fi-udp-proof-v1/response\n";
    case 4:return "fi-udp-proof-v1/ack\n";
    default:return 0;
    }
}
int fi_udp_valid(const unsigned char *p, size_t n) {
    if ((n!=104 && n!=147) || !memory_equal(p,"FIUP",4) || p[4]!=1 || !up_domain(p[5]) || p[6]>1 || p[7]
        || (p[5]==1 ? n!=104 : n!=147) || !up_nonzero(p+8,16) || !up_nonzero(p+24,16)
        || !fi_udp_u64(p+40) || !fi_udp_u64(p+48) || !up_nonzero(p+56,16)) return 0;
    if (p[5]!=1) {
        if(!up_nonzero(p+72,16) || (p[88]!=4 && p[88]!=6) || !(p[105]|p[106]) || !fi_udp_u64(p+107))return 0;
        if(p[88]==4 && (!p[89] || p[89]>=224 || up_nonzero(p+93,12)))return 0;
        if(p[88]==6 && (!up_nonzero(p+89,16) || p[89]==0xff))return 0;
    }
    return 1;
}
int fi_udp_sign(unsigned char *p, size_t n, const unsigned char *key) {
    const char *domain=up_domain(p[5]);
    if(!domain || (n!=104 && n!=147))return 0;
    unsigned char inner_pad[64],outer_pad[64],inner[32];
    for(int i=0;i<64;++i) { inner_pad[i]=(i<32 ? key[i] : 0)^0x36;outer_pad[i]=(i<32 ? key[i] : 0)^0x5c; }
    size_t domain_size=0;while(domain[domain_size])++domain_size;
    Sha256Context hash;sha256_initialize(&hash);sha256_update(&hash,inner_pad,64);sha256_update(&hash,(const unsigned char *)domain,domain_size);
    sha256_update(&hash,p,n-32);sha256_finalize(&hash,inner);sha256_initialize(&hash);sha256_update(&hash,outer_pad,64);
    sha256_update(&hash,inner,32);sha256_finalize(&hash,p+n-32);
    fi_udp_zero(inner_pad,64);fi_udp_zero(outer_pad,64);fi_udp_zero(inner,32);fi_udp_zero(&hash,sizeof(hash));return 1;
}
int fi_udp_verify(const unsigned char *p,size_t n,const unsigned char *key) {
    if(!fi_udp_valid(p,n))return 0;
    unsigned char copy[147];memory_copy(copy,p,n);fi_udp_sign(copy,n,key);
    unsigned diff=0;for(size_t i=n-32;i<n;++i)diff |= copy[i]^p[i];fi_udp_zero(copy,sizeof(copy));return diff==0;
}
