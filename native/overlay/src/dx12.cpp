// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the D3D12 hooks and the per-frame draw.
//
// The vtables come from a dummy device, queue and swapchain on a hidden window; MinHook then
// hooks IDXGISwapChain::Present / Present1 / ResizeBuffers and the factory's four swapchain
// creators, which name the queue each swapchain presents on. In Present the overlay records one
// command list into the current back buffer and submits it on that same queue before the game's
// present. Never another queue: Azimuth 1.0.3 renders on a direct queue and presents on a second
// one, and a back-buffer write from the render queue removed the device on Windows (2026-10-28).
// A swapchain whose queue is unknown gets no overlay. A fence per back buffer keeps an allocator from being
// reset while the GPU still reads it. Any structured exception in the draw sends the overlay to
// passthrough (main.cpp): every hook is disabled and the game's calls run untouched.
//
// The avatar (CR-L40 §1.3, 0x1A) is the one texture besides ImGui's font: the pipe thread leaves its
// pixels here, the render thread creates a texture and an upload buffer (never a wait), records the
// copy into that frame's own command list ahead of the draw, and retires the previous pair once the
// fence passes the last frame that read it. Two SRV slots after the font's alternate, and a new
// avatar waits until the previous one is retired, so a slot is never rewritten while the GPU reads it.
// The pixels stay in memory: a renderer initialised again uploads them again.
#include "overlay.h"

#include <MinHook.h>
#include <d3d12.h>
#include <dxgi1_4.h>

#include <atomic>
#include <chrono>
#include <cstring>
#include <mutex>
#include <vector>

#include "imgui.h"
#include "backends/imgui_impl_dx12.h"
#include "backends/imgui_impl_win32.h"

#pragma comment(lib, "d3d12.lib")
#pragma comment(lib, "dxgi.lib")

namespace fi {

namespace {

using PresentFn = HRESULT(STDMETHODCALLTYPE *)(IDXGISwapChain3 *, UINT, UINT);
using Present1Fn = HRESULT(STDMETHODCALLTYPE *)(IDXGISwapChain3 *, UINT, UINT, const DXGI_PRESENT_PARAMETERS *);
using ResizeFn = HRESULT(STDMETHODCALLTYPE *)(IDXGISwapChain3 *, UINT, UINT, UINT, DXGI_FORMAT, UINT);
using CreateSwapChainFn = HRESULT(STDMETHODCALLTYPE *)(IDXGIFactory2 *, IUnknown *, DXGI_SWAP_CHAIN_DESC *, IDXGISwapChain **);
using ForHwndFn = HRESULT(STDMETHODCALLTYPE *)(IDXGIFactory2 *, IUnknown *, HWND, const DXGI_SWAP_CHAIN_DESC1 *,
                                               const DXGI_SWAP_CHAIN_FULLSCREEN_DESC *, IDXGIOutput *, IDXGISwapChain1 **);
using ForCoreWindowFn = HRESULT(STDMETHODCALLTYPE *)(IDXGIFactory2 *, IUnknown *, IUnknown *, const DXGI_SWAP_CHAIN_DESC1 *,
                                                     IDXGIOutput *, IDXGISwapChain1 **);
using ForCompositionFn = HRESULT(STDMETHODCALLTYPE *)(IDXGIFactory2 *, IUnknown *, const DXGI_SWAP_CHAIN_DESC1 *, IDXGIOutput *,
                                                      IDXGISwapChain1 **);

PresentFn g_present = nullptr;
Present1Fn g_present1 = nullptr;
ResizeFn g_resize = nullptr;
CreateSwapChainFn g_create = nullptr;
ForHwndFn g_for_hwnd = nullptr;
ForCoreWindowFn g_for_core = nullptr;
ForCompositionFn g_for_composition = nullptr;

// Each swapchain created since the hooks went in, with the queue it presents on (one reference
// held on the queue; a reused swapchain address finds its newest entry).
struct ChainQueue {
    IUnknown *chain = nullptr; // identity only, never called
    ID3D12CommandQueue *queue = nullptr;
};
SRWLOCK g_chains_lock = SRWLOCK_INIT;
std::vector<ChainQueue> g_chains;
constexpr size_t kMaxChains = 16;

IUnknown *Identity(IUnknown *object) {
    IUnknown *identity = nullptr;
    if (!object || FAILED(object->QueryInterface(IID_PPV_ARGS(&identity)))) {
        return nullptr;
    }
    identity->Release(); // the object outlives this call: only the address is kept
    return identity;
}

void Remember(IUnknown *device, IUnknown *chain, HRESULT hr) {
    if (FAILED(hr) || !device || !chain) {
        return;
    }
    ID3D12CommandQueue *queue = nullptr;
    if (FAILED(device->QueryInterface(IID_PPV_ARGS(&queue)))) {
        return; // a D3D11 swapchain: not the game's
    }
    IUnknown *identity = Identity(chain);
    AcquireSRWLockExclusive(&g_chains_lock);
    if (g_chains.size() >= kMaxChains) {
        g_chains.front().queue->Release();
        g_chains.erase(g_chains.begin());
    }
    g_chains.push_back({identity, queue});
    ReleaseSRWLockExclusive(&g_chains_lock);
}

// The presenting queue of a swapchain, with a reference, or null.
ID3D12CommandQueue *QueueOf(IUnknown *chain) {
    IUnknown *identity = Identity(chain);
    ID3D12CommandQueue *queue = nullptr;
    AcquireSRWLockShared(&g_chains_lock);
    for (auto it = g_chains.rbegin(); it != g_chains.rend(); ++it) {
        if (it->chain == identity) {
            queue = it->queue;
            queue->AddRef();
            break;
        }
    }
    ReleaseSRWLockShared(&g_chains_lock);
    return queue;
}

struct FrameContext {
    ID3D12CommandAllocator *allocator = nullptr;
    ID3D12Resource *target = nullptr;
    D3D12_CPU_DESCRIPTOR_HANDLE rtv{};
    UINT64 fence_value = 0;
};

struct Renderer {
    bool ready = false;
    IDXGISwapChain3 *swapchain = nullptr; // the one we drew for (not owned)
    ID3D12CommandQueue *queue = nullptr;  // the queue that swapchain presents on (one reference)
    ID3D12Device *device = nullptr;
    ID3D12DescriptorHeap *rtv_heap = nullptr;
    ID3D12DescriptorHeap *srv_heap = nullptr;
    ID3D12GraphicsCommandList *list = nullptr;
    ID3D12Fence *fence = nullptr;
    HANDLE fence_event = nullptr;
    // Our own queue for one-off uploads (the font atlas): ImGui waits on it without a timeout, and
    // the game's queue may hold work that Azimuth releases only after Present returns (a wait on
    // it inside Present deadlocks the game, seen 2026-10-19).
    ID3D12CommandQueue *upload_queue = nullptr;
    UINT64 fence_next = 1;
    std::vector<FrameContext> frames;
    DXGI_FORMAT format = DXGI_FORMAT_R8G8B8A8_UNORM;
    HWND hwnd = nullptr;
    float scale = 1.0f;
    std::chrono::steady_clock::time_point last;
} g;

template <typename T> void SafeRelease(T *&p) {
    if (p) {
        p->Release();
        p = nullptr;
    }
}

void ReleaseTargets() {
    for (auto &f : g.frames) {
        SafeRelease(f.target);
    }
}

// ---------------------------------------------------------------- the avatar (CR-L40 §1.3)
constexpr UINT kSrvSlots = 3; // 0 ImGui's font, 1 and 2 the avatar's, alternating

std::mutex g_avatar_lock;
AvatarImage g_avatar_pending; // what the launcher sent last (side 0: none)
uint64_t g_avatar_serial = 0; // bumped by every SetAvatar

struct Retired {
    ID3D12Resource *resource;
    UINT64 fence; // released once the fence has passed it
};
struct AvatarGpu {
    uint64_t seen = 0; // the serial the GPU side holds
    ID3D12Resource *texture = nullptr;
    ID3D12Resource *upload = nullptr; // the copy's source until the copy is recorded
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprint{};
    bool copy = false; // still to be recorded into a frame's command list
    UINT slot = 1;
    D3D12_GPU_DESCRIPTOR_HANDLE gpu{};
    std::vector<Retired> retired;
} g_av;

void ReleaseRetired() {
    const UINT64 done = g.fence->GetCompletedValue();
    auto &list = g_av.retired;
    for (size_t i = 0; i < list.size();) {
        if (list[i].fence <= done) {
            list[i].resource->Release();
            list[i] = list.back();
            list.pop_back();
        } else {
            ++i;
        }
    }
}

// Every frame recorded from now on signals at least fence_next: the frames that may still read this did not pass it yet.
void Retire(ID3D12Resource *&resource) {
    if (resource) {
        g_av.retired.push_back({resource, g.fence_next});
        resource = nullptr;
    }
}

// Render thread, before the frame is drawn: a new avatar becomes a texture and an upload buffer (no wait).
void PrepareAvatar() {
    ReleaseRetired();
    AvatarImage image;
    {
        std::lock_guard<std::mutex> lock(g_avatar_lock);
        if (g_avatar_serial == g_av.seen || !g_av.retired.empty()) return; // nothing new, or the last one not retired yet
        g_av.seen = g_avatar_serial;
        image = g_avatar_pending;
    }
    Retire(g_av.texture);
    Retire(g_av.upload);
    g_av.copy = false;
    g_av.gpu = {};
    if (image.side == 0) return; // cleared: the initials
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    desc.Width = image.side;
    desc.Height = image.side;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM; // straight alpha, as ImGui blends
    desc.SampleDesc.Count = 1;
    desc.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    UINT64 upload_bytes = 0;
    g.device->GetCopyableFootprints(&desc, 0, 1, 0, &g_av.footprint, nullptr, nullptr, &upload_bytes);
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    if (FAILED(g.device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, nullptr,
                                                 IID_PPV_ARGS(&g_av.texture)))) {
        Log("avatar: texture not created");
        return;
    }
    D3D12_RESOURCE_DESC buffer{};
    buffer.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    buffer.Width = upload_bytes;
    buffer.Height = 1;
    buffer.DepthOrArraySize = 1;
    buffer.MipLevels = 1;
    buffer.SampleDesc.Count = 1;
    buffer.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    heap.Type = D3D12_HEAP_TYPE_UPLOAD;
    void *mapped = nullptr;
    const D3D12_RANGE none{0, 0};
    if (FAILED(g.device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer, D3D12_RESOURCE_STATE_GENERIC_READ, nullptr,
                                                 IID_PPV_ARGS(&g_av.upload))) ||
        FAILED(g_av.upload->Map(0, &none, &mapped))) {
        Log("avatar: upload buffer not created");
        Retire(g_av.texture);
        Retire(g_av.upload);
        return;
    }
    const size_t row = static_cast<size_t>(image.side) * 4;
    for (UINT y = 0; y < image.side; ++y) {
        memcpy(static_cast<uint8_t *>(mapped) + g_av.footprint.Offset + static_cast<size_t>(y) * g_av.footprint.Footprint.RowPitch,
               image.rgba.data() + y * row, row);
    }
    g_av.upload->Unmap(0, nullptr);
    g_av.slot = g_av.slot == 1 ? 2 : 1;
    const UINT step = g.device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    D3D12_CPU_DESCRIPTOR_HANDLE cpu = g.srv_heap->GetCPUDescriptorHandleForHeapStart();
    cpu.ptr += static_cast<SIZE_T>(g_av.slot) * step;
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = desc.Format;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MipLevels = 1;
    g.device->CreateShaderResourceView(g_av.texture, &srv, cpu);
    g_av.gpu = g.srv_heap->GetGPUDescriptorHandleForHeapStart();
    g_av.gpu.ptr += static_cast<UINT64>(g_av.slot) * step;
    g_av.copy = true;
}

// Into the frame's command list, before ImGui draws: the upload, then the texture readable by the pixel shader.
void RecordAvatarCopy(ID3D12GraphicsCommandList *list) {
    if (!g_av.copy) return;
    D3D12_TEXTURE_COPY_LOCATION dst{};
    dst.pResource = g_av.texture;
    dst.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    dst.SubresourceIndex = 0;
    D3D12_TEXTURE_COPY_LOCATION src{};
    src.pResource = g_av.upload;
    src.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    src.PlacedFootprint = g_av.footprint;
    list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition.pResource = g_av.texture;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_COPY_DEST;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
    list->ResourceBarrier(1, &barrier);
    g_av.copy = false;
    Retire(g_av.upload); // this frame's signal is fence_next: the buffer goes once it passed
}

void WaitIdle() {
    auto *queue = g.queue;
    if (!queue || !g.fence) {
        return;
    }
    const UINT64 value = g.fence_next++;
    if (SUCCEEDED(queue->Signal(g.fence, value)) && g.fence->GetCompletedValue() < value) {
        g.fence->SetEventOnCompletion(value, g.fence_event);
        WaitForSingleObject(g.fence_event, 2000);
    }
}

bool CreateTargets() {
    const UINT rtv_size = g.device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    D3D12_CPU_DESCRIPTOR_HANDLE handle = g.rtv_heap->GetCPUDescriptorHandleForHeapStart();
    for (UINT i = 0; i < g.frames.size(); ++i) {
        if (FAILED(g.swapchain->GetBuffer(i, IID_PPV_ARGS(&g.frames[i].target)))) {
            return false;
        }
        D3D12_RENDER_TARGET_VIEW_DESC desc{};
        // the game may use an _SRGB-less UNORM or an HDR format: draw in the buffer's own format
        desc.Format = g.format;
        desc.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2D;
        g.device->CreateRenderTargetView(g.frames[i].target, &desc, handle);
        g.frames[i].rtv = handle;
        handle.ptr += rtv_size;
    }
    return true;
}

bool Initialise(IDXGISwapChain3 *swapchain) {
    // Only the queue this swapchain presents on, on the swapchain's own device; never a guess.
    ID3D12CommandQueue *queue = QueueOf(swapchain);
    if (!queue) {
        static bool logged = false; // presented every frame: say it once
        if (!logged) {
            logged = true;
            Log("no overlay on a swapchain whose presenting queue is unknown (created before the hooks)");
        }
        return false;
    }
    DXGI_SWAP_CHAIN_DESC desc{};
    ID3D12Device *queue_device = nullptr;
    if (FAILED(swapchain->GetDesc(&desc)) || FAILED(swapchain->GetDevice(IID_PPV_ARGS(&g.device))) ||
        FAILED(queue->GetDevice(IID_PPV_ARGS(&queue_device)))) {
        queue->Release();
        return false;
    }
    const bool same_device = Identity(queue_device) == Identity(g.device);
    queue_device->Release();
    if (!same_device || queue->GetDesc().Type != D3D12_COMMAND_LIST_TYPE_DIRECT) {
        queue->Release();
        Passthrough("the swapchain's presenting queue is not a direct queue of its device");
        return false;
    }
    g.queue = queue;
    g.swapchain = swapchain;
    g.hwnd = desc.OutputWindow;
    g.format = desc.BufferDesc.Format;
    g.frames.assign(desc.BufferCount, {});

    D3D12_DESCRIPTOR_HEAP_DESC rtv{D3D12_DESCRIPTOR_HEAP_TYPE_RTV, desc.BufferCount, D3D12_DESCRIPTOR_HEAP_FLAG_NONE, 0};
    D3D12_DESCRIPTOR_HEAP_DESC srv{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, kSrvSlots, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    g_av.seen = 0; // a new heap: the avatar is uploaded again from the pixels kept
    if (FAILED(g.device->CreateDescriptorHeap(&rtv, IID_PPV_ARGS(&g.rtv_heap))) ||
        FAILED(g.device->CreateDescriptorHeap(&srv, IID_PPV_ARGS(&g.srv_heap)))) {
        return false;
    }
    for (auto &f : g.frames) {
        if (FAILED(g.device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&f.allocator)))) {
            return false;
        }
    }
    if (FAILED(g.device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, g.frames[0].allocator, nullptr,
                                           IID_PPV_ARGS(&g.list))) ||
        FAILED(g.list->Close()) ||
        FAILED(g.device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&g.fence)))) {
        return false;
    }
    g.fence_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    D3D12_COMMAND_QUEUE_DESC upload{D3D12_COMMAND_LIST_TYPE_DIRECT, 0, D3D12_COMMAND_QUEUE_FLAG_NONE, 0};
    if (FAILED(g.device->CreateCommandQueue(&upload, IID_PPV_ARGS(&g.upload_queue))) || !CreateTargets()) {
        return false;
    }

    IMGUI_CHECKVERSION();
    ImGui::CreateContext();
    ImGuiIO &io = ImGui::GetIO();
    io.IniFilename = nullptr; // nothing written to the game's folder
    io.LogFilename = nullptr;
    io.ConfigFlags |= ImGuiConfigFlags_NoMouseCursorChange;
    g.scale = desc.BufferDesc.Height > 0 ? desc.BufferDesc.Height / 1080.0f : 1.0f;
    LoadFonts(g.scale);
    ImGui_ImplWin32_Init(g.hwnd);
    ImGui_ImplDX12_InitInfo info{};
    info.Device = g.device;
    info.CommandQueue = g.upload_queue; // never the game's: see upload_queue
    info.NumFramesInFlight = static_cast<int>(desc.BufferCount);
    info.RTVFormat = g.format;
    info.SrvDescriptorHeap = g.srv_heap;
    info.LegacySingleSrvCpuDescriptor = g.srv_heap->GetCPUDescriptorHandleForHeapStart();
    info.LegacySingleSrvGpuDescriptor = g.srv_heap->GetGPUDescriptorHandleForHeapStart();
    if (!ImGui_ImplDX12_Init(&info)) {
        return false;
    }
    InstallWindowHook(g.hwnd);
    g.last = std::chrono::steady_clock::now();
    Log("first frame: %ux%u, %u buffers, format %u", desc.BufferDesc.Width, desc.BufferDesc.Height,
        desc.BufferCount, static_cast<unsigned>(g.format));
    SendEvent(1, std::to_string(desc.BufferDesc.Width) + "x" + std::to_string(desc.BufferDesc.Height));
    return true;
}

void Draw(IDXGISwapChain3 *swapchain) {
    if (!g.ready) {
        g.ready = Initialise(swapchain);
        if (!g.ready) {
            return;
        }
    }
    if (swapchain != g.swapchain) {
        return; // another swapchain (a launcher window, a video): only the game's gets the overlay
    }
    auto *queue = g.queue;
    // Never wait inside the game's Present: when this back buffer's previous overlay pass has not
    // finished on the GPU, skip the overlay for this frame (the game is not held up).
    FrameContext &frame = g.frames[swapchain->GetCurrentBackBufferIndex()];
    if (frame.fence_value && g.fence->GetCompletedValue() < frame.fence_value) {
        return;
    }
    const auto now = std::chrono::steady_clock::now();
    float dt = std::chrono::duration<float>(now - g.last).count();
    g.last = now;
    dt = dt < 0.0f ? 0.0f : dt > 0.1f ? 0.1f : dt;

    PollController();
    PrepareAvatar();
    ImGui_ImplDX12_NewFrame();
    ImGui_ImplWin32_NewFrame();
    // The image is the back buffer, whatever size Windows gives the window (DPI scaling, a
    // resized window): draw in back-buffer pixels and bring the mouse into them.
    {
        DXGI_SWAP_CHAIN_DESC desc{};
        swapchain->GetDesc(&desc);
        ImGuiIO &io = ImGui::GetIO();
        RECT client{};
        GetClientRect(g.hwnd, &client);
        const float cw = static_cast<float>(client.right - client.left), ch = static_cast<float>(client.bottom - client.top);
        io.DisplaySize = ImVec2(static_cast<float>(desc.BufferDesc.Width), static_cast<float>(desc.BufferDesc.Height));
        POINT cursor{};
        if (cw > 0 && ch > 0 && GetCursorPos(&cursor) && ScreenToClient(g.hwnd, &cursor)) {
            io.AddMousePosEvent(cursor.x * io.DisplaySize.x / cw, cursor.y * io.DisplaySize.y / ch);
        }
    }
    ImGui::NewFrame();
    const ImVec2 size = ImGui::GetIO().DisplaySize;
    DrawUi(size.x, size.y, dt);
    ImGui::Render();

    frame.allocator->Reset();
    g.list->Reset(frame.allocator, nullptr);
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition.pResource = frame.target;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_PRESENT;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_RENDER_TARGET;
    g.list->ResourceBarrier(1, &barrier);
    RecordAvatarCopy(g.list);
    g.list->OMSetRenderTargets(1, &frame.rtv, FALSE, nullptr);
    g.list->SetDescriptorHeaps(1, &g.srv_heap);
    ImGui_ImplDX12_RenderDrawData(ImGui::GetDrawData(), g.list);
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_PRESENT;
    g.list->ResourceBarrier(1, &barrier);
    g.list->Close();
    ID3D12CommandList *lists[] = {g.list};
    // the swapchain's own queue: ordered after the game's work on it for this frame, before its Present
    queue->ExecuteCommandLists(1, lists);
    frame.fence_value = g.fence_next++;
    queue->Signal(g.fence, frame.fence_value);
}

// SEH around the C++ draw (no objects with destructors in this frame).
bool GuardedDraw(IDXGISwapChain3 *swapchain) {
    __try {
        Draw(swapchain);
        return true;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return false;
    }
}

void OnPresent(IDXGISwapChain3 *swapchain) {
    if (InPassthrough()) {
        return;
    }
    if (!GuardedDraw(swapchain)) {
        Passthrough("an exception in the overlay's draw");
    }
}

HRESULT STDMETHODCALLTYPE HookPresent(IDXGISwapChain3 *swapchain, UINT sync, UINT flags) {
    if (!(flags & DXGI_PRESENT_TEST)) {
        OnPresent(swapchain);
    }
    return g_present(swapchain, sync, flags);
}

HRESULT STDMETHODCALLTYPE HookPresent1(IDXGISwapChain3 *swapchain, UINT sync, UINT flags, const DXGI_PRESENT_PARAMETERS *params) {
    if (!(flags & DXGI_PRESENT_TEST)) {
        OnPresent(swapchain);
    }
    return g_present1(swapchain, sync, flags, params);
}

HRESULT STDMETHODCALLTYPE HookResize(IDXGISwapChain3 *swapchain, UINT count, UINT width, UINT height, DXGI_FORMAT format, UINT flags) {
    const bool ours = g.ready && swapchain == g.swapchain;
    if (ours) {
        WaitIdle();
        ReleaseTargets();
    }
    HRESULT hr = g_resize(swapchain, count, width, height, format, flags);
    if (ours && SUCCEEDED(hr)) {
        DXGI_SWAP_CHAIN_DESC desc{};
        swapchain->GetDesc(&desc);
        if (desc.BufferCount != g.frames.size() || desc.BufferDesc.Format != g.format) {
            Passthrough("the swapchain changed its buffer count or format"); // v1 does not rebuild
        } else if (!CreateTargets()) {
            Passthrough("the back buffers could not be read after a resize");
        }
    }
    return hr;
}

// The factory's swapchain creators: pDevice of a D3D12 swapchain is the queue it presents on.
HRESULT STDMETHODCALLTYPE HookCreateSwapChain(IDXGIFactory2 *factory, IUnknown *device, DXGI_SWAP_CHAIN_DESC *desc,
                                              IDXGISwapChain **chain) {
    const HRESULT hr = g_create(factory, device, desc, chain);
    Remember(device, chain ? *chain : nullptr, hr);
    return hr;
}

HRESULT STDMETHODCALLTYPE HookForHwnd(IDXGIFactory2 *factory, IUnknown *device, HWND window, const DXGI_SWAP_CHAIN_DESC1 *desc,
                                      const DXGI_SWAP_CHAIN_FULLSCREEN_DESC *fullscreen, IDXGIOutput *output, IDXGISwapChain1 **chain) {
    const HRESULT hr = g_for_hwnd(factory, device, window, desc, fullscreen, output, chain);
    Remember(device, chain ? *chain : nullptr, hr);
    return hr;
}

HRESULT STDMETHODCALLTYPE HookForCoreWindow(IDXGIFactory2 *factory, IUnknown *device, IUnknown *window,
                                            const DXGI_SWAP_CHAIN_DESC1 *desc, IDXGIOutput *output, IDXGISwapChain1 **chain) {
    const HRESULT hr = g_for_core(factory, device, window, desc, output, chain);
    Remember(device, chain ? *chain : nullptr, hr);
    return hr;
}

HRESULT STDMETHODCALLTYPE HookForComposition(IDXGIFactory2 *factory, IUnknown *device, const DXGI_SWAP_CHAIN_DESC1 *desc,
                                             IDXGIOutput *output, IDXGISwapChain1 **chain) {
    const HRESULT hr = g_for_composition(factory, device, desc, output, chain);
    Remember(device, chain ? *chain : nullptr, hr);
    return hr;
}

bool Hook(void *target, void *detour, void **original) {
    return MH_CreateHook(target, detour, original) == MH_OK && MH_EnableHook(target) == MH_OK;
}

LRESULT CALLBACK DummyProc(HWND h, UINT m, WPARAM w, LPARAM l) { return DefWindowProcW(h, m, w, l); }

} // namespace

bool InstallRenderHooks() {
    WNDCLASSEXW wc{sizeof(wc), CS_CLASSDC, DummyProc, 0, 0, GetModuleHandleW(nullptr), nullptr, nullptr, nullptr, nullptr, L"FiOverlayDummy", nullptr};
    RegisterClassExW(&wc);
    HWND window = CreateWindowExW(0, wc.lpszClassName, L"", WS_OVERLAPPEDWINDOW, 0, 0, 64, 64, nullptr, nullptr, wc.hInstance, nullptr);
    if (!window) {
        return false;
    }
    bool ok = false;
    ID3D12Device *device = nullptr;
    ID3D12CommandQueue *queue = nullptr;
    IDXGIFactory4 *factory = nullptr;
    IDXGISwapChain1 *swapchain = nullptr;
    do {
        if (FAILED(D3D12CreateDevice(nullptr, D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device)))) {
            break;
        }
        D3D12_COMMAND_QUEUE_DESC qd{D3D12_COMMAND_LIST_TYPE_DIRECT, 0, D3D12_COMMAND_QUEUE_FLAG_NONE, 0};
        if (FAILED(device->CreateCommandQueue(&qd, IID_PPV_ARGS(&queue))) || FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) {
            break;
        }
        DXGI_SWAP_CHAIN_DESC1 sd{};
        sd.Width = 64;
        sd.Height = 64;
        sd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
        sd.SampleDesc.Count = 1;
        sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        sd.BufferCount = 2;
        sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
        if (FAILED(factory->CreateSwapChainForHwnd(queue, window, &sd, nullptr, nullptr, &swapchain))) {
            break;
        }
        void **chain = *reinterpret_cast<void ***>(swapchain);
        void **factory_vtbl = *reinterpret_cast<void ***>(factory);
        // IDXGIFactory::CreateSwapChain 10, IDXGIFactory2::CreateSwapChainForHwnd 15, ForCoreWindow 16, ForComposition 24
        ok = Hook(factory_vtbl[10], reinterpret_cast<void *>(&HookCreateSwapChain), reinterpret_cast<void **>(&g_create)) &&
             Hook(factory_vtbl[15], reinterpret_cast<void *>(&HookForHwnd), reinterpret_cast<void **>(&g_for_hwnd)) &&
             Hook(factory_vtbl[16], reinterpret_cast<void *>(&HookForCoreWindow), reinterpret_cast<void **>(&g_for_core)) &&
             Hook(factory_vtbl[24], reinterpret_cast<void *>(&HookForComposition), reinterpret_cast<void **>(&g_for_composition)) &&
             Hook(chain[8], reinterpret_cast<void *>(&HookPresent), reinterpret_cast<void **>(&g_present)) &&
             Hook(chain[13], reinterpret_cast<void *>(&HookResize), reinterpret_cast<void **>(&g_resize)) &&
             Hook(chain[22], reinterpret_cast<void *>(&HookPresent1), reinterpret_cast<void **>(&g_present1));
    } while (false);
    SafeRelease(swapchain);
    SafeRelease(factory);
    SafeRelease(queue);
    SafeRelease(device);
    DestroyWindow(window);
    UnregisterClassW(wc.lpszClassName, wc.hInstance);
    return ok;
}

float RenderScale() { return g.scale; }

void SetAvatar(AvatarImage avatar) {
    std::lock_guard<std::mutex> lock(g_avatar_lock);
    if (avatar.side == 0) avatar.rgba.clear();
    g_avatar_pending = std::move(avatar);
    ++g_avatar_serial;
}

uint64_t AvatarTexture() { return g_av.texture ? g_av.gpu.ptr : 0; }

} // namespace fi
