// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the D3D12 hooks and the per-frame draw.
//
// The vtables come from a dummy device, queue and swapchain on a hidden window; MinHook then
// hooks IDXGISwapChain::Present / Present1 / ResizeBuffers and ID3D12CommandQueue::
// ExecuteCommandLists (to learn the game's direct queue, the one its swapchain presents on).
// In Present the overlay records one command list into the current back buffer and submits it
// on that queue before the game's present. A fence per back buffer keeps an allocator from being
// reset while the GPU still reads it. Any structured exception in the draw sends the overlay to
// passthrough (main.cpp): every hook is disabled and the game's calls run untouched.
#include "overlay.h"

#include <MinHook.h>
#include <d3d12.h>
#include <dxgi1_4.h>

#include <atomic>
#include <chrono>
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
using ExecuteFn = void(STDMETHODCALLTYPE *)(ID3D12CommandQueue *, UINT, ID3D12CommandList *const *);

PresentFn g_present = nullptr;
Present1Fn g_present1 = nullptr;
ResizeFn g_resize = nullptr;
ExecuteFn g_execute = nullptr;

std::atomic<ID3D12CommandQueue *> g_queue{nullptr}; // borrowed: the game's direct queue

struct FrameContext {
    ID3D12CommandAllocator *allocator = nullptr;
    ID3D12Resource *target = nullptr;
    D3D12_CPU_DESCRIPTOR_HANDLE rtv{};
    UINT64 fence_value = 0;
};

struct Renderer {
    bool ready = false;
    IDXGISwapChain3 *swapchain = nullptr; // the one we drew for (not owned)
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

void WaitIdle() {
    auto *queue = g_queue.load();
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
    auto *queue = g_queue.load();
    if (!queue) {
        return false; // the game has not submitted on a direct queue yet
    }
    DXGI_SWAP_CHAIN_DESC desc{};
    if (FAILED(swapchain->GetDesc(&desc)) || FAILED(swapchain->GetDevice(IID_PPV_ARGS(&g.device)))) {
        return false;
    }
    g.swapchain = swapchain;
    g.hwnd = desc.OutputWindow;
    g.format = desc.BufferDesc.Format;
    g.frames.assign(desc.BufferCount, {});

    D3D12_DESCRIPTOR_HEAP_DESC rtv{D3D12_DESCRIPTOR_HEAP_TYPE_RTV, desc.BufferCount, D3D12_DESCRIPTOR_HEAP_FLAG_NONE, 0};
    D3D12_DESCRIPTOR_HEAP_DESC srv{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
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
    auto *queue = g_queue.load();
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
    g.list->OMSetRenderTargets(1, &frame.rtv, FALSE, nullptr);
    g.list->SetDescriptorHeaps(1, &g.srv_heap);
    ImGui_ImplDX12_RenderDrawData(ImGui::GetDrawData(), g.list);
    barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
    barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_PRESENT;
    g.list->ResourceBarrier(1, &barrier);
    g.list->Close();
    ID3D12CommandList *lists[] = {g.list};
    g_execute(queue, 1, lists); // the original: our own submission is not observed
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

void STDMETHODCALLTYPE HookExecute(ID3D12CommandQueue *queue, UINT count, ID3D12CommandList *const *lists) {
    if (!g_queue.load()) {
        const D3D12_COMMAND_QUEUE_DESC desc = queue->GetDesc();
        if (desc.Type == D3D12_COMMAND_LIST_TYPE_DIRECT) {
            g_queue.store(queue);
        }
    }
    g_execute(queue, count, lists);
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
        void **queue_vtbl = *reinterpret_cast<void ***>(queue);
        ok = Hook(queue_vtbl[10], reinterpret_cast<void *>(&HookExecute), reinterpret_cast<void **>(&g_execute)) &&
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

} // namespace fi
