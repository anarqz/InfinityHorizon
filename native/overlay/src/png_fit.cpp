// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//
// fi_overlay_fit_png (T19, OD-138; docs/launcher/PNG_FIT.md): a picture fitted into a byte budget
// as a PNG the game reads. WIC decodes and scales (the parts Wine/Proton's windowscodecs has: the
// decoders, IWICFormatConverter to 32bpp BGRA, IWICBitmapScaler, IWICStream on memory); png_write
// writes the file, so it holds only IHDR/IDAT/IEND at zlib level 9, which WIC's PNG
// encoder cannot promise. Generic: the caller's FiPngFit says the budget and the scale steps.
#include <windows.h>
#include <wincodec.h>

#include <cstring>
#include <vector>

#include "fi_overlay.h"
#include "png_write.h"

namespace {

// Spelled out so the build depends neither on the SDK's CLSID_WICImagingFactory mapping (8.x SDKs
// map it to the Factory2 class) nor on windowscodecs.lib.
const CLSID kWicFactory = {0xcacaf262, 0x9370, 0x4615, {0xa1, 0x3b, 0x9f, 0x55, 0x39, 0xda, 0x4c, 0x0a}};
const GUID kFormatBgra = {0x6fddc324, 0x4e03, 0x4bfe, {0xb1, 0x85, 0x3d, 0x77, 0x76, 0x8d, 0xc9, 0x0f}};

constexpr UINT kMaxSide = 16384;
constexpr uint64_t kMaxPixels = 4096ull * 4096ull; // FiPngFit v1: larger inputs are refused before decoding
constexpr size_t kReadCap = 4u << 20;
constexpr size_t kFitV1Size = 92;
static_assert(sizeof(FiPngFit) == kFitV1Size, "FiPngFit v1 layout");

template <class T>
class Ref {
public:
    Ref() = default;
    Ref(const Ref &) = delete;
    Ref &operator=(const Ref &) = delete;
    ~Ref() { Reset(); }
    T **Put() { Reset(); return &p_; }
    T *Get() const { return p_; }
    T *operator->() const { return p_; }
    void Reset() { if (p_) { p_->Release(); p_ = nullptr; } }

private:
    T *p_ = nullptr;
};

class ComScope {
public:
    ComScope() {
        const HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        ok_ = SUCCEEDED(hr) || hr == RPC_E_CHANGED_MODE; // an STA thread keeps its apartment
        owned_ = SUCCEEDED(hr);
    }
    ~ComScope() { if (owned_) CoUninitialize(); }
    bool ok() const { return ok_; }

private:
    bool ok_ = false, owned_ = false;
};

bool StepsOk(const FiPngFit &fit) {
    if (!fit.step_count || fit.step_count > FI_PNG_FIT_MAX_STEPS || !fit.max_bytes) return false;
    for (uint32_t i = 0; i < fit.step_count; ++i)
        if (!fit.steps_permille[i] || fit.steps_permille[i] > 1000) return false;
    return true;
}

UINT Scaled(UINT side, uint32_t permille) {
    const UINT n = static_cast<UINT>((static_cast<uint64_t>(side) * permille + 500) / 1000);
    return n ? n : 1;
}

int32_t Fit(const BYTE *in, size_t in_size, const FiPngFit &fit, BYTE *out, size_t out_cap, size_t *out_size,
            uint32_t *out_width, uint32_t *out_height) {
    ComScope com;
    if (!com.ok()) return FI_PNG_FIT_E_COM;
    Ref<IWICImagingFactory> factory;
    if (FAILED(CoCreateInstance(kWicFactory, nullptr, CLSCTX_INPROC_SERVER, IID_PPV_ARGS(factory.Put()))))
        return FI_PNG_FIT_E_COM;
    Ref<IWICStream> input;
    Ref<IWICBitmapDecoder> decoder;
    Ref<IWICBitmapFrameDecode> frame;
    Ref<IWICFormatConverter> bgra;
    Ref<IWICBitmap> full;
    UINT width = 0, height = 0;
    if (FAILED(factory->CreateStream(input.Put())) ||
        FAILED(input->InitializeFromMemory(const_cast<BYTE *>(in), static_cast<DWORD>(in_size))) ||
        FAILED(factory->CreateDecoderFromStream(input.Get(), nullptr, WICDecodeMetadataCacheOnDemand, decoder.Put())) ||
        FAILED(decoder->GetFrame(0, frame.Put())) || FAILED(frame->GetSize(&width, &height)) || !width || !height ||
        width > kMaxSide || height > kMaxSide || uint64_t{width} * height > kMaxPixels || FAILED(factory->CreateFormatConverter(bgra.Put())) ||
        FAILED(bgra->Initialize(frame.Get(), kFormatBgra, WICBitmapDitherTypeNone, nullptr, 0.0,
                                WICBitmapPaletteTypeCustom)) ||
        FAILED(factory->CreateBitmapFromSource(bgra.Get(), WICBitmapCacheOnLoad, full.Put())))
        return FI_PNG_FIT_E_DECODE;
    size_t limit = fit.max_bytes;
    if (fit.hard_cap_bytes && fit.hard_cap_bytes < limit) limit = fit.hard_cap_bytes;
    if (out_cap < limit) limit = out_cap;
    const bool alpha = fit.keep_alpha != 0;
    const uint32_t min_width = fit.min_width ? fit.min_width : 1;
    std::vector<BYTE> pixels, png;
    for (uint32_t step = 0; step < fit.step_count; ++step) {
        const UINT w = Scaled(width, fit.steps_permille[step]), h = Scaled(height, fit.steps_permille[step]);
        if (w < min_width) break;
        Ref<IWICBitmapScaler> scaler;
        IWICBitmapSource *source = full.Get();
        if (w != width || h != height) {
            if (FAILED(factory->CreateBitmapScaler(scaler.Put()))) return FI_PNG_FIT_E_ENCODE;
            if (FAILED(scaler->Initialize(full.Get(), w, h, WICBitmapInterpolationModeFant)) &&
                (FAILED(factory->CreateBitmapScaler(scaler.Put())) ||
                 FAILED(scaler->Initialize(full.Get(), w, h, WICBitmapInterpolationModeLinear))))
                return FI_PNG_FIT_E_ENCODE;
            source = scaler.Get();
        }
        const UINT stride = w * 4;
        pixels.resize(static_cast<size_t>(stride) * h);
        if (FAILED(source->CopyPixels(nullptr, stride, static_cast<UINT>(pixels.size()), pixels.data())))
            return FI_PNG_FIT_E_ENCODE;
        // BGRA to RGBA (or RGB) in place: an RGB row is packed at the front of its BGRA row.
        for (UINT y = 0; y < h; ++y) {
            BYTE *row = pixels.data() + static_cast<size_t>(stride) * y;
            for (UINT x = 0; x < w; ++x) {
                const BYTE b = row[x * 4], g = row[x * 4 + 1], r = row[x * 4 + 2], a = row[x * 4 + 3];
                BYTE *to = row + x * (alpha ? 4 : 3);
                to[0] = r; to[1] = g; to[2] = b;
                if (alpha) to[3] = a;
            }
        }
        if (!fi::png::Write(pixels.data(), w, h, stride, alpha, png)) return FI_PNG_FIT_E_ENCODE;
        if (png.size() <= limit) {
            std::memcpy(out, png.data(), png.size());
            *out_size = png.size();
            if (out_width) *out_width = w;
            if (out_height) *out_height = h;
            return FI_PNG_FIT_OK;
        }
    }
    return FI_PNG_FIT_E_TOO_BIG;
}

} // namespace

extern "C" __declspec(dllexport) uint32_t fi_overlay_png_fit_version(void) { return FI_PNG_FIT_VERSION; }

extern "C" __declspec(dllexport) int32_t fi_overlay_fit_png(const void *in, size_t in_size, const FiPngFit *fit,
                                                            void *out, size_t out_cap, size_t *out_size,
                                                            uint32_t *out_width, uint32_t *out_height) {
    if (out_size) *out_size = 0;
    if (out_width) *out_width = 0;
    if (out_height) *out_height = 0;
    if (!fit) return FI_PNG_FIT_E_ARGS;
    if (fit->version == 0 || fit->version > FI_PNG_FIT_VERSION || fit->size < kFitV1Size) return FI_PNG_FIT_E_VERSION;
    if (!in || !in_size || in_size > kReadCap || !out || !out_cap || !out_size || !StepsOk(*fit))
        return FI_PNG_FIT_E_ARGS;
    try {
        const int32_t result = Fit(static_cast<const BYTE *>(in), in_size, *fit, static_cast<BYTE *>(out), out_cap,
                                   out_size, out_width, out_height);
        if (result != FI_PNG_FIT_OK) *out_size = 0;
        return result;
    } catch (...) {
        *out_size = 0;
        return FI_PNG_FIT_E_ENCODE;
    }
}
