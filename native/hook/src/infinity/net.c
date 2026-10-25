/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
/* FH2Online network layer for the Azimuth adapter.

   Azimuth 0.9.7 binds the game's (and its XSAPI DLL's) CoCreateInstance to one
   host function at RVA 0x6dbe0. For CLSID_FreeThreadedXMLHTTP60 it returns
   azimuth::services::OfflineRequest, an IXMLHTTPRequest2 that always fails with
   INET_E_RESOURCE_NOT_FOUND, so FH2 never reaches the network. We detour that
   function and return NetRequest instead:

     Capture  log every request (method, URL, headers, body) to
              infinity-network.jsonl, then fail exactly like Azimuth (offline).
     Proxy    also forward it over plain HTTP to a test server and hand the
              server's status, headers and body back to the game.

   The audio device enumerator keeps Azimuth's own factory (RVA 0x144400);
   every other class is refused, as Azimuth would. Freestanding like the rest
   of the mod: Kernel32 only, WinHTTP and OLE32 are loaded on first use. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/sha256.h"
#include "offsets.h"
#include "../core/support.h"
#include <stddef.h>
#include <winhttp.h>

#define NET_CLSCTX_INPROC_SERVER 0x1u
#define NET_HOOK_HASHED_BYTES 0x20u
#define NET_HOOK_JUMP_BYTES 14u
#define NET_OFFLINE_ERROR ((HRESULT)0x800C0005L) /* INET_E_RESOURCE_NOT_FOUND */
#define NET_HEADER_NOT_FOUND ((HRESULT)0x80072F76L)
#define NET_UNEXPECTED ((HRESULT)0x8000FFFFL)
#define NET_MAX_BODY (64u << 20)
#define NET_LOG_NAME FI_NETWORK_LOG_NAME
#define NET_XHR_PROP_TIMEOUT 2

static const unsigned char net_clsid_xhr[16] = {0x09, 0x6a, 0xd9, 0x88, 0x92, 0xf1, 0xd4, 0x11,
                                                0xa6, 0x5f, 0x00, 0x40, 0x96, 0x32, 0x51, 0xe5};
static const unsigned char net_clsid_mmdevice[16] = {0x95, 0x03, 0xde, 0xbc, 0x2f, 0xe5,
                                                     0x7c, 0x46, 0x8e, 0x3d, 0xc4, 0x57,
                                                     0x92, 0x91, 0x69, 0x2e};
static const unsigned char net_iid_unknown[16] = {0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                                                  0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46};
static const unsigned char net_iid_xhr2[16] = {0xc0, 0x7d, 0xd3, 0xe5, 0x2a, 0x55, 0x52, 0x4d,
                                               0x9c, 0xc0, 0xa1, 0x4d, 0x54, 0x6f, 0xbd, 0x04};
static const unsigned char net_iid_agile[16] = {0x94, 0x2b, 0xea, 0x94, 0xcc, 0xe9, 0xe0, 0x49,
                                                0xc0, 0xff, 0xee, 0x64, 0xca, 0x8f, 0x5b, 0x90};
static const unsigned char net_iid_sequential_stream[16] = {0x30, 0x3a, 0x73, 0x0c, 0x1c, 0x2a,
                                                            0xce, 0x11, 0xad, 0xe5, 0x00, 0xaa,
                                                            0x00, 0x44, 0x77, 0x3d};
/* Where to hook this Azimuth build (azimuth_offsets.c); 0 on a build FH2Online was not made for. */
static const AzimuthOffsets *net_offsets;

enum { NET_MODE_OFF, NET_MODE_CAPTURE, NET_MODE_PROXY };

static int net_mode;
static wchar_t net_server_host[256];
static INTERNET_PORT net_server_port = 8081;
static int net_server_secure; /* https:// in [hook] Server */
static int net_auto_update = 1; /* [hook] AutoUpdate (update.c) */
static wchar_t net_log_path[PATH_CAPACITY];
static unsigned char *net_azimuth_base;
static volatile LONG net_next_id;
static SRWLOCK net_log_lock = SRWLOCK_INIT;
static SRWLOCK net_api_lock = SRWLOCK_INIT;

/* ---------------------------------------------------------------- buffers */

typedef struct {
    unsigned char *data;
    size_t size, capacity;
} Buffer;

static void *net_alloc(size_t size) {
    return HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, size);
}

static void net_free(void *pointer) {
    if (pointer) {
        HeapFree(GetProcessHeap(), 0, pointer);
    }
}

/* Keeps two zero bytes after the content so text buffers stay terminated. */
static int buffer_reserve(Buffer *buffer, size_t extra) {
    if (extra > (size_t)-1 - buffer->size - 2) {
        return 0;
    }
    size_t need = buffer->size + extra + 2, capacity = buffer->capacity ? buffer->capacity : 256;
    if (need <= buffer->capacity) {
        return 1;
    }
    while (capacity < need) {
        if (capacity > (size_t)-1 / 2) {
            return 0;
        }
        capacity *= 2;
    }
    unsigned char *data = net_alloc(capacity);
    if (!data) {
        return 0;
    }
    if (buffer->size) {
        memory_copy(data, buffer->data, buffer->size);
    }
    net_free(buffer->data);
    buffer->data = data;
    buffer->capacity = capacity;
    return 1;
}

static int buffer_append(Buffer *buffer, const void *source, size_t size) {
    if (!buffer_reserve(buffer, size)) {
        return 0;
    }
    if (size) {
        memory_copy(buffer->data + buffer->size, source, size);
    }
    buffer->size += size;
    buffer->data[buffer->size] = 0;
    buffer->data[buffer->size + 1] = 0;
    return 1;
}

static int buffer_append_text(Buffer *buffer, const char *text) {
    return buffer_append(buffer, text, text_length(text));
}

static void buffer_free(Buffer *buffer) {
    net_free(buffer->data);
    buffer->data = 0;
    buffer->size = buffer->capacity = 0;
}

static size_t wide_length(const wchar_t *text) {
    size_t length = 0;
    while (text && text[length]) {
        ++length;
    }
    return length;
}

static int buffer_append_wide(Buffer *buffer, const wchar_t *text) {
    return buffer_append(buffer, text, wide_length(text) * sizeof(wchar_t));
}

static const wchar_t *buffer_wide(const Buffer *buffer) {
    return buffer->data ? (const wchar_t *)buffer->data : L"";
}

static wchar_t wide_lower(wchar_t c) {
    return c >= L'A' && c <= L'Z' ? (wchar_t)(c + (L'a' - L'A')) : c;
}

static int wide_starts_ignore_case(const wchar_t *text, size_t length, const wchar_t *prefix) {
    size_t prefix_length = wide_length(prefix);
    if (prefix_length > length) {
        return 0;
    }
    for (size_t i = 0; i < prefix_length; ++i) {
        if (wide_lower(text[i]) != wide_lower(prefix[i])) {
            return 0;
        }
    }
    return 1;
}

/* ------------------------------------------------------- lazily loaded APIs */

typedef void *(WINAPI *CoTaskMemAllocFn)(SIZE_T);
typedef HINTERNET(WINAPI *WinHttpOpenFn)(LPCWSTR, DWORD, LPCWSTR, LPCWSTR, DWORD);
typedef HINTERNET(WINAPI *WinHttpConnectFn)(HINTERNET, LPCWSTR, INTERNET_PORT, DWORD);
typedef HINTERNET(WINAPI *WinHttpOpenRequestFn)(HINTERNET, LPCWSTR, LPCWSTR, LPCWSTR, LPCWSTR,
                                                LPCWSTR *, DWORD);
typedef BOOL(WINAPI *WinHttpAddRequestHeadersFn)(HINTERNET, LPCWSTR, DWORD, DWORD);
typedef BOOL(WINAPI *WinHttpSendRequestFn)(HINTERNET, LPCWSTR, DWORD, LPVOID, DWORD, DWORD,
                                           DWORD_PTR);
typedef BOOL(WINAPI *WinHttpReceiveResponseFn)(HINTERNET, LPVOID);
typedef BOOL(WINAPI *WinHttpQueryHeadersFn)(HINTERNET, DWORD, LPCWSTR, LPVOID, LPDWORD, LPDWORD);
typedef BOOL(WINAPI *WinHttpQueryDataAvailableFn)(HINTERNET, LPDWORD);
typedef BOOL(WINAPI *WinHttpReadDataFn)(HINTERNET, LPVOID, DWORD, LPDWORD);
typedef BOOL(WINAPI *WinHttpCloseHandleFn)(HINTERNET);
typedef BOOL(WINAPI *WinHttpSetTimeoutsFn)(HINTERNET, int, int, int, int);

static int net_ole_state, net_http_state; /* 0 untried, 1 ready, -1 unavailable */
static CoTaskMemAllocFn net_co_task_mem_alloc;
static WinHttpOpenFn http_open;
static WinHttpConnectFn http_connect;
static WinHttpOpenRequestFn http_open_request;
static WinHttpAddRequestHeadersFn http_add_headers;
static WinHttpSendRequestFn http_send;
static WinHttpReceiveResponseFn http_receive;
static WinHttpQueryHeadersFn http_query_headers;
static WinHttpQueryDataAvailableFn http_query_available;
static WinHttpReadDataFn http_read;
static WinHttpCloseHandleFn http_close;
static WinHttpSetTimeoutsFn http_set_timeouts;

/* The game frees response-header strings with OLE32's CoTaskMemFree, which
   Azimuth forwards to the real ole32.dll; allocate from the same place. */
static void *net_task_alloc(size_t size) {
    AcquireSRWLockExclusive(&net_api_lock);
    if (!net_ole_state) {
        HMODULE ole = LoadLibraryExW(L"ole32.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
        net_co_task_mem_alloc =
            ole ? (CoTaskMemAllocFn)(uintptr_t)GetProcAddress(ole, "CoTaskMemAlloc") : 0;
        net_ole_state = net_co_task_mem_alloc ? 1 : -1;
    }
    ReleaseSRWLockExclusive(&net_api_lock);
    return net_ole_state > 0 ? net_co_task_mem_alloc(size) : 0;
}

static int net_load_winhttp(void) {
    AcquireSRWLockExclusive(&net_api_lock);
    if (!net_http_state) {
        HMODULE http = LoadLibraryExW(L"winhttp.dll", 0, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (http) {
            http_open = (WinHttpOpenFn)(uintptr_t)GetProcAddress(http, "WinHttpOpen");
            http_connect = (WinHttpConnectFn)(uintptr_t)GetProcAddress(http, "WinHttpConnect");
            http_open_request =
                (WinHttpOpenRequestFn)(uintptr_t)GetProcAddress(http, "WinHttpOpenRequest");
            http_add_headers = (WinHttpAddRequestHeadersFn)(uintptr_t)GetProcAddress(
                http, "WinHttpAddRequestHeaders");
            http_send = (WinHttpSendRequestFn)(uintptr_t)GetProcAddress(http, "WinHttpSendRequest");
            http_receive = (WinHttpReceiveResponseFn)(uintptr_t)GetProcAddress(
                http, "WinHttpReceiveResponse");
            http_query_headers =
                (WinHttpQueryHeadersFn)(uintptr_t)GetProcAddress(http, "WinHttpQueryHeaders");
            http_query_available = (WinHttpQueryDataAvailableFn)(uintptr_t)GetProcAddress(
                http, "WinHttpQueryDataAvailable");
            http_read = (WinHttpReadDataFn)(uintptr_t)GetProcAddress(http, "WinHttpReadData");
            http_close = (WinHttpCloseHandleFn)(uintptr_t)GetProcAddress(http, "WinHttpCloseHandle");
            http_set_timeouts =
                (WinHttpSetTimeoutsFn)(uintptr_t)GetProcAddress(http, "WinHttpSetTimeouts");
        }
        net_http_state = http && http_open && http_connect && http_open_request &&
                                 http_add_headers && http_send && http_receive &&
                                 http_query_headers && http_query_available && http_read &&
                                 http_close && http_set_timeouts
                             ? 1
                             : -1;
    }
    ReleaseSRWLockExclusive(&net_api_lock);
    return net_http_state > 0;
}

/* ------------------------------------------------------------ JSON capture */

static int buffer_append_json_utf8(Buffer *out, const unsigned char *text, size_t size) {
    static const char hex[] = "0123456789abcdef";
    if (!buffer_append(out, "\"", 1)) {
        return 0;
    }
    for (size_t i = 0; i < size; ++i) {
        unsigned char c = text[i];
        char escaped[7] = {'\\', 'u', '0', '0', hex[c >> 4], hex[c & 15], 0};
        int ok;
        if (c == '"' || c == '\\') {
            char pair[2] = {'\\', (char)c};
            ok = buffer_append(out, pair, 2);
        } else if (c < 0x20) {
            ok = buffer_append(out, escaped, 6);
        } else {
            ok = buffer_append(out, &c, 1);
        }
        if (!ok) {
            return 0;
        }
    }
    return buffer_append(out, "\"", 1);
}

static int buffer_append_json_wide(Buffer *out, const wchar_t *text, size_t length) {
    if (!length) {
        return buffer_append_text(out, "\"\"");
    }
    int bytes = WideCharToMultiByte(CP_UTF8, 0, text, (int)length, 0, 0, 0, 0);
    if (bytes <= 0) {
        return buffer_append_text(out, "\"\"");
    }
    unsigned char *utf8 = net_alloc((size_t)bytes);
    if (!utf8) {
        return 0;
    }
    WideCharToMultiByte(CP_UTF8, 0, text, (int)length, (char *)utf8, bytes, 0, 0);
    int ok = buffer_append_json_utf8(out, utf8, (size_t)bytes);
    net_free(utf8);
    return ok;
}

static int buffer_append_base64(Buffer *out, const unsigned char *data, size_t size) {
    static const char alphabet[] =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if (!buffer_append(out, "\"", 1)) {
        return 0;
    }
    for (size_t i = 0; i < size; i += 3) {
        unsigned value = (unsigned)data[i] << 16;
        size_t remaining = size - i;
        if (remaining > 1) {
            value |= (unsigned)data[i + 1] << 8;
        }
        if (remaining > 2) {
            value |= data[i + 2];
        }
        char quad[4] = {alphabet[(value >> 18) & 63], alphabet[(value >> 12) & 63],
                        remaining > 1 ? alphabet[(value >> 6) & 63] : '=',
                        remaining > 2 ? alphabet[value & 63] : '='};
        if (!buffer_append(out, quad, 4)) {
            return 0;
        }
    }
    return buffer_append(out, "\"", 1);
}

static int buffer_append_number(Buffer *out, unsigned long long value) {
    char digits[24];
    unsigned count = 0;
    do {
        digits[sizeof(digits) - 1 - count++] = (char)('0' + value % 10);
        value /= 10;
    } while (value && count < sizeof(digits));
    return buffer_append(out, digits + sizeof(digits) - count, count);
}

static int buffer_append_hresult(Buffer *out, HRESULT value) {
    static const char hex[] = "0123456789ABCDEF";
    char text[13] = {'"', '0', 'x'};
    unsigned bits = (unsigned)value;
    for (unsigned i = 0; i < 8; ++i) {
        text[3 + i] = hex[(bits >> (28 - 4 * i)) & 15];
    }
    text[11] = '"';
    return buffer_append(out, text, 12);
}

/* ------------------------------------------------------- response streams */

typedef struct SequentialStream SequentialStream;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(SequentialStream *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(SequentialStream *);
    ULONG(STDMETHODCALLTYPE *Release)(SequentialStream *);
    HRESULT(STDMETHODCALLTYPE *Read)(SequentialStream *, void *, ULONG, ULONG *);
    HRESULT(STDMETHODCALLTYPE *Write)(SequentialStream *, const void *, ULONG, ULONG *);
} SequentialStreamVtbl;
struct SequentialStream {
    const SequentialStreamVtbl *lpVtbl;
};

typedef struct {
    const SequentialStreamVtbl *lpVtbl;
    volatile LONG references;
    Buffer data;
    size_t position;
} MemoryStream;

static HRESULT STDMETHODCALLTYPE memory_stream_query(SequentialStream *self, REFIID iid,
                                                     void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_sequential_stream, 16)) {
        *out = self;
        self->lpVtbl->AddRef(self);
        return S_OK;
    }
    log_append("NETWORK: response stream asked for an interface other than ISequentialStream.\r\n");
    return E_NOINTERFACE;
}

static ULONG STDMETHODCALLTYPE memory_stream_add_ref(SequentialStream *self) {
    return (ULONG)InterlockedIncrement(&((MemoryStream *)self)->references);
}

static ULONG STDMETHODCALLTYPE memory_stream_release(SequentialStream *self) {
    MemoryStream *stream = (MemoryStream *)self;
    LONG left = InterlockedDecrement(&stream->references);
    if (!left) {
        buffer_free(&stream->data);
        net_free(stream);
    }
    return (ULONG)left;
}

static HRESULT STDMETHODCALLTYPE memory_stream_read(SequentialStream *self, void *output,
                                                    ULONG size, ULONG *read) {
    MemoryStream *stream = (MemoryStream *)self;
    size_t available = stream->data.size - stream->position, count = size;
    if (!output && size) {
        return STG_E_INVALIDPOINTER;
    }
    if (count > available) {
        count = available;
    }
    if (count) {
        memory_copy(output, stream->data.data + stream->position, count);
    }
    stream->position += count;
    if (read) {
        *read = (ULONG)count;
    }
    return count == size ? S_OK : S_FALSE;
}

static HRESULT STDMETHODCALLTYPE memory_stream_write(SequentialStream *self, const void *input,
                                                     ULONG size, ULONG *written) {
    (void)self;
    (void)input;
    (void)size;
    if (written) {
        *written = 0;
    }
    return STG_E_ACCESSDENIED;
}

static const SequentialStreamVtbl memory_stream_vtable = {
    memory_stream_query, memory_stream_add_ref, memory_stream_release, memory_stream_read,
    memory_stream_write};

static SequentialStream *memory_stream_create(const Buffer *body) {
    MemoryStream *stream = net_alloc(sizeof(*stream));
    if (!stream) {
        return 0;
    }
    stream->lpVtbl = &memory_stream_vtable;
    stream->references = 1;
    if (body->size && !buffer_append(&stream->data, body->data, body->size)) {
        net_free(stream);
        return 0;
    }
    return (SequentialStream *)stream;
}

/* ------------------------------------------------------------ the request */

typedef struct NetRequest NetRequest;
typedef struct XhrCallback XhrCallback;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(XhrCallback *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(XhrCallback *);
    ULONG(STDMETHODCALLTYPE *Release)(XhrCallback *);
    HRESULT(STDMETHODCALLTYPE *OnRedirect)(XhrCallback *, NetRequest *, const WCHAR *);
    HRESULT(STDMETHODCALLTYPE *OnHeadersAvailable)(XhrCallback *, NetRequest *, DWORD,
                                                   const WCHAR *);
    HRESULT(STDMETHODCALLTYPE *OnDataAvailable)(XhrCallback *, NetRequest *, SequentialStream *);
    HRESULT(STDMETHODCALLTYPE *OnResponseReceived)(XhrCallback *, NetRequest *,
                                                   SequentialStream *);
    HRESULT(STDMETHODCALLTYPE *OnError)(XhrCallback *, NetRequest *, HRESULT);
} XhrCallbackVtbl;
struct XhrCallback {
    const XhrCallbackVtbl *lpVtbl;
};

typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetRequest *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetRequest *);
    ULONG(STDMETHODCALLTYPE *Release)(NetRequest *);
    HRESULT(STDMETHODCALLTYPE *Open)(NetRequest *, const WCHAR *, const WCHAR *, XhrCallback *,
                                     const WCHAR *, const WCHAR *, const WCHAR *, const WCHAR *);
    HRESULT(STDMETHODCALLTYPE *Send)(NetRequest *, SequentialStream *, ULONGLONG);
    HRESULT(STDMETHODCALLTYPE *Abort)(NetRequest *);
    HRESULT(STDMETHODCALLTYPE *SetCookie)(NetRequest *, const void *, DWORD *);
    HRESULT(STDMETHODCALLTYPE *SetCustomResponseStream)(NetRequest *, SequentialStream *);
    HRESULT(STDMETHODCALLTYPE *SetProperty)(NetRequest *, int, ULONGLONG);
    HRESULT(STDMETHODCALLTYPE *SetRequestHeader)(NetRequest *, const WCHAR *, const WCHAR *);
    HRESULT(STDMETHODCALLTYPE *GetAllResponseHeaders)(NetRequest *, WCHAR **);
    HRESULT(STDMETHODCALLTYPE *GetCookie)(NetRequest *, const WCHAR *, const WCHAR *, DWORD,
                                          DWORD *, void **);
    HRESULT(STDMETHODCALLTYPE *GetResponseHeader)(NetRequest *, const WCHAR *, WCHAR **);
} NetRequestVtbl;

struct NetRequest {
    const NetRequestVtbl *lpVtbl;
    volatile LONG references, sent, aborted, opened;
    LONG id;
    DWORD timeout_ms;
    Buffer method, url, request_headers, request_body;
    XhrCallback *callback;
    SequentialStream *custom_stream;
    DWORD status;
    Buffer status_text, response_headers, response_body;
};

static void net_request_destroy(NetRequest *request) {
    if (request->callback) {
        request->callback->lpVtbl->Release(request->callback);
    }
    if (request->custom_stream) {
        request->custom_stream->lpVtbl->Release(request->custom_stream);
    }
    buffer_free(&request->method);
    buffer_free(&request->url);
    buffer_free(&request->request_headers);
    buffer_free(&request->request_body);
    buffer_free(&request->status_text);
    buffer_free(&request->response_headers);
    buffer_free(&request->response_body);
    net_free(request);
}

static HRESULT STDMETHODCALLTYPE net_request_query(NetRequest *self, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_xhr2, 16) ||
        memory_equal(iid, net_iid_agile, 16)) {
        *out = self;
        InterlockedIncrement(&self->references);
        return S_OK;
    }
    return E_NOINTERFACE;
}

static ULONG STDMETHODCALLTYPE net_request_add_ref(NetRequest *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}

static ULONG STDMETHODCALLTYPE net_request_release(NetRequest *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        net_request_destroy(self);
    }
    return (ULONG)left;
}

static void net_log_open(NetRequest *request);

static HRESULT STDMETHODCALLTYPE net_request_open(NetRequest *self, const WCHAR *method,
                                                  const WCHAR *url, XhrCallback *callback,
                                                  const WCHAR *user, const WCHAR *password,
                                                  const WCHAR *proxy_user,
                                                  const WCHAR *proxy_password) {
    (void)user;
    (void)password;
    (void)proxy_user;
    (void)proxy_password;
    if (!method || !url || !callback) {
        return E_INVALIDARG;
    }
    if (self->sent || InterlockedCompareExchange(&self->opened, 1, 0)) {
        return NET_UNEXPECTED;
    }
    if (!buffer_append_wide(&self->method, method) || !buffer_append_wide(&self->url, url)) {
        InterlockedExchange(&self->opened, 0);
        return E_OUTOFMEMORY;
    }
    callback->lpVtbl->AddRef(callback);
    self->callback = callback;
    net_log_open(self);
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_set_header(NetRequest *self, const WCHAR *name,
                                                        const WCHAR *value) {
    if (!name || !*name) {
        return E_INVALIDARG;
    }
    if (!buffer_append_wide(&self->request_headers, name) ||
        !buffer_append_wide(&self->request_headers, L": ") ||
        !buffer_append_wide(&self->request_headers, value ? value : L"") ||
        !buffer_append_wide(&self->request_headers, L"\r\n")) {
        return E_OUTOFMEMORY;
    }
    return S_OK;
}

static int net_read_body(NetRequest *request, SequentialStream *body, ULONGLONG size) {
    unsigned char chunk[2048];
    ULONGLONG total = 0;
    if (!body || !size) {
        return 1; /* XHR2: size 0 means no body. */
    }
    if (size > NET_MAX_BODY) {
        return 0;
    }
    while (total < size) {
        ULONG want = sizeof(chunk), got = 0;
        if (size && size - total < want) {
            want = (ULONG)(size - total);
        }
        HRESULT result = body->lpVtbl->Read(body, chunk, want, &got);
        if (FAILED(result)) {
            return 0;
        }
        if (got && !buffer_append(&request->request_body, chunk, got)) {
            return 0;
        }
        total += got;
        if (!got || result == S_FALSE) {
            break;
        }
    }
    return 1;
}

/* Diagnostics in infinity.log: first occurrence of each event. */
static volatile LONG net_seen_xhr, net_seen_mmdevice, net_seen_write_error, net_logged_opens;

static void net_log_once(volatile LONG *flag, const char *message) {
    if (!InterlockedCompareExchange(flag, 1, 0)) {
        log_append(message);
    }
}

static void net_log_guid(const char *prefix, const unsigned char *guid) {
    static const char digits[] = "0123456789abcdef";
    Buffer line = {0};
    char hex[33];
    for (unsigned i = 0; i < 16; ++i) {
        hex[2 * i] = digits[guid[i] >> 4];
        hex[2 * i + 1] = digits[guid[i] & 15];
    }
    hex[32] = 0;
    if (buffer_append_text(&line, prefix) && buffer_append_text(&line, hex) &&
        buffer_append_text(&line, " (raw GUID bytes)\r\n")) {
        log_append((const char *)line.data);
    }
    buffer_free(&line);
}

static void net_log_open(NetRequest *request) {
    if (InterlockedIncrement(&net_logged_opens) > 64) {
        return;
    }
    Buffer line = {0};
    if (buffer_append_text(&line, "NETWORK: open #") &&
        buffer_append_number(&line, (ULONGLONG)request->id) && buffer_append_text(&line, " ") &&
        buffer_append_json_wide(&line, buffer_wide(&request->method),
                                request->method.size / sizeof(wchar_t)) &&
        buffer_append_text(&line, " ") &&
        buffer_append_json_wide(&line, buffer_wide(&request->url),
                                request->url.size / sizeof(wchar_t)) &&
        buffer_append_text(&line, "\r\n")) {
        log_append((const char *)line.data);
    }
    buffer_free(&line);
}

static void net_capture(NetRequest *request, int answered, HRESULT error) {
    Buffer line = {0};
    int ok =
        buffer_append_text(&line, "{\"id\":") && buffer_append_number(&line, (ULONGLONG)request->id) &&
        buffer_append_text(&line, ",\"tick_ms\":") && buffer_append_number(&line, GetTickCount64()) &&
        buffer_append_text(&line, net_mode == NET_MODE_PROXY ? ",\"mode\":\"proxy\""
                                                             : ",\"mode\":\"capture\"") &&
        buffer_append_text(&line, ",\"method\":") &&
        buffer_append_json_wide(&line, buffer_wide(&request->method),
                                request->method.size / sizeof(wchar_t)) &&
        buffer_append_text(&line, ",\"url\":") &&
        buffer_append_json_wide(&line, buffer_wide(&request->url),
                                request->url.size / sizeof(wchar_t)) &&
        buffer_append_text(&line, ",\"request_headers\":") &&
        buffer_append_json_wide(&line, buffer_wide(&request->request_headers),
                                request->request_headers.size / sizeof(wchar_t)) &&
        buffer_append_text(&line, ",\"request_body_b64\":") &&
        buffer_append_base64(&line, request->request_body.data, request->request_body.size);
    if (ok && answered) {
        ok = buffer_append_text(&line, ",\"outcome\":\"response\",\"status\":") &&
             buffer_append_number(&line, request->status) &&
             buffer_append_text(&line, ",\"status_text\":") &&
             buffer_append_json_wide(&line, buffer_wide(&request->status_text),
                                     request->status_text.size / sizeof(wchar_t)) &&
             buffer_append_text(&line, ",\"response_headers\":") &&
             buffer_append_json_wide(&line, buffer_wide(&request->response_headers),
                                     request->response_headers.size / sizeof(wchar_t)) &&
             buffer_append_text(&line, ",\"response_body_b64\":") &&
             buffer_append_base64(&line, request->response_body.data, request->response_body.size);
    } else if (ok) {
        ok = buffer_append_text(&line, error == NET_OFFLINE_ERROR ? ",\"outcome\":\"offline\""
                                                                  : ",\"outcome\":\"error\"") &&
             buffer_append_text(&line, ",\"hresult\":") && buffer_append_hresult(&line, error);
    }
    if (ok) {
        ok = buffer_append_text(&line, "}\r\n");
    }
    if (ok && net_log_path[0]) {
        AcquireSRWLockExclusive(&net_log_lock);
        HANDLE file = CreateFileW(net_log_path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
                                  NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
        if (file != INVALID_HANDLE_VALUE) {
            DWORD written;
            WriteFile(file, line.data, (DWORD)line.size, &written, NULL);
            CloseHandle(file);
        } else {
            net_log_once(&net_seen_write_error, "NETWORK: cannot open infinity-network.jsonl for append.\r\n");
        }
        ReleaseSRWLockExclusive(&net_log_lock);
    }
    buffer_free(&line);
}

/* Headers WinHTTP manages itself are logged but not forwarded. */
static int net_header_is_forwarded(const wchar_t *line, size_t length) {
    static const wchar_t *const skipped[] = {L"host:", L"content-length:", L"connection:",
                                             L"transfer-encoding:", L"accept-encoding:"};
    for (size_t i = 0; i < ARRAY_COUNT(skipped); ++i) {
        if (wide_starts_ignore_case(line, length, skipped[i])) {
            return 0;
        }
    }
    return 1;
}

static int net_query_text(HINTERNET request, DWORD query, Buffer *out) {
    DWORD size = 0;
    http_query_headers(request, query, WINHTTP_HEADER_NAME_BY_INDEX, WINHTTP_NO_OUTPUT_BUFFER,
                       &size, WINHTTP_NO_HEADER_INDEX);
    if (GetLastError() != ERROR_INSUFFICIENT_BUFFER || !size || !buffer_reserve(out, size)) {
        return 0;
    }
    if (!http_query_headers(request, query, WINHTTP_HEADER_NAME_BY_INDEX, out->data, &size,
                            WINHTTP_NO_HEADER_INDEX)) {
        return 0;
    }
    out->size = size;
    out->data[out->size] = out->data[out->size + 1] = 0;
    return 1;
}

/* Forward to the test server as http://SERVER/<original host + path>. */
static int net_forward(NetRequest *request, HRESULT *error) {
    if (net_mode != NET_MODE_PROXY || fi_cfg()->launch == FI_LAUNCH_OFFLINE) {
        return 0; /* OD-118: never on Play offline */
    }
    HINTERNET session = 0, connection = 0, http = 0;
    Buffer path = {0}, headers = {0}, raw = {0};
    int answered = 0;
    *error = NET_OFFLINE_ERROR;
    const wchar_t *url = buffer_wide(&request->url), *rest = url;
    for (const wchar_t *p = url; p[0] && p[1] && p[2]; ++p) {
        if (p[0] == L':' && p[1] == L'/' && p[2] == L'/') {
            rest = p + 3;
            break;
        }
    }
    /* a launcher start forwards only within the launcher's session (offline otherwise) */
    if ((fi_launch_protocol() && !fi_launch_live()) || !net_server_host[0]) {
        goto done;
    }
    if (!net_load_winhttp() || !buffer_append_wide(&path, L"/") ||
        !buffer_append_wide(&path, rest)) {
        goto done;
    }
    /* Original headers minus the ones WinHTTP owns, plus where it was going. */
    for (const wchar_t *line = buffer_wide(&request->request_headers); *line;) {
        size_t length = 0;
        while (line[length] && line[length] != L'\n') {
            ++length;
        }
        size_t taken = line[length] ? length + 1 : length;
        if (net_header_is_forwarded(line, length) &&
            !buffer_append(&headers, line, taken * sizeof(wchar_t))) {
            goto done;
        }
        line += taken;
    }
    char id_text[24];
    unsigned count = 0;
    for (ULONG value = (ULONG)request->id; count == 0 || value; value /= 10) {
        id_text[sizeof(id_text) - 1 - count++] = (char)('0' + value % 10);
    }
    wchar_t id_wide[24];
    for (unsigned i = 0; i < count; ++i) {
        id_wide[i] = (wchar_t)id_text[sizeof(id_text) - count + i];
    }
    id_wide[count] = 0;
    if (!buffer_append_wide(&headers, L"X-Fh2-Original-Url: ") ||
        !buffer_append_wide(&headers, url) ||
        !buffer_append_wide(&headers, L"\r\nX-Fh2-Request-Id: ") ||
        !buffer_append_wide(&headers, id_wide) || !buffer_append_wide(&headers, L"\r\n")) {
        goto done;
    }
    /* The session (X-Fi-Session, X-Fi-Hook) or the Beta account's token (X-Fh2-Token): only ever
       sent to the server it came from. */
    if (azimuth_account_token()) {
        wchar_t *auth = net_alloc(1200 * sizeof(wchar_t));
        int added = auth && azimuth_account_auth_header(auth, 1200) && buffer_append_wide(&headers, auth) &&
                    buffer_append_wide(&headers, L"\r\n");
        net_free(auth);
        if (!added && fi_launch_protocol()) {
            goto done; /* the session ended in between: offline */
        }
    }
    /* the car database's SHA-256: the server only lets the game's own cars play online, and free
       roam worlds only join the same cars (azimuth_world.c) */
    azimuth_world_car_db_wait(15000);
    const wchar_t *car_db = azimuth_world_car_db_header();
    if (car_db && (!buffer_append_wide(&headers, car_db) || !buffer_append_wide(&headers, L"\r\n"))) {
        goto done;
    }
    /* OD-121: what the player is doing, on the game's 30 s RetrieveForUser (activity.c) */
    wchar_t activity[96];
    if (fi_launch_protocol() &&
        fi_activity_header(buffer_wide(&request->request_headers), activity, ARRAY_COUNT(activity)) &&
        (!buffer_append_wide(&headers, activity) || !buffer_append_wide(&headers, L"\r\n"))) {
        goto done;
    }
    session = http_open(L"FH2Online/1", WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_NO_PROXY_NAME,
                        WINHTTP_NO_PROXY_BYPASS, 0);
    if (!session) {
        goto done;
    }
    int receive_timeout = request->timeout_ms ? (int)request->timeout_ms : 30000;
    http_set_timeouts(session, 5000, 5000, 30000, receive_timeout);
    connection = http_connect(session, net_server_host, net_server_port, 0);
    http = connection ? http_open_request(connection, buffer_wide(&request->method),
                                          buffer_wide(&path), NULL, WINHTTP_NO_REFERER,
                                          WINHTTP_DEFAULT_ACCEPT_TYPES,
                                          net_server_secure ? WINHTTP_FLAG_SECURE : 0)
                      : 0;
    if (!http) {
        goto done;
    }
    /* One header per call: a single odd line must not sink the whole request. */
    for (const wchar_t *line = buffer_wide(&headers); *line;) {
        size_t length = 0;
        while (line[length] && line[length] != L'\n') {
            ++length;
        }
        size_t taken = line[length] ? length + 1 : length;
        size_t trimmed = length;
        while (trimmed && (line[trimmed - 1] == L'\r' || line[trimmed - 1] == L' ')) {
            --trimmed;
        }
        if (trimmed && !http_add_headers(http, line, (DWORD)trimmed,
                                         WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE)) {
            log_append("NETWORK: WinHTTP rejected one forwarded request header.\r\n");
        }
        line += taken;
    }
    DWORD body_size = (DWORD)request->request_body.size;
    if (fi_launch_protocol() && !fi_launch_live()) goto done;
    if (!http_send(http, WINHTTP_NO_ADDITIONAL_HEADERS, 0,
                   body_size ? request->request_body.data : WINHTTP_NO_REQUEST_DATA, body_size,
                   body_size, 0) ||
        !http_receive(http, NULL)) {
        goto done;
    }
    if (fi_launch_protocol() && !fi_launch_live()) goto done;
    DWORD status = 0, status_size = sizeof(status);
    if (!http_query_headers(http, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                            WINHTTP_HEADER_NAME_BY_INDEX, &status, &status_size,
                            WINHTTP_NO_HEADER_INDEX)) {
        goto done;
    }
    request->status = status;
    net_query_text(http, WINHTTP_QUERY_STATUS_TEXT, &request->status_text);
    /* Raw CRLF headers start with the status line; the game expects only fields. */
    if (net_query_text(http, WINHTTP_QUERY_RAW_HEADERS_CRLF, &raw)) {
        const wchar_t *fields = buffer_wide(&raw);
        while (*fields && *fields != L'\n') {
            ++fields;
        }
        if (*fields) {
            ++fields;
        }
        size_t length = wide_length(fields);
        while (length >= 2 && fields[length - 1] == L'\n' && fields[length - 2] == L'\r' &&
               length >= 4 && fields[length - 3] == L'\n') {
            length -= 2; /* drop the blank line that ends the header block */
        }
        buffer_append(&request->response_headers, fields, length * sizeof(wchar_t));
        if (fi_launch_protocol()) {
            fi_activity_answer(buffer_wide(&request->response_headers));
        }
    }
    for (;;) {
        if (fi_launch_protocol() && !fi_launch_live()) goto done;
        DWORD available = 0, read = 0;
        if (!http_query_available(http, &available)) {
            *error = HRESULT_FROM_WIN32(GetLastError());
            goto done;
        }
        if (!available) {
            break;
        }
        if (request->response_body.size + available > NET_MAX_BODY ||
            !buffer_reserve(&request->response_body, available)) {
            *error = E_OUTOFMEMORY;
            goto done;
        }
        if (!http_read(http, request->response_body.data + request->response_body.size, available,
                       &read)) {
            *error = HRESULT_FROM_WIN32(GetLastError());
            goto done;
        }
        request->response_body.size += read;
        request->response_body.data[request->response_body.size] = 0;
    }
    if (fi_launch_refused(status)) {
        /* CR-S4: the game falls back to offline play as it does without a profile */
        static const char offline[] = "no launcher session";
        request->status = 503;
        buffer_free(&request->status_text);
        buffer_free(&request->response_headers);
        buffer_free(&request->response_body);
        if (!buffer_append_wide(&request->status_text, L"Service Unavailable") ||
            !buffer_append(&request->response_body, offline, sizeof(offline) - 1)) {
            goto done;
        }
    }
    answered = 1;
done:
    if (http) {
        http_close(http);
    }
    if (connection) {
        http_close(connection);
    }
    if (session) {
        http_close(session);
    }
    buffer_free(&path);
    buffer_free(&headers);
    buffer_free(&raw);
    return answered;
}

/* "host:port" of [hook] Server, for the account module (device id, token file). */
const wchar_t *azimuth_net_server_name(void) {
    static wchar_t name[300];
    size_t length = wide_length(net_server_host);
    if (!net_server_host[0] || length + 8 >= ARRAY_COUNT(name)) {
        return 0;
    }
    memory_copy(name, net_server_host, length * sizeof(wchar_t));
    name[length++] = L':';
    unsigned port = net_server_port, digits = 0;
    wchar_t reversed[6];
    do {
        reversed[digits++] = (wchar_t)(L'0' + port % 10);
        port /= 10;
    } while (port && digits < 6);
    while (digits) {
        name[length++] = reversed[--digits];
    }
    name[length] = 0;
    return name;
}

int azimuth_net_proxy_enabled(void) {
    return net_mode == NET_MODE_PROXY;
}

/* One synchronous request to the configured server (the FH2Online account endpoints), not
   game traffic: no X-Fh2-Original-Url. `extra_header` is one "Name: value" line or NULL.
   The reply body is copied to `out` (NUL-terminated, truncated to `capacity - 1`).
   Returns the HTTP status, 0 when the server could not be reached. */
DWORD azimuth_net_call(const wchar_t *method, const wchar_t *path, const wchar_t *extra_header,
                       const char *body, char *out, size_t capacity) {
    size_t used = 0;
    DWORD status = azimuth_net_request(method, path, extra_header, L"application/json", body,
                                       body ? text_length(body) : 0, out, capacity ? capacity - 1 : 0, &used);
    if (out && capacity) {
        out[used] = 0;
    }
    return status;
}

/* As azimuth_net_call with a binary body: `headers` is NULL or "Name: value" lines joined by
   \r\n; the reply is copied as is to `out` (up to `capacity` bytes, `*received` set to how
   many). */
static DWORD net_request(const wchar_t *method, const wchar_t *path, const wchar_t *headers,
                         const wchar_t *content_type, const void *body, size_t body_size, void *out,
                         size_t capacity, size_t *received, int timeout_ms);
static DWORD net_request_at(const wchar_t *host, INTERNET_PORT port, int secure, const wchar_t *method,
                            const wchar_t *path, const wchar_t *headers, const wchar_t *content_type,
                            const void *body, size_t body_size, void *out, size_t capacity, size_t *received,
                            int timeout_ms);

DWORD azimuth_net_request(const wchar_t *method, const wchar_t *path, const wchar_t *headers,
                          const wchar_t *content_type, const void *body, size_t body_size, void *out,
                          size_t capacity, size_t *received) {
    return net_request(method, path, headers, content_type, body, body_size, out, capacity, received, 0);
}

/* As azimuth_net_call, giving up after timeout_ms (a menu waiting on the answer). */
DWORD azimuth_net_call_quick(const wchar_t *method, const wchar_t *path, const wchar_t *extra_header,
                             const char *body, char *out, size_t capacity, int timeout_ms) {
    size_t used = 0;
    DWORD status = net_request(method, path, extra_header, L"application/json", body, body ? text_length(body) : 0, out,
                               capacity ? capacity - 1 : 0, &used, timeout_ms);
    if (out && capacity) {
        out[used] = 0;
    }
    return status;
}

/* A GET of https://<host><path> (not the game's server: the self-update reads the release from
   GitHub), redirects followed as WinHTTP does by default. As azimuth_net_request otherwise. */
DWORD azimuth_net_fetch(const wchar_t *host, const wchar_t *path, void *out, size_t capacity, size_t *received) {
    return net_request_at(host, INTERNET_DEFAULT_HTTPS_PORT, 1, L"GET", path, 0, L"application/octet-stream", 0, 0,
                          out, capacity, received, 0);
}

/* [hook] AutoUpdate (default on): whether the hook installs a newer release by itself. */
int azimuth_net_auto_update(void) {
    return net_auto_update;
}

/* timeout_ms 0: 5 s to connect, 30 s to send and receive. */
static DWORD net_request(const wchar_t *method, const wchar_t *path, const wchar_t *headers,
                         const wchar_t *content_type, const void *body, size_t body_size, void *out,
                         size_t capacity, size_t *received, int timeout_ms) {
    if (received) *received = 0;
    if (fi_launch_protocol() && !fi_launch_live()) return 0;
    /* the game's language, for the server's messages (X-Fh2-Lang: pt-BR; English otherwise),
       and the car database's SHA-256 (free roam worlds, azimuth_world.c) */
    wchar_t joined[1600]; /* the session's two header lines are about 1.1k characters */
    static const wchar_t lang[] = L"X-Fh2-Lang: pt-BR";
    azimuth_world_car_db_wait(15000);
    const wchar_t *extras[2] = {azimuth_lang_portuguese() ? lang : 0, azimuth_world_car_db_header()};
    if (extras[0] || extras[1]) {
        size_t n = 0;
        int fits = 1;
        while (headers && headers[n] && n + 1 < ARRAY_COUNT(joined)) {
            joined[n] = headers[n];
            ++n;
        }
        fits = !headers || !headers[n];
        for (int e = 0; e < 2 && fits; ++e) {
            if (!extras[e]) {
                continue;
            }
            size_t length = wide_length(extras[e]);
            if (n + 2 + length + 1 > ARRAY_COUNT(joined)) {
                fits = 0;
                break;
            }
            if (n) {
                joined[n++] = L'\r';
                joined[n++] = L'\n';
            }
            memory_copy(joined + n, extras[e], length * sizeof(wchar_t));
            n += length;
        }
        if (fits) {
            joined[n] = 0;
            headers = joined;
        }
    }
    return net_request_at(net_server_host, net_server_port, net_server_secure, method, path, headers, content_type,
                          body, body_size, out, capacity, received, timeout_ms);
}

static DWORD net_request_at(const wchar_t *host, INTERNET_PORT port, int secure, const wchar_t *method,
                            const wchar_t *path, const wchar_t *headers, const wchar_t *content_type,
                            const void *body, size_t body_size, void *out, size_t capacity, size_t *received,
                            int timeout_ms) {
    if (fi_cfg()->launch == FI_LAUNCH_OFFLINE) { /* OD-118: no request of the hook's own leaves a Play offline */
        static volatile LONG refused;
        net_log_once(&refused, "NETWORK: Play offline: the hook's own request was not sent.\r\n");
        if (received) *received = 0;
        return 0;
    }
    wchar_t type_header[96]; /* several threads call this at once */
    HINTERNET session = 0, connection = 0, http = 0;
    DWORD status = 0, status_size = sizeof(status), size = 0;
    size_t used = 0;
    unsigned char *bytes = out;
    if (received) {
        *received = 0;
    }
    if (fi_launch_protocol() && !fi_launch_live()) return 0;
    if (!net_load_winhttp() || body_size > 0x7fffffffu) {
        return 0;
    }
    session = http_open(L"FH2Online/1", WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_NO_PROXY_NAME,
                        WINHTTP_NO_PROXY_BYPASS, 0);
    if (!session) {
        goto done;
    }
    if (timeout_ms > 0) {
        http_set_timeouts(session, timeout_ms, timeout_ms, timeout_ms, timeout_ms);
    } else {
        http_set_timeouts(session, 5000, 5000, 30000, 30000);
    }
    connection = http_connect(session, host, port, 0);
    http = connection ? http_open_request(connection, method, path, NULL, WINHTTP_NO_REFERER,
                                          WINHTTP_DEFAULT_ACCEPT_TYPES, secure ? WINHTTP_FLAG_SECURE : 0)
                      : 0;
    if (!http) {
        goto done;
    }
    {
        static const wchar_t prefix[] = L"Content-Type: ";
        size_t n = ARRAY_COUNT(prefix) - 1, t = 0;
        memory_copy(type_header, prefix, n * sizeof(wchar_t));
        while (content_type && content_type[t] && n + 1 < ARRAY_COUNT(type_header)) {
            type_header[n++] = content_type[t++];
        }
        type_header[n] = 0;
    }
    http_add_headers(http, type_header, (DWORD)-1L, WINHTTP_ADDREQ_FLAG_ADD);
    if (headers) {
        http_add_headers(http, headers, (DWORD)-1L, WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE);
    }
    size = body ? (DWORD)body_size : 0;
    if (fi_launch_protocol() && !fi_launch_live()) goto done;
    if (!http_send(http, WINHTTP_NO_ADDITIONAL_HEADERS, 0,
                   size ? (void *)body : WINHTTP_NO_REQUEST_DATA, size, size, 0) ||
        !http_receive(http, NULL) ||
        !http_query_headers(http, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                            WINHTTP_HEADER_NAME_BY_INDEX, &status, &status_size,
                            WINHTTP_NO_HEADER_INDEX)) {
        status = 0;
        goto done;
    }
    for (;;) {
        if (fi_launch_protocol() && !fi_launch_live()) { status = 0; goto done; }
        DWORD available = 0, read = 0;
        if (!http_query_available(http, &available) || !available) {
            break;
        }
        char chunk[512];
        if (used < capacity && bytes) {
            /* straight into the caller's buffer */
            DWORD room = capacity - used > 0x10000000u ? 0x10000000u : (DWORD)(capacity - used);
            if (!http_read(http, bytes + used, available < room ? available : room, &read) || !read) {
                break;
            }
            used += read;
            continue;
        }
        /* buffer full: drain the rest */
        if (!http_read(http, chunk, available < sizeof(chunk) ? available : sizeof(chunk), &read) || !read) {
            break;
        }
    }
    if (received) {
        *received = used;
    }
done:
    if (http) {
        http_close(http);
    }
    if (connection) {
        http_close(connection);
    }
    if (session) {
        http_close(session);
    }
    return status;
}

static int net_write_all(SequentialStream *stream, const unsigned char *data, size_t size) {
    while (size) {
        ULONG chunk = size > 0x40000000u ? 0x40000000u : (ULONG)size, written = 0;
        HRESULT result = stream->lpVtbl->Write(stream, data, chunk, &written);
        if (FAILED(result) || !written || written > chunk) {
            return 0;
        }
        data += written;
        size -= written;
    }
    return 1;
}

/* Whether the request goes to the game's Forza services (gameservices.FH2.forzamotorsport.net
   and the like). */
static int net_is_forza_service(const wchar_t *url) {
    static const wchar_t host[] = L"forzamotorsport.net";
    for (const wchar_t *s = url; s[0] && s[1] && s[2]; ++s) {
        if (s[0] == L':' && s[1] == L'/' && s[2] == L'/') {
            url = s + 3;
            break;
        }
    }
    for (const wchar_t *p = url; *p && *p != L'/'; ++p) {
        size_t i = 0;
        while (host[i] && (p[i] | 32) == host[i]) {
            ++i;
        }
        if (!host[i]) {
            return 1;
        }
    }
    return 0;
}

/* No FH2Online profile on this PC (or Online=0): the game's Forza services are answered here
   as the server would answer them (503, the game plays offline), and nothing goes to the
   server. Only the hook's own routes (create a profile, sign in, the server's info) reach it. */
static int net_answer_without_profile(NetRequest *request) {
    static volatile LONG logged;
    static const char body[] = "no FH2Online account";
    if (!net_is_forza_service(buffer_wide(&request->url)) || azimuth_account_token_wait()) {
        return 0;
    }
    request->status = 503;
    buffer_free(&request->status_text);
    buffer_free(&request->response_body);
    if (!buffer_append_wide(&request->status_text, L"Service Unavailable") ||
        !buffer_append(&request->response_body, body, sizeof(body) - 1)) {
        return 0;
    }
    if (!InterlockedExchange(&logged, 1)) {
        log_append("NETWORK: no FH2Online profile: the game's online services stay off here (nothing is sent to "
                   "the server).\r\n");
    }
    return 1;
}

/* A free account (tier.c): the universe's services are answered here, never sent (a free
   profile has no records, rivals, ghosts, clubs, designs, friends or cloud copy on the
   server). The first answer of each kind is logged. */
static int net_answer_free(NetRequest *request) {
    static unsigned logged[48];
    static volatile LONG logged_count;
    static SRWLOCK logged_lock = SRWLOCK_INIT;
    static const unsigned char empty_reply[12] = {0};
    if (fi_tier_universe()) {
        return 0;
    }
    const char *json = 0;
    wchar_t what[96];
    int route = fi_tier_free_route(buffer_wide(&request->method), buffer_wide(&request->url),
                                   buffer_wide(&request->request_headers), &json, what, ARRAY_COUNT(what));
    if (route == FI_ROUTE_PASS) {
        return 0;
    }
    request->status = route == FI_ROUTE_UNAVAILABLE ? 503 : 200;
    buffer_free(&request->status_text);
    buffer_free(&request->response_headers);
    buffer_free(&request->response_body);
    int ok = route == FI_ROUTE_UNAVAILABLE
                 ? buffer_append_wide(&request->status_text, L"Service Unavailable") &&
                       buffer_append_text(&request->response_body, "free profile")
             : route == FI_ROUTE_EMPTY_XTSW
                 ? buffer_append_wide(&request->status_text, L"OK") &&
                       buffer_append_wide(&request->response_headers, L"Content-Type: bin/xtsw\r\n") &&
                       buffer_append(&request->response_body, empty_reply, sizeof(empty_reply))
                 : buffer_append_wide(&request->status_text, L"OK") &&
                       buffer_append_wide(&request->response_headers, L"Content-Type: application/json\r\n") &&
                       buffer_append_text(&request->response_body, json);
    unsigned hash = 2166136261u;
    for (const wchar_t *p = what; *p; ++p) {
        hash = (hash ^ (unsigned)*p) * 16777619u;
    }
    int first = 1;
    AcquireSRWLockExclusive(&logged_lock);
    for (LONG i = 0; i < logged_count; ++i) {
        first &= logged[i] != hash;
    }
    if (first && logged_count < (LONG)ARRAY_COUNT(logged)) {
        logged[logged_count++] = hash;
    } else {
        first = 0;
    }
    ReleaseSRWLockExclusive(&logged_lock);
    if (first) {
        char line[200];
        size_t n = 0;
        static const char head[] = "NETWORK: free or Noob profile: ";
        for (size_t i = 0; head[i]; ++i) line[n++] = head[i];
        for (size_t i = 0; what[i] && n + 80 < sizeof(line); ++i) line[n++] = what[i] < 128 ? (char)what[i] : '?';
        const char *tail = route == FI_ROUTE_UNAVAILABLE ? " answered here, unavailable (not sent: free profiles have "
                                                           "none of it).\r\n"
                           : route == FI_ROUTE_EMPTY_XTSW ? " answered here as taken (not sent: no cloud copy).\r\n"
                                                          : " answered here, empty (not sent: no friends on free).\r\n";
        for (size_t i = 0; tail[i] && n + 1 < sizeof(line); ++i) line[n++] = tail[i];
        line[n] = 0;
        log_append(line);
    }
    return ok;
}

/* Foreign XUIDs in a launcher session (tier.c fi_tier_foreign_route): only the chosen profile
   talks to the server. The first answer of each kind is logged, without the XUIDs. */
static int net_answer_foreign(NetRequest *request) {
    static volatile LONG logged_json, logged_unavailable;
    unsigned long long own = azimuth_account_xuid_value();
    if (!fi_launch_protocol() || !own) {
        return 0;
    }
    enum { JSON_CAP = 16384 };
    char *json = net_alloc(JSON_CAP);
    if (!json) {
        return 0;
    }
    int route = fi_tier_foreign_route(buffer_wide(&request->method), buffer_wide(&request->url),
                                      request->request_body.data, request->request_body.size, own,
                                      azimuth_account_gamertag(), !fi_tier_universe(), json, JSON_CAP);
    int ok = 0;
    if (route != FI_ROUTE_PASS) {
        request->status = route == FI_ROUTE_JSON ? 200 : 503;
        buffer_free(&request->status_text);
        buffer_free(&request->response_headers);
        buffer_free(&request->response_body);
        ok = route == FI_ROUTE_JSON
                 ? buffer_append_wide(&request->status_text, L"OK") &&
                       buffer_append_wide(&request->response_headers, L"Content-Type: application/json\r\n") &&
                       buffer_append_text(&request->response_body, json)
                 : buffer_append_wide(&request->status_text, L"Service Unavailable") &&
                       buffer_append_text(&request->response_body, "another player");
        net_log_once(route == FI_ROUTE_JSON ? &logged_json : &logged_unavailable,
                     route == FI_ROUTE_JSON ? "NETWORK: a lookup of other players (drivatars) answered here, with "
                                              "generated names (not sent: only this profile talks to the server).\r\n"
                                            : "NETWORK: a request about another player answered here, unavailable "
                                              "(not sent: only this profile talks to the server).\r\n");
    }
    net_free(json);
    return ok;
}

/* World mates' paints in a launcher session (livery_world.c, OD-66): the game's own
   content-versioning calls for them are answered here from the world's relay. */
static int net_answer_livery(NetRequest *request) {
    if (!fi_launch_protocol()) {
        return 0;
    }
    unsigned char *reply = 0;
    size_t size = 0;
    if (!fi_livery_route(buffer_wide(&request->request_headers), request->request_body.data,
                         request->request_body.size, &reply, &size)) {
        return 0;
    }
    request->status = 200;
    buffer_free(&request->status_text);
    buffer_free(&request->response_headers);
    buffer_free(&request->response_body);
    int ok = buffer_append_wide(&request->status_text, L"OK") &&
             buffer_append_wide(&request->response_headers, L"Content-Type: bin/xtsw\r\n") &&
             buffer_append(&request->response_body, reply, size);
    fi_livery_free(reply);
    return ok;
}

/* A profile sync beyond what play earns (azimuth_progress.c): not sent, and answered as the
   server answers UpdateAll (200, bin/xtsw: status 0, code 0, no message, no results). */
static int net_hold_progress(NetRequest *request) {
    static const unsigned char empty_reply[12] = {0};
    if (!azimuth_progress_hold(buffer_wide(&request->request_headers), request->request_body.data,
                               request->request_body.size)) {
        return 0;
    }
    request->status = 200;
    buffer_free(&request->status_text);
    buffer_free(&request->response_headers);
    buffer_free(&request->response_body);
    return buffer_append_wide(&request->status_text, L"OK") &&
           buffer_append_wide(&request->response_headers, L"Content-Type: bin/xtsw\r\n") &&
           buffer_append(&request->response_body, empty_reply, sizeof(empty_reply));
}

static DWORD WINAPI net_worker(void *parameter) {
    NetRequest *request = parameter;
    HRESULT error = NET_OFFLINE_ERROR;
    if (net_mode == NET_MODE_PROXY && !request->aborted) {
        /* a private free roam session opens this PC's world before the server sees it */
        azimuth_world_on_request(buffer_wide(&request->method), buffer_wide(&request->url),
                                 request->request_body.data, request->request_body.size);
    }
    int answered = net_mode == NET_MODE_PROXY && !request->aborted &&
                   (net_answer_without_profile(request) || net_answer_foreign(request) || net_answer_livery(request) ||
                    net_answer_free(request) || net_hold_progress(request) ||
                    net_forward(request, &error));
    if (request->aborted) {
        answered = 0;
        error = E_ABORT;
    }
    net_capture(request, answered, error);
    /* Take the callback out of the request: msxml6 drops it after the final
       notification, and XSAPI's callback owns the request (cycle otherwise). */
    XhrCallback *callback = InterlockedExchangePointer((void *volatile *)&request->callback, NULL);
    if (callback) {
        if (!answered) {
            callback->lpVtbl->OnError(callback, request, error);
        } else {
            callback->lpVtbl->OnHeadersAvailable(callback, request, request->status,
                                                 buffer_wide(&request->status_text));
            if (request->custom_stream) {
                SequentialStream *custom = request->custom_stream;
                if (!net_write_all(custom, request->response_body.data,
                                   request->response_body.size)) {
                    callback->lpVtbl->OnError(callback, request, E_FAIL);
                } else {
                    callback->lpVtbl->OnDataAvailable(callback, request, custom);
                    callback->lpVtbl->OnResponseReceived(callback, request, custom);
                }
            } else {
                SequentialStream *stream = memory_stream_create(&request->response_body);
                if (!stream) {
                    callback->lpVtbl->OnError(callback, request, E_OUTOFMEMORY);
                } else {
                    callback->lpVtbl->OnDataAvailable(callback, request, stream);
                    callback->lpVtbl->OnResponseReceived(callback, request, stream);
                    stream->lpVtbl->Release(stream);
                }
            }
        }
        callback->lpVtbl->Release(callback);
    }
    net_request_release(request);
    return 0;
}

static HRESULT STDMETHODCALLTYPE net_request_send(NetRequest *self, SequentialStream *body,
                                                  ULONGLONG size) {
    if (!self->opened || !self->callback) {
        return NET_UNEXPECTED;
    }
    if (InterlockedCompareExchange(&self->sent, 1, 0)) {
        return NET_UNEXPECTED;
    }
    /* a game thread, after the player's ExtraOptions (if any) has run */
    fi_game_callback_from("the game's first network request");
    if (!net_read_body(self, body, size)) {
        return E_FAIL;
    }
    InterlockedIncrement(&self->references);
    HANDLE thread = CreateThread(NULL, 0, net_worker, self, 0, NULL);
    if (!thread) {
        InterlockedDecrement(&self->references);
        return HRESULT_FROM_WIN32(GetLastError());
    }
    CloseHandle(thread);
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_abort(NetRequest *self) {
    InterlockedExchange(&self->aborted, 1);
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_set_cookie(NetRequest *self, const void *cookie,
                                                        DWORD *state) {
    (void)self;
    (void)cookie;
    if (state) {
        *state = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_set_stream(NetRequest *self,
                                                        SequentialStream *stream) {
    if (self->custom_stream) {
        self->custom_stream->lpVtbl->Release(self->custom_stream);
    }
    self->custom_stream = stream;
    if (stream) {
        stream->lpVtbl->AddRef(stream);
        log_append("NETWORK: a request uses a custom response stream.\r\n");
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_set_property(NetRequest *self, int property,
                                                          ULONGLONG value) {
    if (property == NET_XHR_PROP_TIMEOUT) {
        self->timeout_ms = value > 0x7fffffffu ? 0x7fffffffu : (DWORD)value;
    }
    return S_OK;
}

static HRESULT net_copy_for_caller(const wchar_t *text, size_t length, WCHAR **out) {
    WCHAR *copy = net_task_alloc((length + 1) * sizeof(WCHAR));
    if (!copy) {
        return E_OUTOFMEMORY;
    }
    if (length) {
        memory_copy(copy, text, length * sizeof(WCHAR));
    }
    copy[length] = 0;
    *out = copy;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_all_headers(NetRequest *self, WCHAR **headers) {
    if (!headers) {
        return E_POINTER;
    }
    *headers = 0;
    return net_copy_for_caller(buffer_wide(&self->response_headers),
                               self->response_headers.size / sizeof(wchar_t), headers);
}

static HRESULT STDMETHODCALLTYPE net_request_get_cookie(NetRequest *self, const WCHAR *url,
                                                        const WCHAR *name, DWORD flags,
                                                        DWORD *count, void **cookies) {
    (void)self;
    (void)url;
    (void)name;
    (void)flags;
    if (count) {
        *count = 0;
    }
    if (cookies) {
        *cookies = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_request_header(NetRequest *self, const WCHAR *name,
                                                    WCHAR **value) {
    if (!name || !value) {
        return E_POINTER;
    }
    *value = 0;
    size_t name_length = wide_length(name);
    for (const wchar_t *line = buffer_wide(&self->response_headers); *line;) {
        size_t length = 0;
        while (line[length] && line[length] != L'\n') {
            ++length;
        }
        if (length > name_length && line[name_length] == L':' &&
            wide_starts_ignore_case(line, length, name)) {
            size_t start = name_length + 1, end = length;
            while (start < end && line[start] == L' ') {
                ++start;
            }
            while (end > start && (line[end - 1] == L'\r' || line[end - 1] == L' ')) {
                --end;
            }
            return net_copy_for_caller(line + start, end - start, value);
        }
        line += line[length] ? length + 1 : length;
    }
    return NET_HEADER_NOT_FOUND;
}

static const NetRequestVtbl net_request_vtable = {
    net_request_query,        net_request_add_ref,     net_request_release,
    net_request_open,         net_request_send,        net_request_abort,
    net_request_set_cookie,   net_request_set_stream,  net_request_set_property,
    net_request_set_header,   net_request_all_headers, net_request_get_cookie,
    net_request_header};

/* ------------------------------------------------- CoCreateInstance detour */

typedef HRESULT (*MmDeviceFactoryFn)(REFIID, void **);

static HRESULT net_co_create_instance(REFCLSID clsid, void *outer, DWORD context, REFIID iid,
                                      void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(clsid, net_clsid_xhr, 16)) {
        if (outer) {
            return CLASS_E_NOAGGREGATION;
        }
        net_log_once(&net_seen_xhr, "NETWORK: game created its first IXMLHTTPRequest2.\r\n");
        NetRequest *request = net_alloc(sizeof(*request));
        if (!request) {
            return E_OUTOFMEMORY;
        }
        request->lpVtbl = &net_request_vtable;
        request->references = 1;
        request->id = InterlockedIncrement(&net_next_id);
        HRESULT result = net_request_query(request, iid, out);
        net_request_release(request);
        return result;
    }
    if (memory_equal(clsid, net_clsid_mmdevice, 16)) {
        if (outer) {
            return CLASS_E_NOAGGREGATION;
        }
        if (!(context & NET_CLSCTX_INPROC_SERVER)) {
            return E_INVALIDARG;
        }
        net_log_once(&net_seen_mmdevice,
                     "NETWORK: CoCreateInstance reached the hook (MMDeviceEnumerator -> Azimuth).\r\n");
        MmDeviceFactoryFn factory =
            (MmDeviceFactoryFn)(uintptr_t)(net_azimuth_base + net_offsets->net_mmdevice_factory);
        return factory(iid, out);
    }
    net_log_guid("NETWORK: refused CoCreateInstance for unknown CLSID ", (const unsigned char *)clsid);
    return REGDB_E_CLASSNOTREG;
}

/* 1 = installed, 2 = already installed, 0 = unsupported host, -1 = write failed. */
static int net_install_hook(unsigned char *base) {
    unsigned char current[NET_HOOK_HASHED_BYTES], digest[32];
    unsigned char jump[NET_HOOK_JUMP_BYTES] = {0xff, 0x25, 0x00, 0x00, 0x00, 0x00};
    uintptr_t target = (uintptr_t)net_co_create_instance;
    for (unsigned i = 0; i < 8; ++i) {
        jump[6 + i] = (unsigned char)(target >> (8 * i));
    }
    net_offsets = azimuth_offsets(base);
    if (!net_offsets || !memory_read(base + net_offsets->net_hook, current, sizeof(current))) {
        return 0;
    }
    if (memory_equal(current, jump, sizeof(jump))) {
        return 2;
    }
    Sha256Context hash;
    sha256_initialize(&hash);
    sha256_update(&hash, current, sizeof(current));
    sha256_finalize(&hash, digest);
    if (!memory_equal(digest, net_offsets->net_hook_hash, sizeof(digest))) {
        return 0;
    }
    unsigned char *at = base + net_offsets->net_hook;
    DWORD old_protection, ignored;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        return -1;
    }
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    VirtualProtect(at, sizeof(jump), old_protection, &ignored);
    return 1;
}

/* ----------------------------------------------------------------- setup */

/* Infinity.cfg [hook] NetMode, Server and AutoUpdate (cfg.c, read at start). */
static void net_read_settings(const wchar_t *directory) {
    const FiConfig *config = fi_cfg();
    net_mode = config->net_mode == FI_NET_PROXY     ? NET_MODE_PROXY
               : config->net_mode == FI_NET_CAPTURE ? NET_MODE_CAPTURE
                                                    : NET_MODE_OFF;
    /* OD-118: cfg_launch already turns the network off for Play offline, whatever [hook] NetMode says; kept here too,
       so no later setting can turn it back on (the game's requests get Azimuth's own offline answer). */
    if (config->launch == FI_LAUNCH_OFFLINE) {
        net_mode = NET_MODE_OFF;
    }
    net_auto_update = config->auto_update != 0 && config->launch != FI_LAUNCH_OFFLINE;
    const wchar_t *server = config->server;
    net_server_secure = 0;
    if (wide_starts_ignore_case(server, wide_length(server), L"http://")) {
        server += 7;
    } else if (wide_starts_ignore_case(server, wide_length(server), L"https://")) {
        server += 8;
        net_server_secure = 1;
        net_server_port = INTERNET_DEFAULT_HTTPS_PORT;
    }
    size_t length = 0, colon = (size_t)-1;
    while (server[length] && server[length] != L'/') {
        if (server[length] == L':') {
            colon = length;
        }
        ++length;
    }
    size_t host_length = colon == (size_t)-1 ? length : colon;
    if (!host_length || host_length >= ARRAY_COUNT(net_server_host)) {
        memory_copy(net_server_host, L"127.0.0.1", sizeof(L"127.0.0.1"));
    } else {
        memory_copy(net_server_host, server, host_length * sizeof(wchar_t));
        net_server_host[host_length] = 0;
    }
    if (colon != (size_t)-1) {
        unsigned port = 0;
        for (size_t i = colon + 1; i < length && server[i] >= L'0' && server[i] <= L'9'; ++i) {
            port = port * 10 + (unsigned)(server[i] - L'0');
        }
        if (port && port < 65536) {
            net_server_port = (INTERNET_PORT)port;
        }
    }
    if (fi_launch_protocol()) {
        /* a launcher start: the server is the one the ticket names (launch.c), never [hook] Server */
        net_server_host[0] = 0;
        net_server_secure = 0;
    }
    if (!path_join(net_log_path, directory, NET_LOG_NAME)) {
        net_log_path[0] = 0;
    }
}

/* The launcher ticket's server (launch.c, before the session goes live): https://host[:port],
   or in a development build http://127.0.0.1:port. A path after the host is not used (game
   requests go to /<original host and path>, as with [hook] Server). */
int azimuth_net_set_launch_server(const char *url) {
    size_t n = text_length(url), skip;
    int secure;
    if (n > 8 && memory_equal(url, "https://", 8)) {
        skip = 8;
        secure = 1;
    } else if (n > 7 && memory_equal(url, "http://", 7)) {
        skip = 7;
        secure = 0;
    } else {
        return 0;
    }
    size_t length = 0, colon = (size_t)-1;
    const char *host = url + skip;
    while (host[length] && host[length] != '/') {
        if (host[length] == ':') {
            colon = length;
        }
        ++length;
    }
    size_t host_length = colon == (size_t)-1 ? length : colon;
    unsigned port = secure ? INTERNET_DEFAULT_HTTPS_PORT : 80;
    if (colon != (size_t)-1) {
        port = 0;
        for (size_t i = colon + 1; i < length; ++i) {
            if (host[i] < '0' || host[i] > '9' || port > 65535) {
                return 0;
            }
            port = port * 10 + (unsigned)(host[i] - '0');
        }
    }
    if (!host_length || host_length >= ARRAY_COUNT(net_server_host) || !port || port > 65535) {
        return 0;
    }
    for (size_t i = 0; i < host_length; ++i) {
        char c = host[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '.')) {
            return 0;
        }
        net_server_host[i] = (wchar_t)c;
    }
    net_server_host[host_length] = 0;
    net_server_port = (INTERNET_PORT)port;
    net_server_secure = secure;
    return 1;
}

/* ------------------------------------------------------------ WinRT trace */
/* Diagnostics for the offline gates: which WinRT classes the game activates
   (Azimuth's vccorlib110!GetActivationFactoryByPCWSTR) and whether it asks
   User for an Xbox Live token. Written to infinity.log. */

#define NET_TRACE_MAX_LINES 400
/* The activation (0.9.7: FUN_14006da50) asks its providers (net_offsets) in order;
   REGDB_E_CLASSNOTREG means "next". */
typedef HRESULT (*NetActivationProvider)(const wchar_t *, const void *, void **);

/* User.GetTokenAndSignature*: six slots of the two User vtables (net_offsets). */
typedef HRESULT (*NetTokenFn)(void *, void *, void *, void *, void *, void *, void *, void *);
static NetTokenFn net_token_original[6];

static volatile LONG net_trace_lines;
static SRWLOCK net_trace_lock = SRWLOCK_INIT;
static unsigned net_trace_seen[512];
static unsigned net_trace_seen_count;

static int net_trace_first(const wchar_t *name, HRESULT result) {
    unsigned hash = 2166136261u ^ (unsigned)result;
    for (const wchar_t *p = name; p && *p; ++p) {
        hash = (hash ^ (unsigned)*p) * 16777619u;
    }
    int first = 1;
    AcquireSRWLockExclusive(&net_trace_lock);
    for (unsigned i = 0; i < net_trace_seen_count; ++i) {
        if (net_trace_seen[i] == hash) {
            first = 0;
            break;
        }
    }
    if (first && net_trace_seen_count < ARRAY_COUNT(net_trace_seen)) {
        net_trace_seen[net_trace_seen_count++] = hash;
    }
    ReleaseSRWLockExclusive(&net_trace_lock);
    return first;
}

static void net_trace_line(Buffer *line) {
    if (InterlockedIncrement(&net_trace_lines) <= NET_TRACE_MAX_LINES &&
        buffer_append_text(line, "\r\n")) {
        log_append((const char *)line->data);
    }
    buffer_free(line);
}

static HRESULT net_activate_raw(const wchar_t *name, const void *iid, void **out,
                                unsigned *provider_out) {
    HRESULT result = REGDB_E_CLASSNOTREG;
    unsigned provider = 0;
    *out = 0;
    for (; provider < ARRAY_COUNT(net_offsets->net_activation_providers); ++provider) {
        NetActivationProvider call = (NetActivationProvider)(uintptr_t)(
            net_azimuth_base + net_offsets->net_activation_providers[provider]);
        result = call(name, iid, out);
        if (result != REGDB_E_CLASSNOTREG) {
            break;
        }
    }
    if (provider_out) {
        *provider_out = provider;
    }
    return result;
}

/* ------------------------------------------------- DataWriter replacement */
/* XSAPI turns every response body into an IBuffer with a default-constructed
   Windows.Storage.Streams.DataWriter (WriteBytes, then DetachBuffer), and
   treats an empty buffer as "no result": a 2xx reply then completes with a
   null result and the game dereferences it (achievements, game+0x41fd28).
   Azimuth 0.9.7 has no IDataWriter of its own (the class comes from a generic
   table). The first time XSAPI asks for the factory, we write four bytes with
   Azimuth's writer and detach them; if they do not come back, every later
   writer is this one, which detaches into Azimuth's own Buffer class. */

static const unsigned char net_iid_inspectable[16] = {0xe0, 0xe2, 0x86, 0xaf, 0x2d, 0xb1,
                                                      0x6a, 0x4c, 0x9c, 0x5a, 0xd7, 0xaa,
                                                      0x65, 0x10, 0x1e, 0x90};
static const unsigned char net_iid_activation_factory[16] = {0x35, 0x00, 0x00, 0x00, 0x00, 0x00,
                                                             0x00, 0x00, 0xc0, 0x00, 0x00, 0x00,
                                                             0x00, 0x00, 0x00, 0x46};
static const unsigned char net_iid_data_writer[16] = {0x65, 0x92, 0xb8, 0x64, 0x41, 0xd3,
                                                      0x22, 0x49, 0xb3, 0x8a, 0xdd, 0x4a,
                                                      0xf8, 0x80, 0x8c, 0x4e};
static const unsigned char net_iid_closable[16] = {0x29, 0xa8, 0xd5, 0x30, 0xa4, 0x7f, 0x26, 0x40,
                                                   0x83, 0xbb, 0xd7, 0x5b, 0xae, 0x4e, 0xa9, 0x9e};
static const unsigned char net_iid_buffer_byte_access[16] = {0xef, 0x0f, 0x5a, 0x90, 0x53, 0xbc,
                                                             0xdf, 0x11, 0x8c, 0x49, 0x00, 0x1e,
                                                             0x4f, 0xc6, 0x86, 0xda};
static const unsigned char net_iid_buffer_factory[16] = {0x4d, 0x91, 0xaf, 0x71, 0x0f, 0xc1,
                                                         0x4b, 0x48, 0xbc, 0x50, 0x14, 0xbc,
                                                         0x62, 0x3b, 0x3a, 0x27};
#define NET_E_NOTIMPL ((HRESULT)0x80004001L)
#define NET_E_BOUNDS ((HRESULT)0x8000000BL)
#define NET_RO_E_CLOSED ((HRESULT)0x80000013L)

/* Just enough of the interfaces we call on Azimuth's objects. */
typedef struct NetCom NetCom;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetCom *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetCom *);
    ULONG(STDMETHODCALLTYPE *Release)(NetCom *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetCom *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetCom *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetCom *, int *);
    /* IActivationFactory::ActivateInstance, IBufferFactory::Create,
       IBuffer::get_Capacity or IDataWriter::get_UnstoredBufferLength */
    HRESULT(STDMETHODCALLTYPE *Slot6)(NetCom *, void *);
    void *slots7to11[5];
    HRESULT(STDMETHODCALLTYPE *WriteBytes)(NetCom *, UINT32, BYTE *); /* IDataWriter */
    void *slots13to30[18];
    HRESULT(STDMETHODCALLTYPE *DetachBuffer)(NetCom *, NetCom **); /* IDataWriter */
} NetComVtbl;
struct NetCom {
    const NetComVtbl *lpVtbl;
};
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetCom *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetCom *);
    ULONG(STDMETHODCALLTYPE *Release)(NetCom *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetCom *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetCom *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetCom *, int *);
    HRESULT(STDMETHODCALLTYPE *get_Capacity)(NetCom *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *get_Length)(NetCom *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *put_Length)(NetCom *, UINT32);
} NetIBufferVtbl;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetCom *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetCom *);
    ULONG(STDMETHODCALLTYPE *Release)(NetCom *);
    HRESULT(STDMETHODCALLTYPE *Buffer)(NetCom *, BYTE **);
} NetByteAccessVtbl;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetCom *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetCom *);
    ULONG(STDMETHODCALLTYPE *Release)(NetCom *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetCom *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetCom *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetCom *, int *);
    HRESULT(STDMETHODCALLTYPE *Create)(NetCom *, UINT32, NetCom **);
} NetBufferFactoryVtbl;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetCom *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetCom *);
    ULONG(STDMETHODCALLTYPE *Release)(NetCom *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetCom *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetCom *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetCom *, int *);
    HRESULT(STDMETHODCALLTYPE *ActivateInstance)(NetCom *, NetCom **);
} NetActivationFactoryVtbl;

#define NET_IBUFFER(object) ((const NetIBufferVtbl *)(const void *)(object)->lpVtbl)

static SRWLOCK net_writer_lock = SRWLOCK_INIT;
static int net_writer_state; /* 0 untested, 1 Azimuth's works, 2 replaced */
static NetCom *net_buffer_factory;
static volatile LONG net_writer_logged;

static void net_writer_log_once(volatile LONG *flag, const char *message) {
    if (!InterlockedCompareExchange(flag, 1, 0)) {
        log_append(message);
    }
}

/* Reads an Azimuth IBuffer: pointer and length (IBufferByteAccess). */
static int net_buffer_bytes(NetCom *buffer, BYTE **data, UINT32 *length) {
    NetCom *access = 0;
    *data = 0;
    *length = 0;
    if (!buffer || FAILED(NET_IBUFFER(buffer)->get_Length(buffer, length))) {
        return 0;
    }
    if (FAILED(buffer->lpVtbl->QueryInterface(buffer, (REFIID)net_iid_buffer_byte_access,
                                              (void **)&access)) ||
        !access) {
        return 0;
    }
    HRESULT result = ((const NetByteAccessVtbl *)(const void *)access->lpVtbl)->Buffer(access, data);
    access->lpVtbl->Release(access);
    return SUCCEEDED(result) && (*data || !*length);
}

/* A new Azimuth Buffer holding a copy of the bytes. */
static HRESULT net_buffer_create(const unsigned char *data, size_t size, NetCom **out) {
    *out = 0;
    if (size > 0xffffffffu) {
        return E_OUTOFMEMORY;
    }
    AcquireSRWLockExclusive(&net_writer_lock);
    if (!net_buffer_factory) {
        void *factory = 0;
        if (SUCCEEDED(net_activate_raw(L"Windows.Storage.Streams.Buffer", net_iid_buffer_factory,
                                       &factory, 0))) {
            net_buffer_factory = factory;
        }
    }
    NetCom *factory = net_buffer_factory;
    ReleaseSRWLockExclusive(&net_writer_lock);
    if (!factory) {
        return REGDB_E_CLASSNOTREG;
    }
    NetCom *buffer = 0;
    HRESULT result = ((const NetBufferFactoryVtbl *)(const void *)factory->lpVtbl)
                         ->Create(factory, size ? (UINT32)size : 1u, &buffer);
    if (FAILED(result) || !buffer) {
        return FAILED(result) ? result : E_OUTOFMEMORY;
    }
    NetCom *access = 0;
    BYTE *target = 0;
    if (size) {
        result = buffer->lpVtbl->QueryInterface(buffer, (REFIID)net_iid_buffer_byte_access,
                                                (void **)&access);
        if (SUCCEEDED(result) && access) {
            result = ((const NetByteAccessVtbl *)(const void *)access->lpVtbl)->Buffer(access, &target);
            access->lpVtbl->Release(access);
        }
        if (SUCCEEDED(result) && target) {
            memory_copy(target, data, size);
        } else if (SUCCEEDED(result)) {
            result = E_POINTER;
        }
    }
    if (SUCCEEDED(result)) {
        result = NET_IBUFFER(buffer)->put_Length(buffer, (UINT32)size);
    }
    if (FAILED(result)) {
        buffer->lpVtbl->Release(buffer);
        return result;
    }
    *out = buffer;
    return S_OK;
}

typedef struct NetWriter NetWriter;
typedef struct NetWriterClosable NetWriterClosable;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetWriter *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetWriter *);
    ULONG(STDMETHODCALLTYPE *Release)(NetWriter *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetWriter *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetWriter *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetWriter *, int *);
    HRESULT(STDMETHODCALLTYPE *get_UnstoredBufferLength)(NetWriter *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *get_UnicodeEncoding)(NetWriter *, int *);
    HRESULT(STDMETHODCALLTYPE *put_UnicodeEncoding)(NetWriter *, int);
    HRESULT(STDMETHODCALLTYPE *get_ByteOrder)(NetWriter *, int *);
    HRESULT(STDMETHODCALLTYPE *put_ByteOrder)(NetWriter *, int);
    HRESULT(STDMETHODCALLTYPE *WriteByte)(NetWriter *, BYTE);
    HRESULT(STDMETHODCALLTYPE *WriteBytes)(NetWriter *, UINT32, BYTE *);
    HRESULT(STDMETHODCALLTYPE *WriteBuffer)(NetWriter *, NetCom *);
    HRESULT(STDMETHODCALLTYPE *WriteBufferRange)(NetWriter *, NetCom *, UINT32, UINT32);
    HRESULT(STDMETHODCALLTYPE *WriteBoolean)(NetWriter *, BYTE);
    HRESULT(STDMETHODCALLTYPE *WriteGuid)(NetWriter *, GUID);
    HRESULT(STDMETHODCALLTYPE *WriteInt16)(NetWriter *, INT16);
    HRESULT(STDMETHODCALLTYPE *WriteInt32)(NetWriter *, INT32);
    HRESULT(STDMETHODCALLTYPE *WriteInt64)(NetWriter *, INT64);
    HRESULT(STDMETHODCALLTYPE *WriteUInt16)(NetWriter *, UINT16);
    HRESULT(STDMETHODCALLTYPE *WriteUInt32)(NetWriter *, UINT32);
    HRESULT(STDMETHODCALLTYPE *WriteUInt64)(NetWriter *, UINT64);
    HRESULT(STDMETHODCALLTYPE *WriteSingle)(NetWriter *, float);
    HRESULT(STDMETHODCALLTYPE *WriteDouble)(NetWriter *, double);
    HRESULT(STDMETHODCALLTYPE *WriteDateTime)(NetWriter *, INT64);
    HRESULT(STDMETHODCALLTYPE *WriteTimeSpan)(NetWriter *, INT64);
    HRESULT(STDMETHODCALLTYPE *WriteString)(NetWriter *, void *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *MeasureString)(NetWriter *, void *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *StoreAsync)(NetWriter *, void **);
    HRESULT(STDMETHODCALLTYPE *FlushAsync)(NetWriter *, void **);
    HRESULT(STDMETHODCALLTYPE *DetachBuffer)(NetWriter *, NetCom **);
    HRESULT(STDMETHODCALLTYPE *DetachStream)(NetWriter *, void **);
} NetWriterVtbl;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetWriterClosable *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetWriterClosable *);
    ULONG(STDMETHODCALLTYPE *Release)(NetWriterClosable *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetWriterClosable *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetWriterClosable *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetWriterClosable *, int *);
    HRESULT(STDMETHODCALLTYPE *Close)(NetWriterClosable *);
} NetWriterClosableVtbl;
struct NetWriterClosable {
    const NetWriterClosableVtbl *lpVtbl;
};
struct NetWriter {
    const NetWriterVtbl *lpVtbl;
    NetWriterClosable closable;
    volatile LONG references;
    int closed, byte_order, encoding; /* ByteOrder: 0 little, 1 big (default) */
    Buffer data;
};

static NetWriter *net_writer_from_closable(NetWriterClosable *closable) {
    return (NetWriter *)(void *)((unsigned char *)closable - offsetof(NetWriter, closable));
}

static HRESULT STDMETHODCALLTYPE net_writer_query(NetWriter *self, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_inspectable, 16) ||
        memory_equal(iid, net_iid_data_writer, 16) || memory_equal(iid, net_iid_agile, 16)) {
        *out = self;
    } else if (memory_equal(iid, net_iid_closable, 16)) {
        *out = &self->closable;
    } else {
        return E_NOINTERFACE;
    }
    InterlockedIncrement(&self->references);
    return S_OK;
}

static ULONG STDMETHODCALLTYPE net_writer_add_ref(NetWriter *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}

static ULONG STDMETHODCALLTYPE net_writer_release(NetWriter *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        buffer_free(&self->data);
        net_free(self);
    }
    return (ULONG)left;
}

static HRESULT STDMETHODCALLTYPE net_writer_iids(NetWriter *self, ULONG *count, void **iids) {
    (void)self;
    if (count) {
        *count = 0;
    }
    if (iids) {
        *iids = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_class_name(NetWriter *self, void **name) {
    (void)self;
    if (name) {
        *name = 0;
    }
    return NET_E_NOTIMPL;
}

static HRESULT STDMETHODCALLTYPE net_writer_trust(NetWriter *self, int *level) {
    (void)self;
    if (level) {
        *level = 0; /* BaseTrust */
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_unstored(NetWriter *self, UINT32 *length) {
    if (!length) {
        return E_POINTER;
    }
    *length = (UINT32)self->data.size;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_get_encoding(NetWriter *self, int *value) {
    if (!value) {
        return E_POINTER;
    }
    *value = self->encoding;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_put_encoding(NetWriter *self, int value) {
    self->encoding = value;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_get_order(NetWriter *self, int *value) {
    if (!value) {
        return E_POINTER;
    }
    *value = self->byte_order;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_put_order(NetWriter *self, int value) {
    self->byte_order = value;
    return S_OK;
}

static HRESULT net_writer_append(NetWriter *self, const void *data, size_t size) {
    if (self->closed) {
        return NET_RO_E_CLOSED;
    }
    if (size && !data) {
        return E_POINTER;
    }
    return buffer_append(&self->data, data, size) ? S_OK : E_OUTOFMEMORY;
}

/* Integers in the writer's byte order. */
static HRESULT net_writer_number(NetWriter *self, unsigned long long value, unsigned size) {
    unsigned char bytes[8];
    for (unsigned i = 0; i < size; ++i) {
        unsigned shift = 8 * (self->byte_order ? size - 1 - i : i);
        bytes[i] = (unsigned char)(value >> shift);
    }
    return net_writer_append(self, bytes, size);
}

static HRESULT STDMETHODCALLTYPE net_writer_byte(NetWriter *self, BYTE value) {
    return net_writer_append(self, &value, 1);
}

static HRESULT STDMETHODCALLTYPE net_writer_bytes(NetWriter *self, UINT32 size, BYTE *data) {
    return net_writer_append(self, data, size);
}

static HRESULT STDMETHODCALLTYPE net_writer_buffer_range(NetWriter *self, NetCom *buffer,
                                                         UINT32 start, UINT32 count) {
    BYTE *data;
    UINT32 length;
    if (!buffer) {
        return E_POINTER;
    }
    if (!net_buffer_bytes(buffer, &data, &length)) {
        return E_INVALIDARG;
    }
    if (start > length || count > length - start) {
        return NET_E_BOUNDS;
    }
    return net_writer_append(self, data + start, count);
}

static HRESULT STDMETHODCALLTYPE net_writer_buffer(NetWriter *self, NetCom *buffer) {
    BYTE *data;
    UINT32 length;
    if (!buffer) {
        return E_POINTER;
    }
    if (!net_buffer_bytes(buffer, &data, &length)) {
        return E_INVALIDARG;
    }
    return net_writer_append(self, data, length);
}

static HRESULT STDMETHODCALLTYPE net_writer_boolean(NetWriter *self, BYTE value) {
    BYTE byte = value ? 1 : 0;
    return net_writer_append(self, &byte, 1);
}

static HRESULT STDMETHODCALLTYPE net_writer_guid(NetWriter *self, GUID value) {
    HRESULT result = net_writer_number(self, value.Data1, 4);
    if (SUCCEEDED(result)) {
        result = net_writer_number(self, value.Data2, 2);
    }
    if (SUCCEEDED(result)) {
        result = net_writer_number(self, value.Data3, 2);
    }
    return SUCCEEDED(result) ? net_writer_append(self, value.Data4, 8) : result;
}

static HRESULT STDMETHODCALLTYPE net_writer_int16(NetWriter *self, INT16 value) {
    return net_writer_number(self, (UINT16)value, 2);
}

static HRESULT STDMETHODCALLTYPE net_writer_int32(NetWriter *self, INT32 value) {
    return net_writer_number(self, (UINT32)value, 4);
}

static HRESULT STDMETHODCALLTYPE net_writer_int64(NetWriter *self, INT64 value) {
    return net_writer_number(self, (UINT64)value, 8);
}

static HRESULT STDMETHODCALLTYPE net_writer_uint16(NetWriter *self, UINT16 value) {
    return net_writer_number(self, value, 2);
}

static HRESULT STDMETHODCALLTYPE net_writer_uint32(NetWriter *self, UINT32 value) {
    return net_writer_number(self, value, 4);
}

static HRESULT STDMETHODCALLTYPE net_writer_uint64(NetWriter *self, UINT64 value) {
    return net_writer_number(self, value, 8);
}

static HRESULT STDMETHODCALLTYPE net_writer_single(NetWriter *self, float value) {
    UINT32 bits;
    memory_copy(&bits, &value, sizeof(bits));
    return net_writer_number(self, bits, 4);
}

static HRESULT STDMETHODCALLTYPE net_writer_double(NetWriter *self, double value) {
    UINT64 bits;
    memory_copy(&bits, &value, sizeof(bits));
    return net_writer_number(self, bits, 8);
}

static HRESULT STDMETHODCALLTYPE net_writer_date(NetWriter *self, INT64 value) {
    return net_writer_number(self, (UINT64)value, 8);
}

/* Azimuth's HSTRING: magic "FSH2", length, reserved, then the text pointer
   (same layout net_trace_hstring reads). */
static int net_writer_hstring(void *value, const wchar_t **text, unsigned *length) {
    struct {
        unsigned magic, length;
        unsigned long long reserved;
        const wchar_t *text;
    } header;
    *text = L"";
    *length = 0;
    if (!value) {
        return 1;
    }
    if (!memory_read(value, &header, sizeof(header)) || header.magic != 0x32485346u) {
        return 0;
    }
    *text = header.text;
    *length = header.length;
    return 1;
}

/* Encoding: 0 UTF-8, 1 UTF-16LE, 2 UTF-16BE. */
static HRESULT net_writer_text(NetWriter *self, void *value, UINT32 *size, int store) {
    const wchar_t *text;
    unsigned length;
    if (!size) {
        return E_POINTER;
    }
    *size = 0;
    if (!net_writer_hstring(value, &text, &length)) {
        net_writer_log_once(&net_writer_logged,
                            "WINRT: DataWriter got a string that is not an Azimuth HSTRING.\r\n");
        return E_INVALIDARG;
    }
    if (!length) {
        return S_OK;
    }
    if (self->encoding == 0) {
        int bytes = WideCharToMultiByte(CP_UTF8, 0, text, (int)length, 0, 0, 0, 0);
        if (bytes <= 0) {
            return E_INVALIDARG;
        }
        *size = (UINT32)bytes;
        if (!store) {
            return S_OK;
        }
        if (self->closed) {
            return NET_RO_E_CLOSED;
        }
        if (!buffer_reserve(&self->data, (size_t)bytes)) {
            return E_OUTOFMEMORY;
        }
        WideCharToMultiByte(CP_UTF8, 0, text, (int)length,
                            (char *)self->data.data + self->data.size, bytes, 0, 0);
        self->data.size += (size_t)bytes;
        self->data.data[self->data.size] = self->data.data[self->data.size + 1] = 0;
        return S_OK;
    }
    *size = length * 2;
    if (!store) {
        return S_OK;
    }
    for (unsigned i = 0; i < length; ++i) {
        unsigned char pair[2] = {(unsigned char)(text[i] & 0xff), (unsigned char)(text[i] >> 8)};
        if (self->encoding == 2) {
            unsigned char swap = pair[0];
            pair[0] = pair[1];
            pair[1] = swap;
        }
        HRESULT result = net_writer_append(self, pair, 2);
        if (FAILED(result)) {
            return result;
        }
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_writer_string(NetWriter *self, void *value, UINT32 *size) {
    return net_writer_text(self, value, size, 1);
}

static HRESULT STDMETHODCALLTYPE net_writer_measure(NetWriter *self, void *value, UINT32 *size) {
    return net_writer_text(self, value, size, 0);
}

static HRESULT STDMETHODCALLTYPE net_writer_async(NetWriter *self, void **operation) {
    (void)self;
    if (operation) {
        *operation = 0;
    }
    net_writer_log_once(&net_writer_logged,
                        "WINRT: DataWriter Store/Flush/DetachStream called; not implemented.\r\n");
    return NET_E_NOTIMPL;
}

static HRESULT STDMETHODCALLTYPE net_writer_detach(NetWriter *self, NetCom **buffer) {
    if (!buffer) {
        return E_POINTER;
    }
    *buffer = 0;
    if (self->closed) {
        return NET_RO_E_CLOSED;
    }
    HRESULT result = net_buffer_create(self->data.data, self->data.size, buffer);
    if (SUCCEEDED(result)) {
        buffer_free(&self->data);
    }
    return result;
}

static const NetWriterVtbl net_writer_vtable = {
    net_writer_query,        net_writer_add_ref,      net_writer_release,
    net_writer_iids,         net_writer_class_name,   net_writer_trust,
    net_writer_unstored,     net_writer_get_encoding, net_writer_put_encoding,
    net_writer_get_order,    net_writer_put_order,    net_writer_byte,
    net_writer_bytes,        net_writer_buffer,       net_writer_buffer_range,
    net_writer_boolean,      net_writer_guid,         net_writer_int16,
    net_writer_int32,        net_writer_int64,        net_writer_uint16,
    net_writer_uint32,       net_writer_uint64,       net_writer_single,
    net_writer_double,       net_writer_date,         net_writer_date,
    net_writer_string,       net_writer_measure,      net_writer_async,
    net_writer_async,        net_writer_detach,       net_writer_async};

static HRESULT STDMETHODCALLTYPE net_closable_query(NetWriterClosable *self, REFIID iid,
                                                    void **out) {
    return net_writer_query(net_writer_from_closable(self), iid, out);
}

static ULONG STDMETHODCALLTYPE net_closable_add_ref(NetWriterClosable *self) {
    return net_writer_add_ref(net_writer_from_closable(self));
}

static ULONG STDMETHODCALLTYPE net_closable_release(NetWriterClosable *self) {
    return net_writer_release(net_writer_from_closable(self));
}

static HRESULT STDMETHODCALLTYPE net_closable_iids(NetWriterClosable *self, ULONG *count,
                                                   void **iids) {
    return net_writer_iids(net_writer_from_closable(self), count, iids);
}

static HRESULT STDMETHODCALLTYPE net_closable_class_name(NetWriterClosable *self, void **name) {
    return net_writer_class_name(net_writer_from_closable(self), name);
}

static HRESULT STDMETHODCALLTYPE net_closable_trust(NetWriterClosable *self, int *level) {
    return net_writer_trust(net_writer_from_closable(self), level);
}

static HRESULT STDMETHODCALLTYPE net_closable_close(NetWriterClosable *self) {
    NetWriter *writer = net_writer_from_closable(self);
    writer->closed = 1;
    buffer_free(&writer->data);
    return S_OK;
}

static const NetWriterClosableVtbl net_closable_vtable = {
    net_closable_query, net_closable_add_ref,    net_closable_release, net_closable_iids,
    net_closable_class_name, net_closable_trust, net_closable_close};

/* IActivationFactory that makes NetWriters; other interfaces (IDataWriterFactory)
   are Azimuth's. */
typedef struct NetWriterFactory NetWriterFactory;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetWriterFactory *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetWriterFactory *);
    ULONG(STDMETHODCALLTYPE *Release)(NetWriterFactory *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetWriterFactory *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetWriterFactory *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetWriterFactory *, int *);
    HRESULT(STDMETHODCALLTYPE *ActivateInstance)(NetWriterFactory *, NetWriter **);
} NetWriterFactoryVtbl;
struct NetWriterFactory {
    const NetWriterFactoryVtbl *lpVtbl;
    volatile LONG references;
    NetCom *inner;
};

static HRESULT STDMETHODCALLTYPE net_wfactory_query(NetWriterFactory *self, REFIID iid,
                                                    void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_inspectable, 16) ||
        memory_equal(iid, net_iid_activation_factory, 16) || memory_equal(iid, net_iid_agile, 16)) {
        *out = self;
        InterlockedIncrement(&self->references);
        return S_OK;
    }
    return self->inner ? self->inner->lpVtbl->QueryInterface(self->inner, iid, out)
                       : E_NOINTERFACE;
}

static ULONG STDMETHODCALLTYPE net_wfactory_add_ref(NetWriterFactory *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}

static ULONG STDMETHODCALLTYPE net_wfactory_release(NetWriterFactory *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        if (self->inner) {
            self->inner->lpVtbl->Release(self->inner);
        }
        net_free(self);
    }
    return (ULONG)left;
}

static HRESULT STDMETHODCALLTYPE net_wfactory_iids(NetWriterFactory *self, ULONG *count,
                                                   void **iids) {
    (void)self;
    if (count) {
        *count = 0;
    }
    if (iids) {
        *iids = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_wfactory_class_name(NetWriterFactory *self, void **name) {
    (void)self;
    if (name) {
        *name = 0;
    }
    return NET_E_NOTIMPL;
}

static HRESULT STDMETHODCALLTYPE net_wfactory_trust(NetWriterFactory *self, int *level) {
    (void)self;
    if (level) {
        *level = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_wfactory_activate(NetWriterFactory *self, NetWriter **out) {
    (void)self;
    if (!out) {
        return E_POINTER;
    }
    NetWriter *writer = net_alloc(sizeof(*writer));
    *out = writer;
    if (!writer) {
        return E_OUTOFMEMORY;
    }
    writer->lpVtbl = &net_writer_vtable;
    writer->closable.lpVtbl = &net_closable_vtable;
    writer->references = 1;
    writer->byte_order = 1; /* DataWriter's default is BigEndian */
    return S_OK;
}

static const NetWriterFactoryVtbl net_wfactory_vtable = {
    net_wfactory_query,      net_wfactory_add_ref, net_wfactory_release, net_wfactory_iids,
    net_wfactory_class_name, net_wfactory_trust,   net_wfactory_activate};

/* Writes 4 bytes with Azimuth's writer and detaches them. 1: they came back. */
static int net_writer_self_test(NetCom *factory) {
    static BYTE probe[4] = {'F', 'H', '2', '!'};
    NetCom *instance = 0, *writer = 0, *buffer = 0;
    UINT32 length = 0xffffffffu;
    BYTE *data = 0;
    HRESULT step = ((const NetActivationFactoryVtbl *)(const void *)factory->lpVtbl)
                       ->ActivateInstance(factory, &instance);
    if (SUCCEEDED(step) && instance) {
        step = instance->lpVtbl->QueryInterface(instance, (REFIID)net_iid_data_writer,
                                                (void **)&writer);
    }
    if (SUCCEEDED(step) && writer) {
        step = writer->lpVtbl->WriteBytes(writer, sizeof(probe), probe);
    }
    if (SUCCEEDED(step) && writer) {
        step = writer->lpVtbl->DetachBuffer(writer, &buffer);
    }
    int ok = SUCCEEDED(step) && buffer && net_buffer_bytes(buffer, &data, &length) &&
             length == sizeof(probe) && memory_equal(data, probe, sizeof(probe));
    Buffer line = {0};
    if (buffer_append_text(&line, "WINRT: DataWriter self-test: Azimuth's writer ") &&
        buffer_append_text(&line, ok ? "works" : "loses the bytes") &&
        buffer_append_text(&line, " (step ") && buffer_append_hresult(&line, step) &&
        buffer_append_text(&line, ", detached length ") &&
        buffer_append_number(&line, buffer ? length : 0) &&
        buffer_append_text(&line, ok ? ").\r\n" : "); using FH2Online's DataWriter.\r\n")) {
        log_append((const char *)line.data);
    }
    buffer_free(&line);
    if (buffer) {
        buffer->lpVtbl->Release(buffer);
    }
    if (writer) {
        writer->lpVtbl->Release(writer);
    }
    if (instance) {
        instance->lpVtbl->Release(instance);
    }
    return ok;
}

/* Called with Azimuth's IActivationFactory for DataWriter; may swap it. */
static void net_writer_filter(void **factory) {
    AcquireSRWLockExclusive(&net_writer_lock);
    int state = net_writer_state;
    ReleaseSRWLockExclusive(&net_writer_lock);
    if (!state) {
        int works = net_writer_self_test((NetCom *)*factory);
        AcquireSRWLockExclusive(&net_writer_lock);
        if (!net_writer_state) {
            net_writer_state = works ? 1 : 2;
        }
        state = net_writer_state;
        ReleaseSRWLockExclusive(&net_writer_lock);
    }
    if (state != 2) {
        return;
    }
    NetWriterFactory *ours = net_alloc(sizeof(*ours));
    if (!ours) {
        return;
    }
    ours->lpVtbl = &net_wfactory_vtable;
    ours->references = 1;
    ours->inner = (NetCom *)*factory; /* takes over the caller's reference */
    *factory = ours;
}

/* ------------------------------------------------- DataReader replacement */
/* Same story for the reader: XSAPI turns the detached buffer into the body
   text with DataReader.FromBuffer(buffer).ReadString(length), and Azimuth's
   generic DataReader hands back nothing. Strings are made with Azimuth's own
   WindowsCreateString, so the rest of the process can read them. */

static const unsigned char net_iid_data_reader[16] = {0x29, 0x00, 0xb5, 0xe2, 0xc1, 0xb4,
                                                      0x14, 0x43, 0xa4, 0xb8, 0xfb, 0x81,
                                                      0x3a, 0x2f, 0x27, 0x5e};
static const unsigned char net_iid_data_reader_statics[16] = {0xc8, 0xbf, 0xfc, 0x11, 0x3a, 0xf9,
                                                              0x1b, 0x47, 0xb1, 0x21, 0xf3, 0x79,
                                                              0xe3, 0x49, 0x31, 0x3c};
typedef HRESULT (*NetHstringCreateFn)(const wchar_t *, unsigned, void **);

static int net_reader_state; /* 0 untested, 1 Azimuth's works, 2 replaced */

typedef struct NetReader NetReader;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetReader *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetReader *);
    ULONG(STDMETHODCALLTYPE *Release)(NetReader *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetReader *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetReader *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetReader *, int *);
    HRESULT(STDMETHODCALLTYPE *get_UnconsumedBufferLength)(NetReader *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *get_UnicodeEncoding)(NetReader *, int *);
    HRESULT(STDMETHODCALLTYPE *put_UnicodeEncoding)(NetReader *, int);
    HRESULT(STDMETHODCALLTYPE *get_ByteOrder)(NetReader *, int *);
    HRESULT(STDMETHODCALLTYPE *put_ByteOrder)(NetReader *, int);
    HRESULT(STDMETHODCALLTYPE *get_InputStreamOptions)(NetReader *, int *);
    HRESULT(STDMETHODCALLTYPE *put_InputStreamOptions)(NetReader *, int);
    HRESULT(STDMETHODCALLTYPE *ReadByte)(NetReader *, BYTE *);
    HRESULT(STDMETHODCALLTYPE *ReadBytes)(NetReader *, UINT32, BYTE *);
    HRESULT(STDMETHODCALLTYPE *ReadBuffer)(NetReader *, UINT32, NetCom **);
    HRESULT(STDMETHODCALLTYPE *ReadBoolean)(NetReader *, BYTE *);
    HRESULT(STDMETHODCALLTYPE *ReadGuid)(NetReader *, GUID *);
    HRESULT(STDMETHODCALLTYPE *ReadInt16)(NetReader *, INT16 *);
    HRESULT(STDMETHODCALLTYPE *ReadInt32)(NetReader *, INT32 *);
    HRESULT(STDMETHODCALLTYPE *ReadInt64)(NetReader *, INT64 *);
    HRESULT(STDMETHODCALLTYPE *ReadUInt16)(NetReader *, UINT16 *);
    HRESULT(STDMETHODCALLTYPE *ReadUInt32)(NetReader *, UINT32 *);
    HRESULT(STDMETHODCALLTYPE *ReadUInt64)(NetReader *, UINT64 *);
    HRESULT(STDMETHODCALLTYPE *ReadSingle)(NetReader *, float *);
    HRESULT(STDMETHODCALLTYPE *ReadDouble)(NetReader *, double *);
    HRESULT(STDMETHODCALLTYPE *ReadString)(NetReader *, UINT32, void **);
    HRESULT(STDMETHODCALLTYPE *ReadDateTime)(NetReader *, INT64 *);
    HRESULT(STDMETHODCALLTYPE *ReadTimeSpan)(NetReader *, INT64 *);
    HRESULT(STDMETHODCALLTYPE *LoadAsync)(NetReader *, UINT32, void **);
    HRESULT(STDMETHODCALLTYPE *DetachBuffer)(NetReader *, NetCom **);
    HRESULT(STDMETHODCALLTYPE *DetachStream)(NetReader *, void **);
} NetReaderVtbl;
struct NetReader {
    const NetReaderVtbl *lpVtbl;
    NetWriterClosable closable;
    volatile LONG references;
    int closed, byte_order, encoding, options;
    Buffer data;
    size_t position;
};

static NetReader *net_reader_from_closable(NetWriterClosable *closable) {
    return (NetReader *)(void *)((unsigned char *)closable - offsetof(NetReader, closable));
}

static HRESULT STDMETHODCALLTYPE net_reader_query(NetReader *self, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_inspectable, 16) ||
        memory_equal(iid, net_iid_data_reader, 16) || memory_equal(iid, net_iid_agile, 16)) {
        *out = self;
    } else if (memory_equal(iid, net_iid_closable, 16)) {
        *out = &self->closable;
    } else {
        return E_NOINTERFACE;
    }
    InterlockedIncrement(&self->references);
    return S_OK;
}

static ULONG STDMETHODCALLTYPE net_reader_add_ref(NetReader *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}

static ULONG STDMETHODCALLTYPE net_reader_release(NetReader *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        buffer_free(&self->data);
        net_free(self);
    }
    return (ULONG)left;
}

static HRESULT STDMETHODCALLTYPE net_reader_iids(NetReader *self, ULONG *count, void **iids) {
    (void)self;
    if (count) {
        *count = 0;
    }
    if (iids) {
        *iids = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_class_name(NetReader *self, void **name) {
    (void)self;
    if (name) {
        *name = 0;
    }
    return NET_E_NOTIMPL;
}

static HRESULT STDMETHODCALLTYPE net_reader_trust(NetReader *self, int *level) {
    (void)self;
    if (level) {
        *level = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_unconsumed(NetReader *self, UINT32 *length) {
    if (!length) {
        return E_POINTER;
    }
    *length = (UINT32)(self->data.size - self->position);
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_get_encoding(NetReader *self, int *value) {
    if (!value) {
        return E_POINTER;
    }
    *value = self->encoding;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_put_encoding(NetReader *self, int value) {
    self->encoding = value;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_get_order(NetReader *self, int *value) {
    if (!value) {
        return E_POINTER;
    }
    *value = self->byte_order;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_put_order(NetReader *self, int value) {
    self->byte_order = value;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_get_options(NetReader *self, int *value) {
    if (!value) {
        return E_POINTER;
    }
    *value = self->options;
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_put_options(NetReader *self, int value) {
    self->options = value;
    return S_OK;
}

/* Next `size` bytes, or E_BOUNDS when the buffer is shorter. */
static HRESULT net_reader_take(NetReader *self, size_t size, const unsigned char **data) {
    if (self->closed) {
        return NET_RO_E_CLOSED;
    }
    if (size > self->data.size - self->position) {
        return NET_E_BOUNDS;
    }
    *data = self->data.data + self->position;
    self->position += size;
    return S_OK;
}

static HRESULT net_reader_number(NetReader *self, unsigned size, unsigned long long *value) {
    const unsigned char *bytes;
    HRESULT result = net_reader_take(self, size, &bytes);
    *value = 0;
    if (FAILED(result)) {
        return result;
    }
    for (unsigned i = 0; i < size; ++i) {
        unsigned shift = 8 * (self->byte_order ? size - 1 - i : i);
        *value |= (unsigned long long)bytes[i] << shift;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_reader_byte(NetReader *self, BYTE *value) {
    const unsigned char *bytes;
    if (!value) {
        return E_POINTER;
    }
    HRESULT result = net_reader_take(self, 1, &bytes);
    *value = SUCCEEDED(result) ? bytes[0] : 0;
    return result;
}

static HRESULT STDMETHODCALLTYPE net_reader_bytes(NetReader *self, UINT32 size, BYTE *value) {
    const unsigned char *bytes;
    if (size && !value) {
        return E_POINTER;
    }
    HRESULT result = net_reader_take(self, size, &bytes);
    if (SUCCEEDED(result) && size) {
        memory_copy(value, bytes, size);
    }
    return result;
}

static HRESULT STDMETHODCALLTYPE net_reader_buffer(NetReader *self, UINT32 size, NetCom **out) {
    const unsigned char *bytes;
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    HRESULT result = net_reader_take(self, size, &bytes);
    return SUCCEEDED(result) ? net_buffer_create(bytes, size, out) : result;
}

static HRESULT STDMETHODCALLTYPE net_reader_boolean(NetReader *self, BYTE *value) {
    HRESULT result = net_reader_byte(self, value);
    if (SUCCEEDED(result)) {
        *value = *value ? 1 : 0;
    }
    return result;
}

static HRESULT STDMETHODCALLTYPE net_reader_guid(NetReader *self, GUID *value) {
    unsigned long long a, b, c;
    const unsigned char *rest;
    if (!value) {
        return E_POINTER;
    }
    HRESULT result = net_reader_number(self, 4, &a);
    if (SUCCEEDED(result)) {
        result = net_reader_number(self, 2, &b);
    }
    if (SUCCEEDED(result)) {
        result = net_reader_number(self, 2, &c);
    }
    if (SUCCEEDED(result)) {
        result = net_reader_take(self, 8, &rest);
    }
    if (SUCCEEDED(result)) {
        value->Data1 = (unsigned long)a;
        value->Data2 = (unsigned short)b;
        value->Data3 = (unsigned short)c;
        memory_copy(value->Data4, rest, 8);
    }
    return result;
}

#define NET_READER_NUMBER(name, type, size)                                           \
    static HRESULT STDMETHODCALLTYPE name(NetReader *self, type *value) {             \
        unsigned long long raw;                                                       \
        if (!value) {                                                                 \
            return E_POINTER;                                                         \
        }                                                                             \
        HRESULT result = net_reader_number(self, size, &raw);                         \
        if (SUCCEEDED(result)) {                                                      \
            *value = (type)raw;                                                       \
        }                                                                             \
        return result;                                                                \
    }
NET_READER_NUMBER(net_reader_int16, INT16, 2)
NET_READER_NUMBER(net_reader_int32, INT32, 4)
NET_READER_NUMBER(net_reader_int64, INT64, 8)
NET_READER_NUMBER(net_reader_uint16, UINT16, 2)
NET_READER_NUMBER(net_reader_uint32, UINT32, 4)
NET_READER_NUMBER(net_reader_uint64, UINT64, 8)

static HRESULT STDMETHODCALLTYPE net_reader_single(NetReader *self, float *value) {
    unsigned long long raw;
    if (!value) {
        return E_POINTER;
    }
    HRESULT result = net_reader_number(self, 4, &raw);
    if (SUCCEEDED(result)) {
        UINT32 bits = (UINT32)raw;
        memory_copy(value, &bits, sizeof(bits));
    }
    return result;
}

static HRESULT STDMETHODCALLTYPE net_reader_double(NetReader *self, double *value) {
    unsigned long long raw;
    if (!value) {
        return E_POINTER;
    }
    HRESULT result = net_reader_number(self, 8, &raw);
    if (SUCCEEDED(result)) {
        memory_copy(value, &raw, sizeof(raw));
    }
    return result;
}

/* count is in code units: bytes for UTF-8, wchar_t for UTF-16. */
static HRESULT STDMETHODCALLTYPE net_reader_string(NetReader *self, UINT32 count, void **out) {
    const unsigned char *bytes;
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    size_t size = self->encoding ? (size_t)count * 2 : count;
    HRESULT result = net_reader_take(self, size, &bytes);
    if (FAILED(result)) {
        return result;
    }
    NetHstringCreateFn create =
        (NetHstringCreateFn)(uintptr_t)(net_azimuth_base + net_offsets->hstring_create);
    if (!count) {
        return create(L"", 0, out);
    }
    wchar_t *text;
    int length;
    if (self->encoding == 0) {
        length = MultiByteToWideChar(CP_UTF8, 0, (const char *)bytes, (int)count, 0, 0);
        if (length <= 0) {
            return E_INVALIDARG;
        }
        text = net_alloc(((size_t)length + 1) * sizeof(wchar_t));
        if (!text) {
            return E_OUTOFMEMORY;
        }
        MultiByteToWideChar(CP_UTF8, 0, (const char *)bytes, (int)count, text, length);
    } else {
        length = (int)count;
        text = net_alloc(((size_t)length + 1) * sizeof(wchar_t));
        if (!text) {
            return E_OUTOFMEMORY;
        }
        for (UINT32 i = 0; i < count; ++i) {
            unsigned lo = bytes[2 * i], hi = bytes[2 * i + 1];
            text[i] = (wchar_t)(self->encoding == 2 ? (lo << 8) | hi : (hi << 8) | lo);
        }
    }
    result = create(text, (unsigned)length, out);
    net_free(text);
    return result;
}

static HRESULT STDMETHODCALLTYPE net_reader_date(NetReader *self, INT64 *value) {
    return net_reader_int64(self, value);
}

static HRESULT STDMETHODCALLTYPE net_reader_load(NetReader *self, UINT32 count, void **operation) {
    (void)self;
    (void)count;
    if (operation) {
        *operation = 0;
    }
    net_writer_log_once(&net_writer_logged,
                        "WINRT: DataReader LoadAsync/DetachStream called; not implemented.\r\n");
    return NET_E_NOTIMPL;
}

static HRESULT STDMETHODCALLTYPE net_reader_detach(NetReader *self, NetCom **out) {
    const unsigned char *bytes;
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    size_t left = self->data.size - self->position;
    HRESULT result = net_reader_take(self, left, &bytes);
    return SUCCEEDED(result) ? net_buffer_create(bytes, left, out) : result;
}

static HRESULT STDMETHODCALLTYPE net_reader_detach_stream(NetReader *self, void **out) {
    return net_reader_load(self, 0, out);
}

static const NetReaderVtbl net_reader_vtable = {
    net_reader_query,        net_reader_add_ref,     net_reader_release,
    net_reader_iids,         net_reader_class_name,  net_reader_trust,
    net_reader_unconsumed,   net_reader_get_encoding, net_reader_put_encoding,
    net_reader_get_order,    net_reader_put_order,   net_reader_get_options,
    net_reader_put_options,  net_reader_byte,        net_reader_bytes,
    net_reader_buffer,       net_reader_boolean,     net_reader_guid,
    net_reader_int16,        net_reader_int32,       net_reader_int64,
    net_reader_uint16,       net_reader_uint32,      net_reader_uint64,
    net_reader_single,       net_reader_double,      net_reader_string,
    net_reader_date,         net_reader_date,        net_reader_load,
    net_reader_detach,       net_reader_detach_stream};

static HRESULT STDMETHODCALLTYPE net_rclosable_query(NetWriterClosable *self, REFIID iid,
                                                     void **out) {
    return net_reader_query(net_reader_from_closable(self), iid, out);
}

static ULONG STDMETHODCALLTYPE net_rclosable_add_ref(NetWriterClosable *self) {
    return net_reader_add_ref(net_reader_from_closable(self));
}

static ULONG STDMETHODCALLTYPE net_rclosable_release(NetWriterClosable *self) {
    return net_reader_release(net_reader_from_closable(self));
}

static HRESULT STDMETHODCALLTYPE net_rclosable_close(NetWriterClosable *self) {
    NetReader *reader = net_reader_from_closable(self);
    reader->closed = 1;
    buffer_free(&reader->data);
    reader->position = 0;
    return S_OK;
}

static const NetWriterClosableVtbl net_rclosable_vtable = {
    net_rclosable_query,     net_rclosable_add_ref, net_rclosable_release, net_closable_iids,
    net_closable_class_name, net_closable_trust,    net_rclosable_close};

/* IDataReaderStatics: FromBuffer makes NetReaders over a copy of the bytes. */
typedef struct NetReaderStatics NetReaderStatics;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetReaderStatics *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetReaderStatics *);
    ULONG(STDMETHODCALLTYPE *Release)(NetReaderStatics *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetReaderStatics *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetReaderStatics *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetReaderStatics *, int *);
    HRESULT(STDMETHODCALLTYPE *FromBuffer)(NetReaderStatics *, NetCom *, NetCom **);
} NetReaderStaticsVtbl;
struct NetReaderStatics {
    const NetReaderStaticsVtbl *lpVtbl;
    volatile LONG references;
    NetCom *inner;
};

static HRESULT STDMETHODCALLTYPE net_rstatics_query(NetReaderStatics *self, REFIID iid,
                                                    void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_inspectable, 16) ||
        memory_equal(iid, net_iid_data_reader_statics, 16) || memory_equal(iid, net_iid_agile, 16)) {
        *out = self;
        InterlockedIncrement(&self->references);
        return S_OK;
    }
    return self->inner ? self->inner->lpVtbl->QueryInterface(self->inner, iid, out)
                       : E_NOINTERFACE;
}

static ULONG STDMETHODCALLTYPE net_rstatics_add_ref(NetReaderStatics *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}

static ULONG STDMETHODCALLTYPE net_rstatics_release(NetReaderStatics *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        if (self->inner) {
            self->inner->lpVtbl->Release(self->inner);
        }
        net_free(self);
    }
    return (ULONG)left;
}

static HRESULT STDMETHODCALLTYPE net_rstatics_iids(NetReaderStatics *self, ULONG *count,
                                                   void **iids) {
    (void)self;
    if (count) {
        *count = 0;
    }
    if (iids) {
        *iids = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_rstatics_class_name(NetReaderStatics *self, void **name) {
    (void)self;
    if (name) {
        *name = 0;
    }
    return NET_E_NOTIMPL;
}

static HRESULT STDMETHODCALLTYPE net_rstatics_trust(NetReaderStatics *self, int *level) {
    (void)self;
    if (level) {
        *level = 0;
    }
    return S_OK;
}

static HRESULT STDMETHODCALLTYPE net_rstatics_from_buffer(NetReaderStatics *self, NetCom *buffer,
                                                          NetCom **out) {
    BYTE *data;
    UINT32 length;
    (void)self;
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (!buffer) {
        return E_INVALIDARG;
    }
    if (!net_buffer_bytes(buffer, &data, &length)) {
        return E_INVALIDARG;
    }
    NetReader *reader = net_alloc(sizeof(*reader));
    if (!reader) {
        return E_OUTOFMEMORY;
    }
    reader->lpVtbl = &net_reader_vtable;
    reader->closable.lpVtbl = &net_rclosable_vtable;
    reader->references = 1;
    reader->byte_order = 1; /* DataReader's default is BigEndian */
    if (length && !buffer_append(&reader->data, data, length)) {
        net_free(reader);
        return E_OUTOFMEMORY;
    }
    *out = (NetCom *)(void *)reader;
    return S_OK;
}

static const NetReaderStaticsVtbl net_rstatics_vtable = {
    net_rstatics_query,      net_rstatics_add_ref, net_rstatics_release, net_rstatics_iids,
    net_rstatics_class_name, net_rstatics_trust,   net_rstatics_from_buffer};

/* Reads "FH2!" back through Azimuth's reader. 1: the string came back. */
static int net_reader_self_test(NetCom *statics) {
    static const unsigned char probe[4] = {'F', 'H', '2', '!'};
    NetCom *buffer = 0, *reader = 0;
    void *text = 0;
    const wchar_t *chars = L"";
    unsigned length = 0;
    HRESULT step = net_buffer_create(probe, sizeof(probe), &buffer);
    if (SUCCEEDED(step)) {
        step = ((const NetReaderStaticsVtbl *)(const void *)statics->lpVtbl)
                   ->FromBuffer((NetReaderStatics *)(void *)statics, buffer, &reader);
    }
    if (SUCCEEDED(step) && reader) {
        step = ((const NetReaderVtbl *)(const void *)reader->lpVtbl)
                   ->ReadString((NetReader *)(void *)reader, sizeof(probe), &text);
    }
    int ok = SUCCEEDED(step) && text && net_writer_hstring(text, &chars, &length) &&
             length == 4 && chars[0] == L'F' && chars[3] == L'!';
    Buffer line = {0};
    if (buffer_append_text(&line, "WINRT: DataReader self-test: Azimuth's reader ") &&
        buffer_append_text(&line, ok ? "works" : "loses the text") &&
        buffer_append_text(&line, " (step ") && buffer_append_hresult(&line, step) &&
        buffer_append_text(&line, ", string length ") && buffer_append_number(&line, length) &&
        buffer_append_text(&line, ok ? ").\r\n" : "); using FH2Online's DataReader.\r\n")) {
        log_append((const char *)line.data);
    }
    buffer_free(&line);
    if (reader) {
        reader->lpVtbl->Release(reader);
    }
    if (buffer) {
        buffer->lpVtbl->Release(buffer);
    }
    return ok; /* the probe HSTRING is leaked once: Azimuth's delete is not mapped */
}

static void net_reader_filter(void **statics) {
    AcquireSRWLockExclusive(&net_writer_lock);
    int state = net_reader_state;
    ReleaseSRWLockExclusive(&net_writer_lock);
    if (!state) {
        int works = net_reader_self_test((NetCom *)*statics);
        AcquireSRWLockExclusive(&net_writer_lock);
        if (!net_reader_state) {
            net_reader_state = works ? 1 : 2;
        }
        state = net_reader_state;
        ReleaseSRWLockExclusive(&net_writer_lock);
    }
    if (state != 2) {
        return;
    }
    NetReaderStatics *ours = net_alloc(sizeof(*ours));
    if (!ours) {
        return;
    }
    ours->lpVtbl = &net_rstatics_vtable;
    ours->references = 1;
    ours->inner = (NetCom *)*statics;
    *statics = ours;
}

/* Windows.Networking.Connectivity. The game's CLobby (FUN_140a2b900) sets its "connected to
   Xbox Live" flag only when GetInternetConnectionProfile()->GetNetworkConnectivityLevel() is
   4 (XboxLiveAccess, console-only). The profile Azimuth hands out never says 4, so the main
   menu locks every online feature behind IDS_Popup_FeatureLocked_LSPDown_Message. These shims
   forward everything to Azimuth's objects and only raise the level to 4. */
static const unsigned char net_iid_network_information_statics[16] = {
    0x51, 0xf8, 0x74, 0x50, 0x0d, 0x95, 0x65, 0x41,
    0x9c, 0x15, 0x36, 0x56, 0x19, 0x48, 0x1e, 0xea};
static const unsigned char net_iid_connection_profile[16] = {
    0x3c, 0x14, 0xba, 0x71, 0x8e, 0x59, 0xd0, 0x49,
    0x84, 0xeb, 0x8f, 0xeb, 0xae, 0xdc, 0xc1, 0x95};
#define NET_XBOX_LIVE_ACCESS 4

typedef struct NetShim NetShim;
struct NetShim {
    void *const *lpVtbl;
    volatile LONG references;
    NetCom *inner;
    const unsigned char *iid;
};
typedef HRESULT(STDMETHODCALLTYPE *NetShimCall)(NetCom *, uintptr_t, uintptr_t, uintptr_t,
                                                uintptr_t, uintptr_t);
static volatile LONG net_connectivity_logged;

/* A profile the shim made up because Azimuth had none (inner is null): getters that only
   take an out pointer answer null; the rest are not implemented. */
static HRESULT net_shim_stand_alone(unsigned slot, uintptr_t out) {
    if (slot == 12 || slot == 13) {
        return NET_E_NOTIMPL;
    }
    if (out) {
        *(void **)out = 0;
    }
    return S_OK;
}

static void *net_shim_slot(NetShim *self, unsigned slot) {
    return ((void *const *)(const void *)self->inner->lpVtbl)[slot];
}

/* Methods past IInspectable take at most five integer or pointer arguments in these two
   interfaces; passing five on is harmless for those that take fewer. */
#define NET_SHIM_FORWARD(slot)                                                                 \
    static HRESULT STDMETHODCALLTYPE net_shim_forward##slot(                                   \
        NetShim *self, uintptr_t a, uintptr_t b, uintptr_t c, uintptr_t d, uintptr_t e) {       \
        if (!self->inner) {                                                                    \
            return net_shim_stand_alone(slot, a);                                              \
        }                                                                                      \
        NetShimCall call = (NetShimCall)(uintptr_t)net_shim_slot(self, slot);                   \
        return call(self->inner, a, b, c, d, e);                                               \
    }
NET_SHIM_FORWARD(3)
NET_SHIM_FORWARD(4)
NET_SHIM_FORWARD(5)
NET_SHIM_FORWARD(6)
NET_SHIM_FORWARD(8)
NET_SHIM_FORWARD(9)
NET_SHIM_FORWARD(10)
NET_SHIM_FORWARD(11)
NET_SHIM_FORWARD(12)
NET_SHIM_FORWARD(13)
NET_SHIM_FORWARD(14)
#undef NET_SHIM_FORWARD

static HRESULT STDMETHODCALLTYPE net_shim_query(NetShim *self, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_inspectable, 16) ||
        memory_equal(iid, net_iid_agile, 16) || memory_equal(iid, self->iid, 16)) {
        *out = self;
        InterlockedIncrement(&self->references);
        return S_OK;
    }
    return self->inner ? self->inner->lpVtbl->QueryInterface(self->inner, iid, out)
                       : E_NOINTERFACE;
}

static ULONG STDMETHODCALLTYPE net_shim_add_ref(NetShim *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}

static ULONG STDMETHODCALLTYPE net_shim_release(NetShim *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        if (self->inner) {
            self->inner->lpVtbl->Release(self->inner);
        }
        net_free(self);
    }
    return (ULONG)left;
}

/* IConnectionProfile::GetNetworkConnectivityLevel */
static HRESULT STDMETHODCALLTYPE net_profile_level(NetShim *self, int *level) {
    typedef HRESULT(STDMETHODCALLTYPE * Call)(NetCom *, int *);
    HRESULT result = S_OK;
    int original = -1;
    if (self->inner) {
        Call call = (Call)(uintptr_t)net_shim_slot(self, 7);
        result = call(self->inner, level);
        original = level ? *level : -1;
    }
    if (level) {
        *level = NET_XBOX_LIVE_ACCESS;
    }
    if (!InterlockedExchange(&net_connectivity_logged, 1)) {
        Buffer line = {0};
        if (buffer_append_text(&line, "WINRT: connectivity level ") &&
            buffer_append_number(&line, (unsigned)original) &&
            buffer_append_text(&line, " (") && buffer_append_hresult(&line, result) &&
            buffer_append_text(&line, ") reported as 4, XboxLiveAccess.\r\n")) {
            log_append((const char *)line.data);
        }
        buffer_free(&line);
    }
    return level ? S_OK : result;
}

static NetShim *net_shim_wrap(NetCom *inner, void *const *vtable, const unsigned char *iid);

/* INetworkInformationStatics::GetInternetConnectionProfile */
static HRESULT STDMETHODCALLTYPE net_netinfo_profile(NetShim *self, NetCom **out) {
    typedef HRESULT(STDMETHODCALLTYPE * Call)(NetCom *, NetCom **);
    Call call = (Call)(uintptr_t)net_shim_slot(self, 7);
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    HRESULT result = call(self->inner, out);
    static volatile LONG logged;
    if (!InterlockedExchange(&logged, 1)) {
        Buffer line = {0};
        if (buffer_append_text(&line, "WINRT: Azimuth's GetInternetConnectionProfile -> ") &&
            buffer_append_hresult(&line, result) &&
            buffer_append_text(&line, *out ? ", a profile.\r\n" : ", no profile; using ours.\r\n")) {
            log_append((const char *)line.data);
        }
        buffer_free(&line);
    }
    if (FAILED(result) || !*out) {
        if (*out) {
            (*out)->lpVtbl->Release(*out);
        }
        *out = 0;
        result = S_OK;
    }
    {
        static void *const profile_vtable[15] = {
            (void *)(uintptr_t)net_shim_query,     (void *)(uintptr_t)net_shim_add_ref,
            (void *)(uintptr_t)net_shim_release,   (void *)(uintptr_t)net_shim_forward3,
            (void *)(uintptr_t)net_shim_forward4,  (void *)(uintptr_t)net_shim_forward5,
            (void *)(uintptr_t)net_shim_forward6,  (void *)(uintptr_t)net_profile_level,
            (void *)(uintptr_t)net_shim_forward8,  (void *)(uintptr_t)net_shim_forward9,
            (void *)(uintptr_t)net_shim_forward10, (void *)(uintptr_t)net_shim_forward11,
            (void *)(uintptr_t)net_shim_forward12, (void *)(uintptr_t)net_shim_forward13,
            (void *)(uintptr_t)net_shim_forward14};
        NetShim *shim = net_shim_wrap(*out, profile_vtable, net_iid_connection_profile);
        if (shim) {
            *out = (NetCom *)(void *)shim;
        }
    }
    return result;
}

static const void *const net_netinfo_vtable[14] = {
    (void *)(uintptr_t)net_shim_query,     (void *)(uintptr_t)net_shim_add_ref,
    (void *)(uintptr_t)net_shim_release,   (void *)(uintptr_t)net_shim_forward3,
    (void *)(uintptr_t)net_shim_forward4,  (void *)(uintptr_t)net_shim_forward5,
    (void *)(uintptr_t)net_shim_forward6,  (void *)(uintptr_t)net_netinfo_profile,
    (void *)(uintptr_t)net_shim_forward8,  (void *)(uintptr_t)net_shim_forward9,
    (void *)(uintptr_t)net_shim_forward10, (void *)(uintptr_t)net_shim_forward11,
    (void *)(uintptr_t)net_shim_forward12, (void *)(uintptr_t)net_shim_forward13};

/* Takes over the caller's reference to `inner`; on failure the caller keeps it. */
static NetShim *net_shim_wrap(NetCom *inner, void *const *vtable, const unsigned char *iid) {
    NetShim *shim = net_alloc(sizeof(*shim));
    if (shim) {
        shim->lpVtbl = vtable;
        shim->references = 1;
        shim->inner = inner;
        shim->iid = iid;
    }
    return shim;
}

static void net_netinfo_filter(void **statics) {
    NetShim *shim = net_shim_wrap((NetCom *)*statics, (void *const *)net_netinfo_vtable,
                                  net_iid_network_information_statics);
    if (shim) {
        *statics = shim;
    }
}

static int net_wide_equal(const wchar_t *left, const wchar_t *right) {
    while (*left && *left == *right) {
        ++left;
        ++right;
    }
    return *left == *right;
}

static HRESULT net_get_activation_factory(const wchar_t *name, const void *iid, void **out) {
    if (!name || !out) {
        return E_POINTER;
    }
    unsigned provider = 0;
    HRESULT result = net_activate_raw(name, iid, out, &provider);
    if (SUCCEEDED(result) && *out && iid && memory_equal(iid, net_iid_activation_factory, 16) &&
        net_wide_equal(name, L"Windows.Storage.Streams.DataWriter")) {
        net_writer_filter(out);
    }
    if (SUCCEEDED(result) && *out && iid && memory_equal(iid, net_iid_data_reader_statics, 16) &&
        net_wide_equal(name, L"Windows.Storage.Streams.DataReader")) {
        net_reader_filter(out);
    }
    if (SUCCEEDED(result) && *out && iid &&
        memory_equal(iid, net_iid_network_information_statics, 16) &&
        net_wide_equal(name, L"Windows.Networking.Connectivity.NetworkInformation")) {
        net_netinfo_filter(out);
    }
    if (SUCCEEDED(result) && *out) {
        azimuth_freeroam_filter(name, iid, out);
    }
    if (net_trace_first(name, result)) {
        Buffer line = {0};
        if (buffer_append_text(&line, "WINRT: activate ") &&
            buffer_append_json_wide(&line, name, wide_length(name)) &&
            buffer_append_text(&line, " provider=") &&
            buffer_append_number(&line, provider) && buffer_append_text(&line, " -> ")) {
            buffer_append_hresult(&line, result);
        }
        net_trace_line(&line);
    }
    /* Azimuth terminates the process here on failure; the log line above is
       the point of this build, so hand the failure back instead. */
    return result;
}

static void net_trace_hstring(Buffer *line, void *value) {
    struct {
        unsigned magic, length;
        unsigned long long reserved;
        const wchar_t *text;
    } header;
    static wchar_t text[256];
    if (!value) {
        buffer_append_text(line, "\"\"");
        return;
    }
    if (!memory_read(value, &header, sizeof(header)) || header.magic != 0x32485346u) {
        buffer_append_text(line, "<not an HSTRING>");
        return;
    }
    unsigned length = header.length < 255 ? header.length : 255;
    AcquireSRWLockExclusive(&net_trace_lock);
    if (length && !memory_read(header.text, text, length * sizeof(wchar_t))) {
        length = 0;
    }
    text[length] = 0;
    buffer_append_json_wide(line, text, length);
    ReleaseSRWLockExclusive(&net_trace_lock);
}

static void net_trace_token(unsigned slot, void *method, void *url) {
    Buffer line = {0};
    if (buffer_append_text(&line, "XBL: User.GetTokenAndSignature slot ") &&
        buffer_append_number(&line, slot) && buffer_append_text(&line, " ")) {
        net_trace_hstring(&line, method);
        buffer_append_text(&line, " ");
        net_trace_hstring(&line, url);
    }
    net_trace_line(&line);
}

/* ------------------------------------------------------------- fake token */
/* Azimuth's User.GetTokenAndSignature* always fails (ERROR_NETWORK_UNREACHABLE),
   so XSAPI and the game never build a request. Instead, hand back a completed
   IAsyncOperation<GetTokenAndSignatureResult>. The result's two string getters
   return distinguishable values so a capture shows which one is the token. */

typedef HRESULT (*NetCreateStringFn)(const wchar_t *, unsigned, void **);

static const unsigned char net_iid_async_info[16] = {0x36, 0, 0, 0, 0, 0, 0, 0,
                                                     0xc0, 0, 0, 0, 0, 0, 0, 0x46};
/* IAsyncOperation<Windows.Xbox.System.GetTokenAndSignatureResult>, as Azimuth uses it. */
static const unsigned char net_iid_token_operation[16] = {0xb1, 0xe9, 0xc6, 0xe6, 0x3d, 0x36,
                                                          0xfe, 0x54, 0x80, 0x8b, 0x89, 0xda,
                                                          0x6b, 0xcf, 0x78, 0xbf};
static const unsigned char net_iid_no_marshal[16] = {0x1b, 0x69, 0xc8, 0xec, 0xdb, 0xc1, 0xc0, 0x4d,
                                                     0x85, 0x5e, 0x65, 0xf6, 0xc5, 0x51, 0xaf, 0x49};
static volatile LONG net_token_ids, net_logged_result_iid, net_logged_operation_iid;

static void net_log_iid_once(volatile LONG *flag, const char *prefix, const void *iid) {
    if (!InterlockedCompareExchange(flag, 1, 0)) {
        net_log_guid(prefix, (const unsigned char *)iid);
    }
}

static int net_is_classic_com_iid(const unsigned char *iid) {
    static const unsigned char tail[8] = {0xc0, 0, 0, 0, 0, 0, 0, 0x46};
    return memory_equal(iid + 8, tail, 8) && iid[4] == 0 && iid[5] == 0 && iid[6] == 0 &&
           iid[7] == 0;
}

/* -- result object */
typedef struct NetTokenResult NetTokenResult;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetTokenResult *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetTokenResult *);
    ULONG(STDMETHODCALLTYPE *Release)(NetTokenResult *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(NetTokenResult *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(NetTokenResult *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(NetTokenResult *, int *);
    HRESULT(STDMETHODCALLTYPE *Getter[6])(NetTokenResult *, void **);
} NetTokenResultVtbl;
struct NetTokenResult {
    const NetTokenResultVtbl *lpVtbl;
    volatile LONG references;
};

static HRESULT STDMETHODCALLTYPE net_result_query(NetTokenResult *self, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    const unsigned char *id = (const unsigned char *)iid;
    /* Classic COM interfaces (IMarshal, IWeakReferenceSource, ...) other than
       IUnknown, and INoMarshal, are refused. Anything else is taken to be the
       result's own interface, whose IID this build does not know. */
    if ((net_is_classic_com_iid(id) && !memory_equal(id, net_iid_unknown, 16)) ||
        memory_equal(id, net_iid_no_marshal, 16)) {
        return E_NOINTERFACE;
    }
    if (!memory_equal(id, net_iid_unknown, 16) && !memory_equal(id, net_iid_inspectable, 16) &&
        !memory_equal(id, net_iid_agile, 16)) {
        net_log_iid_once(&net_logged_result_iid, "XBL: token result asked for interface ", iid);
    }
    InterlockedIncrement(&self->references);
    *out = self;
    return S_OK;
}
static ULONG STDMETHODCALLTYPE net_result_add_ref(NetTokenResult *self) {
    return (ULONG)InterlockedIncrement(&self->references);
}
static ULONG STDMETHODCALLTYPE net_result_release(NetTokenResult *self) {
    LONG left = InterlockedDecrement(&self->references);
    if (!left) {
        net_free(self);
    }
    return (ULONG)left;
}
static HRESULT STDMETHODCALLTYPE net_inspectable_iids(void *self, ULONG *count, void **iids) {
    (void)self;
    if (count) {
        *count = 0;
    }
    if (iids) {
        *iids = 0;
    }
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_inspectable_name(void *self, void **name) {
    (void)self;
    if (name) {
        *name = 0;
    }
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_inspectable_trust(void *self, int *level) {
    (void)self;
    if (level) {
        *level = 0;
    }
    return S_OK;
}
static HRESULT net_make_hstring(const wchar_t *text, void **out) {
    if (!out) {
        return E_POINTER;
    }
    NetCreateStringFn create = (NetCreateStringFn)(uintptr_t)(net_azimuth_base + net_offsets->hstring_create);
    return create(text, (unsigned)wide_length(text), out);
}
HRESULT azimuth_net_make_hstring(const wchar_t *text, void **out) {
    return net_azimuth_base && net_offsets ? net_make_hstring(text, out) : E_FAIL;
}
static HRESULT STDMETHODCALLTYPE net_result_get_0(NetTokenResult *self, void **out) {
    (void)self;
    return net_make_hstring(L"fh2online-signature", out); /* Signature */
}
static HRESULT STDMETHODCALLTYPE net_result_get_1(NetTokenResult *self, void **out) {
    (void)self;
    return net_make_hstring(L"XBL3.0 x=1;fh2online-token", out); /* Token */
}
static HRESULT STDMETHODCALLTYPE net_result_unknown(NetTokenResult *self, void **out) {
    (void)self;
    (void)out;
    log_append("XBL: token result: a getter past the first two was called.\r\n");
    return E_NOTIMPL;
}
static const NetTokenResultVtbl net_result_vtable = {
    net_result_query,
    net_result_add_ref,
    net_result_release,
    (HRESULT(STDMETHODCALLTYPE *)(NetTokenResult *, ULONG *, void **))net_inspectable_iids,
    (HRESULT(STDMETHODCALLTYPE *)(NetTokenResult *, void **))net_inspectable_name,
    (HRESULT(STDMETHODCALLTYPE *)(NetTokenResult *, int *))net_inspectable_trust,
    {net_result_get_0, net_result_get_1, net_result_unknown, net_result_unknown, net_result_unknown,
     net_result_unknown}};

/* -- completed async operation: IAsyncOperation<T> at +0, IAsyncInfo at +8 */
typedef struct NetTokenOperation NetTokenOperation;
typedef struct NetCompletedHandler NetCompletedHandler;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(NetCompletedHandler *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(NetCompletedHandler *);
    ULONG(STDMETHODCALLTYPE *Release)(NetCompletedHandler *);
    HRESULT(STDMETHODCALLTYPE *Invoke)(NetCompletedHandler *, NetTokenOperation *, int);
} NetCompletedHandlerVtbl;
struct NetCompletedHandler {
    const NetCompletedHandlerVtbl *lpVtbl;
};
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(void *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(void *);
    ULONG(STDMETHODCALLTYPE *Release)(void *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(void *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(void *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(void *, int *);
    HRESULT(STDMETHODCALLTYPE *PutCompleted)(void *, NetCompletedHandler *);
    HRESULT(STDMETHODCALLTYPE *GetCompleted)(void *, NetCompletedHandler **);
    HRESULT(STDMETHODCALLTYPE *GetResults)(void *, NetTokenResult **);
} NetTokenOperationVtbl;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(void *, REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(void *);
    ULONG(STDMETHODCALLTYPE *Release)(void *);
    HRESULT(STDMETHODCALLTYPE *GetIids)(void *, ULONG *, void **);
    HRESULT(STDMETHODCALLTYPE *GetRuntimeClassName)(void *, void **);
    HRESULT(STDMETHODCALLTYPE *GetTrustLevel)(void *, int *);
    HRESULT(STDMETHODCALLTYPE *GetId)(void *, unsigned *);
    HRESULT(STDMETHODCALLTYPE *GetStatus)(void *, int *);
    HRESULT(STDMETHODCALLTYPE *GetErrorCode)(void *, HRESULT *);
    HRESULT(STDMETHODCALLTYPE *Cancel)(void *);
    HRESULT(STDMETHODCALLTYPE *Close)(void *);
} NetAsyncInfoVtbl;
struct NetTokenOperation {
    const NetTokenOperationVtbl *operation;
    const NetAsyncInfoVtbl *info;
    volatile LONG references, handler_set;
    unsigned id;
    NetTokenResult *result;
    NetCompletedHandler *handler;
};

static NetTokenOperation *net_op_from(void *self, int info) {
    return (NetTokenOperation *)((unsigned char *)self - (info ? sizeof(void *) : 0));
}
static HRESULT net_op_query(NetTokenOperation *op, REFIID iid, void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    if (memory_equal(iid, net_iid_unknown, 16) || memory_equal(iid, net_iid_inspectable, 16) ||
        memory_equal(iid, net_iid_agile, 16) || memory_equal(iid, net_iid_token_operation, 16)) {
        *out = &op->operation;
    } else if (memory_equal(iid, net_iid_async_info, 16)) {
        *out = &op->info;
    } else {
        net_log_iid_once(&net_logged_operation_iid, "XBL: token operation refused interface ", iid);
        return E_NOINTERFACE;
    }
    InterlockedIncrement(&op->references);
    return S_OK;
}
static ULONG net_op_add_ref(NetTokenOperation *op) {
    return (ULONG)InterlockedIncrement(&op->references);
}
static ULONG net_op_release(NetTokenOperation *op) {
    LONG left = InterlockedDecrement(&op->references);
    if (!left) {
        if (op->handler) {
            op->handler->lpVtbl->Release(op->handler);
        }
        op->result->lpVtbl->Release(op->result);
        net_free(op);
    }
    return (ULONG)left;
}
static HRESULT STDMETHODCALLTYPE net_opa_query(void *s, REFIID i, void **o) { return net_op_query(net_op_from(s, 0), i, o); }
static ULONG STDMETHODCALLTYPE net_opa_add_ref(void *s) { return net_op_add_ref(net_op_from(s, 0)); }
static ULONG STDMETHODCALLTYPE net_opa_release(void *s) { return net_op_release(net_op_from(s, 0)); }
static HRESULT STDMETHODCALLTYPE net_opi_query(void *s, REFIID i, void **o) { return net_op_query(net_op_from(s, 1), i, o); }
static ULONG STDMETHODCALLTYPE net_opi_add_ref(void *s) { return net_op_add_ref(net_op_from(s, 1)); }
static ULONG STDMETHODCALLTYPE net_opi_release(void *s) { return net_op_release(net_op_from(s, 1)); }

static HRESULT STDMETHODCALLTYPE net_op_put_completed(void *self, NetCompletedHandler *handler) {
    NetTokenOperation *op = net_op_from(self, 0);
    if (InterlockedCompareExchange(&op->handler_set, 1, 0)) {
        return (HRESULT)0x80000018L; /* E_ILLEGAL_DELEGATE_ASSIGNMENT */
    }
    if (handler) {
        handler->lpVtbl->AddRef(handler);
        op->handler = handler;
        /* Already complete: WinRT invokes the handler right away. */
        handler->lpVtbl->Invoke(handler, (NetTokenOperation *)&op->operation, 1);
    }
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_op_get_completed(void *self, NetCompletedHandler **handler) {
    NetTokenOperation *op = net_op_from(self, 0);
    if (!handler) {
        return E_POINTER;
    }
    *handler = op->handler;
    if (op->handler) {
        op->handler->lpVtbl->AddRef(op->handler);
    }
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_op_get_results(void *self, NetTokenResult **result) {
    NetTokenOperation *op = net_op_from(self, 0);
    if (!result) {
        return E_POINTER;
    }
    op->result->lpVtbl->AddRef(op->result);
    *result = op->result;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_op_get_id(void *self, unsigned *id) {
    if (!id) {
        return E_POINTER;
    }
    *id = net_op_from(self, 1)->id;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_op_get_status(void *self, int *status) {
    (void)self;
    if (!status) {
        return E_POINTER;
    }
    *status = 1; /* AsyncStatus::Completed */
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_op_get_error(void *self, HRESULT *error) {
    (void)self;
    if (!error) {
        return E_POINTER;
    }
    *error = S_OK;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE net_op_nothing(void *self) {
    (void)self;
    return S_OK;
}
static const NetTokenOperationVtbl net_op_vtable = {
    net_opa_query, net_opa_add_ref, net_opa_release, net_inspectable_iids, net_inspectable_name,
    net_inspectable_trust, net_op_put_completed, net_op_get_completed, net_op_get_results};
static const NetAsyncInfoVtbl net_op_info_vtable = {
    net_opi_query, net_opi_add_ref, net_opi_release, net_inspectable_iids, net_inspectable_name,
    net_inspectable_trust, net_op_get_id, net_op_get_status, net_op_get_error, net_op_nothing,
    net_op_nothing};

static HRESULT net_issue_token(void **out) {
    if (!out) {
        return E_POINTER;
    }
    *out = 0;
    NetTokenResult *result = net_alloc(sizeof(*result));
    NetTokenOperation *op = net_alloc(sizeof(*op));
    if (!result || !op) {
        net_free(result);
        net_free(op);
        return E_OUTOFMEMORY;
    }
    result->lpVtbl = &net_result_vtable;
    result->references = 1;
    op->operation = &net_op_vtable;
    op->info = &net_op_info_vtable;
    op->references = 1;
    op->id = (unsigned)InterlockedIncrement(&net_token_ids);
    op->result = result;
    *out = &op->operation;
    return S_OK;
}

/* Out-parameter position per slot: (method, url, headers, out),
   (method, url, headers, body length, body, out), (method, url, headers, body, out). */
#define NET_TOKEN_THUNK(n, out)                                                                 \
    static HRESULT net_token_##n(void *a, void *b, void *c, void *d, void *e, void *f, void *g, \
                                 void *h) {                                                     \
        (void)a, (void)d, (void)e, (void)f, (void)g, (void)h;                                   \
        net_trace_token(n, b, c);                                                               \
        return net_issue_token((void **)(out));                                                 \
    }
NET_TOKEN_THUNK(0, e)
NET_TOKEN_THUNK(1, g)
NET_TOKEN_THUNK(2, f)
NET_TOKEN_THUNK(3, e)
NET_TOKEN_THUNK(4, g)
NET_TOKEN_THUNK(5, f)
static const NetTokenFn net_token_thunks[6] = {net_token_0, net_token_1, net_token_2,
                                               net_token_3, net_token_4, net_token_5};

static int net_write_jump(unsigned char *at, const void *target, const unsigned char *hash,
                          unsigned hashed) {
    unsigned char current[32], digest[32];
    unsigned char jump[NET_HOOK_JUMP_BYTES] = {0xff, 0x25, 0x00, 0x00, 0x00, 0x00};
    uintptr_t value = (uintptr_t)target;
    for (unsigned i = 0; i < 8; ++i) {
        jump[6 + i] = (unsigned char)(value >> (8 * i));
    }
    if (hashed > sizeof(current) || !memory_read(at, current, hashed)) {
        return 0;
    }
    if (memory_equal(current, jump, sizeof(jump))) {
        return 2;
    }
    Sha256Context context;
    sha256_initialize(&context);
    sha256_update(&context, current, hashed);
    sha256_finalize(&context, digest);
    if (!memory_equal(digest, hash, sizeof(digest))) {
        return 0;
    }
    DWORD old_protection, ignored;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        return -1;
    }
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    VirtualProtect(at, sizeof(jump), old_protection, &ignored);
    return 1;
}

static void net_install_trace(void) {
    int activation = net_write_jump(net_azimuth_base + net_offsets->net_activation,
                                    (const void *)net_get_activation_factory,
                                    net_offsets->net_activation_hash, 0x20);
    log_append(activation > 0 ? "WINRT: activation trace installed.\r\n"
                              : "WINRT: activation trace NOT installed (unexpected bytes).\r\n");
    unsigned patched = 0;
    for (unsigned i = 0; i < ARRAY_COUNT(net_offsets->net_token_slots); ++i) {
        void **slot = (void **)(net_azimuth_base + net_offsets->net_token_slots[i]);
        void *expected = net_azimuth_base + net_offsets->net_token_targets[i], *current = 0;
        DWORD old_protection, ignored;
        if (!memory_read(slot, &current, sizeof(current)) || current != expected ||
            !VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) {
            continue;
        }
        net_token_original[i] = (NetTokenFn)(uintptr_t)expected;
        *slot = (void *)(uintptr_t)net_token_thunks[i];
        VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
        ++patched;
    }
    log_append(patched == ARRAY_COUNT(net_offsets->net_token_slots)
                   ? "XBL: fake token installed on all 6 User slots.\r\n"
                   : "XBL: fake token installed on only some User slots.\r\n");
}

/* ------------------------------------------------------- economy probe */
/* Diagnostic: the offline defaults of ForzaEconomyModule (CreditsPerKm, XPPerKm*,
   TombolaPrizeScale...), needed to answer User.GetEconomySettings without changing
   the game's economy. The property getters are tiny (CreditsPerKm at game+0x3ee230
   is "mov eax,[rdx+0x288]; ret"), so the getter is redirected to this function with
   the same signature: it logs the object it is asked about (first few distinct
   ones, since identical getters are folded across classes) and returns the same
   value the original would. */

#define NET_ECONOMY_GETTER_RVA 0x3ee230u
#define NET_ECONOMY_MAX_LOGGED 4
static const unsigned char net_economy_getter_bytes[16] = {0x8b, 0x82, 0x88, 0x02, 0x00, 0x00,
                                                          0xc3, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc,
                                                          0xcc, 0xcc, 0xcc, 0xcc};
static unsigned char *volatile net_economy_seen[NET_ECONOMY_MAX_LOGGED];
static volatile LONG net_economy_count;

static int buffer_append_hex(Buffer *out, unsigned long long value, unsigned digits) {
    static const char hex[] = "0123456789abcdef";
    char text[16];
    if (digits > sizeof(text)) {
        digits = sizeof(text);
    }
    for (unsigned i = 0; i < digits; ++i) {
        text[i] = hex[(value >> (4 * (digits - 1 - i))) & 15];
    }
    return buffer_append(out, text, digits);
}

/* Floats as fixed point with 4 decimals (no CRT printf here). */
static int buffer_append_float(Buffer *out, float value) {
    if (value != value) {
        return buffer_append_text(out, "nan");
    }
    if (value < 0) {
        if (!buffer_append(out, "-", 1)) {
            return 0;
        }
        value = -value;
    }
    if (value > 1e15f) {
        return buffer_append_text(out, "huge");
    }
    /* via signed: float -> unsigned 64-bit would need libgcc's __fixunssfdi */
    unsigned long long scaled = (unsigned long long)(long long)(value * 10000.0f + 0.5f);
    char frac[5];
    unsigned long long f = scaled % 10000;
    for (int i = 3; i >= 0; --i) {
        frac[i] = (char)('0' + f % 10);
        f /= 10;
    }
    frac[4] = 0;
    return buffer_append_number(out, scaled / 10000) && buffer_append(out, ".", 1) &&
           buffer_append(out, frac, 4);
}

static void net_economy_log(unsigned char *object) {
    static const struct {
        const char *name;
        unsigned offset;
        char kind; /* i int, f float, b byte */
    } fields[] = {{"RetrievalAttemptInterval", 0x120, 'f'}, {"RetrievalMaxRetryTime", 0x140, 'f'},
                  {"DownloadFailedBackoffRate", 0x160, 'f'}, {"UseServerValues", 0x1e8, 'b'},
                  {"CreditDestinationBonus", 0x208, 'i'},  {"CreditDestinationFalloff", 0x228, 'i'},
                  {"XPPerKmOffline", 0x248, 'i'},           {"XPPerKmOnline", 0x268, 'i'},
                  {"CreditsPerKm", 0x288, 'i'},             {"MinimumFinishlineXP", 0x2a8, 'i'},
                  {"SubmitXpFrequency", 0x2c8, 'i'},        {"TombolaPrizeScale", 0x2e8, 'f'},
                  {"HorizonPromoBonusScale", 0x308, 'f'},   {"TokensEnabled", 0x328, 'b'},
                  {"XboxMusicEnabled", 0x348, 'b'},          {"DownloadedEconomyValues", 0x351, 'b'}};
    Buffer line = {0};
    int ok = buffer_append_text(&line, "ECONOMY: CreditsPerKm getter called for object 0x") &&
             buffer_append_hex(&line, (unsigned long long)(uintptr_t)object, 16) &&
             buffer_append_text(&line, "\r\n");
    for (unsigned i = 0; ok && i < ARRAY_COUNT(fields); ++i) {
        unsigned char raw[4] = {0};
        if (!memory_read(object + fields[i].offset, raw, sizeof(raw))) {
            ok = buffer_append_text(&line, "ECONOMY:   <unreadable>\r\n");
            break;
        }
        ok = buffer_append_text(&line, "ECONOMY:   ") && buffer_append_text(&line, fields[i].name) &&
             buffer_append_text(&line, " = ");
        if (!ok) {
            break;
        }
        if (fields[i].kind == 'i') {
            int value;
            memory_copy(&value, raw, sizeof(value));
            if (value < 0) {
                ok = buffer_append(&line, "-", 1) && buffer_append_number(&line, (unsigned)-value);
            } else {
                ok = buffer_append_number(&line, (unsigned)value);
            }
        } else if (fields[i].kind == 'f') {
            float value;
            memory_copy(&value, raw, sizeof(value));
            ok = buffer_append_float(&line, value);
        } else {
            ok = buffer_append_number(&line, raw[0]);
        }
        ok = ok && buffer_append_text(&line, "\r\n");
    }
    /* Raw dump of the property block, to check the offsets above. */
    for (unsigned row = 0x100; ok && row < 0x360; row += 0x20) {
        unsigned long long q[4] = {0, 0, 0, 0};
        if (!memory_read(object + row, q, sizeof(q))) {
            break;
        }
        ok = buffer_append_text(&line, "ECONOMY:   +") && buffer_append_hex(&line, row, 3);
        for (unsigned k = 0; ok && k < 4; ++k) {
            ok = buffer_append(&line, " ", 1) && buffer_append_hex(&line, q[k], 16);
        }
        ok = ok && buffer_append_text(&line, "\r\n");
    }
    if (ok) {
        log_append((const char *)line.data);
    }
    buffer_free(&line);
}

/* Same signature as the getter: (rcx = property descriptor, rdx = object). */
static int net_economy_getter(void *descriptor, unsigned char *object) {
    (void)descriptor;
    if (net_economy_count < NET_ECONOMY_MAX_LOGGED) {
        int known = 0;
        for (LONG i = 0; i < NET_ECONOMY_MAX_LOGGED; ++i) {
            if (net_economy_seen[i] == object) {
                known = 1;
                break;
            }
        }
        if (!known) {
            LONG slot = InterlockedIncrement(&net_economy_count) - 1;
            if (slot < NET_ECONOMY_MAX_LOGGED) {
                net_economy_seen[slot] = object;
                net_economy_log(object);
            }
        }
    }
    int value;
    memory_copy(&value, object + 0x288, sizeof(value));
    return value;
}

/* Plan B: the getter above is never called (the game reads the fields directly).
   FUN_140561000 sends User.GetEconomySettings on every retry and gets the module in
   rcx, so its entry jumps to a trampoline that logs the object once, runs the 16
   original bytes (push rbx; sub rsp,20h; mov rbx,rcx; add rcx,168h - nothing
   RIP-relative) and jumps back to game+0x561010. */
#define NET_ECONOMY_REQUEST_RVA 0x561000u
static const unsigned char net_economy_request_bytes[16] = {0x40, 0x53, 0x48, 0x83, 0xec, 0x20,
                                                           0x48, 0x8b, 0xd9, 0x48, 0x81, 0xc1,
                                                           0x68, 0x01, 0x00, 0x00};
static volatile LONG net_economy_dumped;

static void net_economy_dump(unsigned char *module) {
    if (InterlockedCompareExchange(&net_economy_dumped, 1, 0)) {
        return;
    }
    Buffer line = {0};
    int ok = buffer_append_text(&line, "ECONOMY: GetEconomySettings request from module 0x") &&
             buffer_append_hex(&line, (unsigned long long)(uintptr_t)module, 16) &&
             buffer_append_text(&line, " (raw +000..+400, qwords)\r\n");
    for (unsigned row = 0; ok && row < 0x400; row += 0x20) {
        unsigned long long q[4] = {0, 0, 0, 0};
        if (!memory_read(module + row, q, sizeof(q))) {
            ok = buffer_append_text(&line, "ECONOMY:   <unreadable>\r\n");
            break;
        }
        ok = buffer_append_text(&line, "ECONOMY:   +") && buffer_append_hex(&line, row, 3);
        for (unsigned k = 0; ok && k < 4; ++k) {
            ok = buffer_append(&line, " ", 1) && buffer_append_hex(&line, q[k], 16);
        }
        ok = ok && buffer_append_text(&line, "\r\n");
    }
    if (ok) {
        log_append((const char *)line.data);
    }
    buffer_free(&line);
}

static void net_install_economy_request_probe(unsigned char *game_image) {
    unsigned char *at = game_image + NET_ECONOMY_REQUEST_RVA;
    unsigned char current[16];
    if (!memory_read(at, current, sizeof(current)) ||
        !memory_equal(current, net_economy_request_bytes, sizeof(current))) {
        log_append("ECONOMY: request probe not installed (unexpected bytes).\r\n");
        return;
    }
    unsigned char *tramp = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!tramp) {
        log_append("ECONOMY: request probe not installed (no trampoline memory).\r\n");
        return;
    }
    /* entry rsp = 8 mod 16; 4 pushes keep it; sub 28h aligns it with shadow space */
    static const unsigned char save[] = {0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x48, 0x83, 0xec, 0x28};
    static const unsigned char restore[] = {0x48, 0x83, 0xc4, 0x28, 0x41, 0x59, 0x41, 0x58, 0x5a, 0x59};
    unsigned n = 0;
    uintptr_t dump = (uintptr_t)net_economy_dump, back = (uintptr_t)(at + sizeof(current));
    memory_copy(tramp + n, save, sizeof(save));
    n += sizeof(save);
    tramp[n++] = 0x48; /* mov rax, imm64 ; call rax */
    tramp[n++] = 0xb8;
    memory_copy(tramp + n, &dump, 8);
    n += 8;
    tramp[n++] = 0xff;
    tramp[n++] = 0xd0;
    memory_copy(tramp + n, restore, sizeof(restore));
    n += sizeof(restore);
    memory_copy(tramp + n, current, sizeof(current)); /* the original 16 bytes */
    n += sizeof(current);
    tramp[n++] = 0x48; /* mov rax, imm64 ; jmp rax */
    tramp[n++] = 0xb8;
    memory_copy(tramp + n, &back, 8);
    n += 8;
    tramp[n++] = 0xff;
    tramp[n++] = 0xe0;
    FlushInstructionCache(GetCurrentProcess(), tramp, n);

    unsigned char jump[16] = {0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xe0, 0x90, 0x90, 0x90, 0x90};
    uintptr_t target = (uintptr_t)tramp;
    memory_copy(jump + 2, &target, sizeof(target));
    DWORD old_protection, ignored;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        log_append("ECONOMY: request probe not installed (VirtualProtect failed).\r\n");
        return;
    }
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    VirtualProtect(at, sizeof(jump), old_protection, &ignored);
    log_append("ECONOMY: request probe installed; the module is dumped on the next GetEconomySettings.\r\n");
}

void azimuth_apply_economy_probe(unsigned char *game_image) {
    net_install_economy_request_probe(game_image);
    unsigned char *at = game_image + NET_ECONOMY_GETTER_RVA;
    unsigned char current[16];
    if (!memory_read(at, current, sizeof(current)) ||
        !memory_equal(current, net_economy_getter_bytes, sizeof(current))) {
        log_append("ECONOMY: probe not installed (unexpected getter bytes).\r\n");
        return;
    }
    /* mov rax, imm64 ; jmp rax  (12 bytes, inside the 16 the getter + padding own) */
    unsigned char jump[12] = {0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xe0};
    uintptr_t target = (uintptr_t)net_economy_getter;
    memory_copy(jump + 2, &target, sizeof(target));
    DWORD old_protection, ignored;
    if (!VirtualProtect(at, sizeof(jump), PAGE_EXECUTE_READWRITE, &old_protection)) {
        log_append("ECONOMY: probe not installed (VirtualProtect failed).\r\n");
        return;
    }
    memory_copy(at, jump, sizeof(jump));
    FlushInstructionCache(GetCurrentProcess(), at, sizeof(jump));
    VirtualProtect(at, sizeof(jump), old_protection, &ignored);
    log_append("ECONOMY: probe installed on the CreditsPerKm getter; values are logged on first use.\r\n");
}

/* Game DVR clips: at a notable race moment (FUN_140562160, from nine places) the game records
   a clip in a PPL task (FUN_14054e560 -> lambda FUN_140542b70) that activates
   Windows.Xbox.Media.Capture.ApplicationClipCapture. Azimuth has no such class (0x80040154),
   the task throws, nothing observes it and the runtime ends the game (0xc0000409 "an exception
   a PPL task held that nothing observed"). The recorder returns at once instead. */
#define NET_CLIP_RECORD_RVA 0x54e560u
static const unsigned char net_clip_record_bytes[16] = {0x4c, 0x89, 0x4c, 0x24, 0x20, 0x48, 0x89, 0x54,
                                                        0x24, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41};

void azimuth_apply_no_game_clips(unsigned char *game_image) {
    unsigned char *at = game_image + NET_CLIP_RECORD_RVA;
    unsigned char current[16];
    if (!memory_read(at, current, sizeof(current)) || !memory_equal(current, net_clip_record_bytes, sizeof(current))) {
        log_append("CLIPS: recorder not as expected; left as is.\r\n");
        return;
    }
    DWORD old_protection, ignored;
    if (!VirtualProtect(at, 1, PAGE_EXECUTE_READWRITE, &old_protection)) {
        return;
    }
    *at = 0xc3; /* ret */
    FlushInstructionCache(GetCurrentProcess(), at, 1);
    VirtualProtect(at, 1, old_protection, &ignored);
    log_append("CLIPS: game clip recording off (no ApplicationClipCapture under Azimuth).\r\n");
}

void azimuth_apply_network(const wchar_t *directory) {
    net_read_settings(directory);
    if (net_mode == NET_MODE_OFF) {
        log_append("NETWORK: off (Infinity.cfg [hook] NetMode=Off; Proxy or Capture turns it on).\r\n");
        return;
    }
    net_azimuth_base = (unsigned char *)GetModuleHandleW(NULL);
    int result = net_install_hook(net_azimuth_base);
    if (result > 0) {
        log_append("NETWORK: Azimuth ");
        log_append(net_offsets->name);
        log_append(".\r\n");
        net_install_trace();
        log_append(net_mode == NET_MODE_PROXY
                       ? "NETWORK: HTTP hook installed; requests are captured to "
                         "infinity-network.jsonl and forwarded to the [hook] Server.\r\n"
                       : "NETWORK: HTTP hook installed; requests are captured to "
                         "infinity-network.jsonl and answered offline, as Azimuth does.\r\n");
    } else if (result < 0) {
        net_mode = NET_MODE_OFF;
        log_append("NETWORK: HTTP hook could not be written; network layer disabled.\r\n");
    } else {
        net_mode = NET_MODE_OFF;
        log_append("NETWORK: skipped; FH2Online supports Azimuth 0.9.7, 0.9.9 and 0.9.10, not this build "
                   "(the game plays offline).\r\n");
    }
}
