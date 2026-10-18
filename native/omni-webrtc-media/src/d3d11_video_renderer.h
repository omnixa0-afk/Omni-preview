#pragma once

// Vendor-neutral D3D11 video renderer for the SwapChainPanel video path
// (docs/decisions/video-render-architecture.md, docs/notes/video-gpu-render-swapchainpanel.md).
//
// Uses ONLY OS abstractions (D3D11 / DXGI / D3DCompile) — never vendor SDKs
// (NVENC/NVDEC/AMF/QSV). Falls back to the WARP software device when no hardware device is
// available, so it "works everywhere" (integrated Intel, AMD, Intel Arc, NVIDIA, headless CI).
//
// Increment 1a: device + offscreen render-target + staging readback (headless plumbing).
// Increment 1b: I420 -> RGB on the GPU via a BT.601 pixel shader (offscreen + readback),
//   golden-verified against libyuv headlessly.
// Increment 2 (this file): composition swapchain (CreateSwapChainForComposition) + render the
//   I420 frame to its backbuffer + Present. The C# side binds the swapchain to a WinUI 3
//   SwapChainPanel via ISwapChainPanelNative::SetSwapChain. Rendering to the backbuffer is
//   still headless-verifiable (readback before Present); only the visible Present + panel
//   binding need a live run.

#if defined(_WIN32)

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>

#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi.h>
#include <dxgi1_2.h>
#include <wrl/client.h>

namespace omni_media {

class D3D11VideoRenderer {
public:
    // Creates a D3D11 device (hardware, falling back to WARP software). Idempotent.
    bool Initialize()
    {
        if (device_) {
            return true;
        }

        // Device selection (task #85 energy + #87 user choice): pick the GPU the receive path
        // (HW H.264 DXVA decode + swapchain compositing) runs on. Fallback chain:
        //   0) the user's explicitly chosen adapter (preferred_adapter_vendor_id_, 0 = Auto),
        //   1) Auto = the INTEGRATED GPU's media engine (Intel 0x8086) — the iGPU draws far less
        //      power than waking the discrete GPU (NVDEC/3D) for a 1:1 720p call,
        //   2) default HW adapter with video support, 3) default HW (no video → SW decode), 4) WARP.
        // Vendor-neutral: we only bias the *adapter*; the codec stays MF/DXVA on whatever it exposes.
        D3D_FEATURE_LEVEL feature_level = D3D_FEATURE_LEVEL_11_0;
        const UINT video_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        HRESULT hr = E_FAIL;
        if (preferred_adapter_vendor_id_ != 0) {
            hr = CreateDeviceOnVendorAdapter(preferred_adapter_vendor_id_, video_flags, &feature_level);
        }
        if (FAILED(hr)) {
            hr = CreateDeviceOnVendorAdapter(0x8086u, video_flags, &feature_level);
        }
        if (FAILED(hr)) {
            hr = ::D3D11CreateDevice(
                nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, video_flags, nullptr, 0,
                D3D11_SDK_VERSION, device_.GetAddressOf(), &feature_level, context_.GetAddressOf());
        }
        if (FAILED(hr)) {
            hr = ::D3D11CreateDevice(
                nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                D3D11_SDK_VERSION, device_.GetAddressOf(), &feature_level, context_.GetAddressOf());
        }
        if (FAILED(hr)) {
            hr = ::D3D11CreateDevice(
                nullptr, D3D_DRIVER_TYPE_WARP, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                D3D11_SDK_VERSION, device_.GetAddressOf(), &feature_level, context_.GetAddressOf());
        }
        if (SUCCEEDED(hr) && device_) {
            // The decoder MFT decodes on its own thread; D3D11 must be multithread-protected.
            Microsoft::WRL::ComPtr<ID3D10Multithread> multithread;
            if (SUCCEEDED(device_.As(&multithread)) && multithread) {
                multithread->SetMultithreadProtected(TRUE);
            }
        }
        (void)feature_level;
        return SUCCEEDED(hr);
    }

    // Task #87: set the user's chosen receive-path GPU (PCI vendor id; 0 = Auto). No effect once
    // the device is created (Initialize() is idempotent), so callers set it before the first attach.
    void SetPreferredAdapterVendor(UINT vendor_id) { preferred_adapter_vendor_id_ = vendor_id; }

    // The renderer's D3D11 device — shared with the HW H.264 decoder (task #84 increment 3) so the
    // decoded NV12 texture is rendered without a cross-device copy. Null until Initialize().
    ID3D11Device* device() const { return device_.Get(); }
    bool HasSwapChain() const { return swap_chain_ != nullptr; }

    // Task #85: create the device on the first non-software DXGI adapter with the given PCI vendor
    // id (0x8086 = Intel). Lets Initialize() prefer the integrated GPU's media engine for energy.
    // D3D_DRIVER_TYPE_UNKNOWN is mandatory when an explicit adapter is supplied. Returns a failure
    // HRESULT when no such adapter exists so the caller falls back to the default adapter / WARP.
    HRESULT CreateDeviceOnVendorAdapter(UINT vendor_id, UINT flags, D3D_FEATURE_LEVEL* feature_level)
    {
        Microsoft::WRL::ComPtr<IDXGIFactory1> factory;
        if (FAILED(::CreateDXGIFactory1(IID_PPV_ARGS(factory.GetAddressOf())))) {
            return E_FAIL;
        }
        Microsoft::WRL::ComPtr<IDXGIAdapter1> adapter;
        for (UINT i = 0;
             factory->EnumAdapters1(i, adapter.ReleaseAndGetAddressOf()) != DXGI_ERROR_NOT_FOUND;
             ++i) {
            DXGI_ADAPTER_DESC1 desc = {};
            if (FAILED(adapter->GetDesc1(&desc))) {
                continue;
            }
            if ((desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) != 0 || desc.VendorId != vendor_id) {
                continue;
            }
            return ::D3D11CreateDevice(
                adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, flags, nullptr, 0,
                D3D11_SDK_VERSION, device_.GetAddressOf(), feature_level, context_.GetAddressOf());
        }
        return E_FAIL;
    }

    // Clears an offscreen BGRA render target to (r,g,b,255) and reads it back (B,G,R,A).
    bool RenderSolidAndReadback(
        uint8_t r, uint8_t g, uint8_t b,
        int width, int height,
        uint8_t* out, size_t out_len)
    {
        if (!device_ || !context_ || width <= 0 || height <= 0 || out == nullptr) {
            return false;
        }
        Microsoft::WRL::ComPtr<ID3D11Texture2D> target;
        Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv;
        if (!CreateRenderTarget(width, height, target.GetAddressOf(), rtv.GetAddressOf())) {
            return false;
        }
        const float clear_rgba[4] = {
            static_cast<float>(r) / 255.0f,
            static_cast<float>(g) / 255.0f,
            static_cast<float>(b) / 255.0f,
            1.0f,
        };
        context_->ClearRenderTargetView(rtv.Get(), clear_rgba);
        return ReadbackBgra(target.Get(), width, height, out, out_len);
    }

    // Converts an I420 frame to BGRA on the GPU (BT.601 shader) into an offscreen target and
    // reads it back. Headless-verifiable against libyuv.
    bool RenderI420ToRgba(
        const uint8_t* y, int y_stride,
        const uint8_t* u, int u_stride,
        const uint8_t* v, int v_stride,
        int width, int height,
        uint8_t* out, size_t out_len)
    {
        Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> target;
        if (!device_ || !context_ || width <= 0 || height <= 0 || out == nullptr) {
            return false;
        }
        if (!CreateRenderTarget(width, height, target.GetAddressOf(), rtv.GetAddressOf())) {
            return false;
        }
        if (!DrawI420(y, y_stride, u, u_stride, v, v_stride, width, height, rtv.Get())) {
            return false;
        }
        return ReadbackBgra(target.Get(), width, height, out, out_len);
    }

    // Task #84 increment 2: render an NV12 ID3D11Texture2D (HW-decoder output, a GPU surface —
    // possibly a texture-array slice `subresource`) to an offscreen RGBA target and read it back.
    // Headless golden path (vs libyuv::NV12ToARGB). The NV12 texture needs D3D11_BIND_SHADER_RESOURCE.
    bool RenderNv12ToRgba(
        ID3D11Texture2D* nv12, UINT subresource, int width, int height,
        uint8_t* out, size_t out_len)
    {
        Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> target;
        if (!device_ || !context_ || nv12 == nullptr || width <= 0 || height <= 0 || out == nullptr) {
            return false;
        }
        if (!CreateRenderTarget(width, height, target.GetAddressOf(), rtv.GetAddressOf())) {
            return false;
        }
        if (!DrawNv12(nv12, subresource, width, height, rtv.Get())) {
            return false;
        }
        return ReadbackBgra(target.Get(), width, height, out, out_len);
    }

    // Task #84 increment 2: render an NV12 texture (HW-decoder output) to the composition swapchain
    // backbuffer (fitted whole, see FitViewport) + Present. The zero-copy receive leaf: decoded GPU surface →
    // swapchain, no CPU round-trip and no BGRA across the FFI.
    bool RenderNv12TextureToSwapChain(ID3D11Texture2D* nv12, UINT subresource, int width, int height)
    {
        Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv;
        if (!AcquireBackbufferRtv(rtv.GetAddressOf())) {
            return false;
        }
        if (!DrawNv12(nv12, subresource, width, height, rtv.Get(), swap_chain_width_, swap_chain_height_)) {
            return false;
        }
        return SUCCEEDED(swap_chain_->Present(0, 0));
    }

    // Creates a composition swapchain (for a WinUI 3 SwapChainPanel) sized width x height.
    // Returns the IDXGISwapChain1* (AddRef'd) via out_swapchain so the C# layer can call
    // ISwapChainPanelNative::SetSwapChain on the UI thread. The renderer keeps its own ref.
    bool CreateCompositionSwapChain(int width, int height, IDXGISwapChain1** out_swapchain)
    {
        if (!Initialize() || width <= 0 || height <= 0 || out_swapchain == nullptr) {
            return false;
        }

        Microsoft::WRL::ComPtr<IDXGIDevice> dxgi_device;
        if (FAILED(device_.As(&dxgi_device))) {
            return false;
        }
        Microsoft::WRL::ComPtr<IDXGIAdapter> adapter;
        if (FAILED(dxgi_device->GetAdapter(adapter.GetAddressOf()))) {
            return false;
        }
        Microsoft::WRL::ComPtr<IDXGIFactory2> factory;
        if (FAILED(adapter->GetParent(IID_PPV_ARGS(factory.GetAddressOf())))) {
            return false;
        }

        DXGI_SWAP_CHAIN_DESC1 desc = {};
        desc.Width = static_cast<UINT>(width);
        desc.Height = static_cast<UINT>(height);
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        desc.BufferCount = 2;
        desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
        desc.Scaling = DXGI_SCALING_STRETCH;
        desc.AlphaMode = DXGI_ALPHA_MODE_IGNORE;

        swap_chain_.Reset();
        if (FAILED(factory->CreateSwapChainForComposition(
                device_.Get(), &desc, nullptr, swap_chain_.GetAddressOf()))) {
            return false;
        }
        swap_chain_width_ = width;
        swap_chain_height_ = height;
        return SUCCEEDED(swap_chain_.CopyTo(out_swapchain));
    }

    bool ResizeSwapChain(int width, int height)
    {
        if (!swap_chain_ || width <= 0 || height <= 0) {
            return false;
        }
        if (width == swap_chain_width_ && height == swap_chain_height_) {
            return true;
        }
        if (FAILED(swap_chain_->ResizeBuffers(
                0, static_cast<UINT>(width), static_cast<UINT>(height),
                DXGI_FORMAT_UNKNOWN, 0))) {
            return false;
        }
        swap_chain_width_ = width;
        swap_chain_height_ = height;
        return true;
    }

    // Renders an I420 frame to the swapchain backbuffer (fitted whole, see FitViewport) and Presents.
    bool RenderI420ToSwapChain(
        const uint8_t* y, int y_stride,
        const uint8_t* u, int u_stride,
        const uint8_t* v, int v_stride,
        int width, int height)
    {
        Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv;
        if (!AcquireBackbufferRtv(rtv.GetAddressOf())) {
            return false;
        }
        if (!DrawI420(y, y_stride, u, u_stride, v, v_stride, width, height, rtv.Get(),
                      swap_chain_width_, swap_chain_height_)) {
            return false;
        }
        return SUCCEEDED(swap_chain_->Present(0, 0));
    }

    // Test-only: render an I420 frame to the swapchain backbuffer and read it back WITHOUT
    // Present (Present needs a live visual). Lets the swapchain render path be golden-verified
    // against libyuv headlessly.
    bool RenderI420ToSwapChainAndReadback(
        const uint8_t* y, int y_stride,
        const uint8_t* u, int u_stride,
        const uint8_t* v, int v_stride,
        int width, int height,
        uint8_t* out, size_t out_len)
    {
        Microsoft::WRL::ComPtr<ID3D11Texture2D> backbuffer;
        if (!swap_chain_ || FAILED(swap_chain_->GetBuffer(0, IID_PPV_ARGS(backbuffer.GetAddressOf())))) {
            return false;
        }
        Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv;
        if (FAILED(device_->CreateRenderTargetView(backbuffer.Get(), nullptr, rtv.GetAddressOf()))) {
            return false;
        }
        if (!DrawI420(y, y_stride, u, u_stride, v, v_stride, width, height, rtv.Get(),
                      swap_chain_width_, swap_chain_height_)) {
            return false;
        }
        return ReadbackBgra(backbuffer.Get(), swap_chain_width_, swap_chain_height_, out, out_len);
    }

    void DetachSwapChain()
    {
        swap_chain_.Reset();
        swap_chain_width_ = 0;
        swap_chain_height_ = 0;
    }

private:
    bool AcquireBackbufferRtv(ID3D11RenderTargetView** out_rtv)
    {
        if (!swap_chain_) {
            return false;
        }
        Microsoft::WRL::ComPtr<ID3D11Texture2D> backbuffer;
        if (FAILED(swap_chain_->GetBuffer(0, IID_PPV_ARGS(backbuffer.GetAddressOf())))) {
            return false;
        }
        return SUCCEEDED(device_->CreateRenderTargetView(backbuffer.Get(), nullptr, out_rtv));
    }

    bool CreateRenderTarget(
        int width, int height,
        ID3D11Texture2D** out_target, ID3D11RenderTargetView** out_rtv)
    {
        D3D11_TEXTURE2D_DESC desc = {};
        desc.Width = static_cast<UINT>(width);
        desc.Height = static_cast<UINT>(height);
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = D3D11_BIND_RENDER_TARGET;
        if (FAILED(device_->CreateTexture2D(&desc, nullptr, out_target))) {
            return false;
        }
        return SUCCEEDED(device_->CreateRenderTargetView(*out_target, nullptr, out_rtv));
    }

    bool ReadbackBgra(ID3D11Texture2D* target, int width, int height, uint8_t* out, size_t out_len)
    {
        if (width <= 0 || height <= 0) {
            return false;
        }
        const size_t required = static_cast<size_t>(width) * static_cast<size_t>(height) * 4u;
        if (out_len < required) {
            return false;
        }
        D3D11_TEXTURE2D_DESC desc = {};
        target->GetDesc(&desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        desc.MiscFlags = 0;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
        if (FAILED(device_->CreateTexture2D(&desc, nullptr, staging.GetAddressOf()))) {
            return false;
        }
        context_->CopyResource(staging.Get(), target);

        D3D11_MAPPED_SUBRESOURCE mapped = {};
        if (FAILED(context_->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
            return false;
        }
        const auto* src = static_cast<const uint8_t*>(mapped.pData);
        const size_t row_bytes = static_cast<size_t>(width) * 4u;
        for (int y = 0; y < height; ++y) {
            std::memcpy(
                out + static_cast<size_t>(y) * row_bytes,
                src + static_cast<size_t>(y) * static_cast<size_t>(mapped.RowPitch),
                row_bytes);
        }
        context_->Unmap(staging.Get(), 0);
        return true;
    }

    // Draws an I420 frame (BT.601 shader) to `rtv`, fitted whole into target_width x target_height
    // (defaults to the frame size for the offscreen path).
    bool DrawI420(
        const uint8_t* y, int y_stride,
        const uint8_t* u, int u_stride,
        const uint8_t* v, int v_stride,
        int width, int height,
        ID3D11RenderTargetView* rtv,
        int target_width = 0, int target_height = 0)
    {
        if (!device_ || !context_ || y == nullptr || u == nullptr || v == nullptr ||
            width <= 0 || height <= 0 || rtv == nullptr) {
            return false;
        }
        if (!EnsurePipeline()) {
            return false;
        }
        if (target_width <= 0 || target_height <= 0) {
            target_width = width;
            target_height = height;
        }

        const int chroma_width = (width + 1) / 2;
        const int chroma_height = (height + 1) / 2;
        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> srv_y;
        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> srv_u;
        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> srv_v;
        if (!CreateR8Texture(y, y_stride, width, height, srv_y.GetAddressOf()) ||
            !CreateR8Texture(u, u_stride, chroma_width, chroma_height, srv_u.GetAddressOf()) ||
            !CreateR8Texture(v, v_stride, chroma_width, chroma_height, srv_v.GetAddressOf())) {
            return false;
        }

        const float clear[4] = {0.0f, 0.0f, 0.0f, 1.0f};
        context_->ClearRenderTargetView(rtv, clear);

        const D3D11_VIEWPORT viewport = FitViewport(width, height, target_width, target_height);
        context_->RSSetViewports(1, &viewport);
        ID3D11RenderTargetView* rtvs[1] = {rtv};
        context_->OMSetRenderTargets(1, rtvs, nullptr);
        context_->IASetInputLayout(nullptr);
        context_->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        context_->VSSetShader(vertex_shader_.Get(), nullptr, 0);
        context_->PSSetShader(pixel_shader_.Get(), nullptr, 0);
        ID3D11ShaderResourceView* srvs[3] = {srv_y.Get(), srv_u.Get(), srv_v.Get()};
        context_->PSSetShaderResources(0, 3, srvs);
        ID3D11SamplerState* samplers[1] = {sampler_.Get()};
        context_->PSSetSamplers(0, 1, samplers);
        context_->Draw(3, 0);
        return true;
    }

    // The frame shown whole and undistorted inside the target: centred, scaled to the smaller of the
    // two ratios, black bars (the cleared target) on the long sides. Until 2026-10-05 the frame was
    // stretched over the whole target, so a screen of another shape than the window came out
    // squashed. A target of the frame's own shape (every readback path) gets the full target.
    // The shell maps a viewer's pointer by the same rule (MainWindow.ScreenControl.cs,
    // MapToSharedScreen) — change both together.
    static D3D11_VIEWPORT FitViewport(int width, int height, int target_width, int target_height)
    {
        D3D11_VIEWPORT viewport = {};
        const float scale = (std::min)(
            static_cast<float>(target_width) / static_cast<float>(width),
            static_cast<float>(target_height) / static_cast<float>(height));
        viewport.Width = static_cast<float>(width) * scale;
        viewport.Height = static_cast<float>(height) * scale;
        viewport.TopLeftX = (static_cast<float>(target_width) - viewport.Width) / 2.0f;
        viewport.TopLeftY = (static_cast<float>(target_height) - viewport.Height) / 2.0f;
        viewport.MaxDepth = 1.0f;
        return viewport;
    }

    bool CreateR8Texture(
        const uint8_t* data, int stride, int width, int height,
        ID3D11ShaderResourceView** out_srv)
    {
        if (stride < width) {
            return false;
        }
        D3D11_TEXTURE2D_DESC desc = {};
        desc.Width = static_cast<UINT>(width);
        desc.Height = static_cast<UINT>(height);
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_R8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.Usage = D3D11_USAGE_IMMUTABLE;
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        D3D11_SUBRESOURCE_DATA init = {};
        init.pSysMem = data;
        init.SysMemPitch = static_cast<UINT>(stride);
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        if (FAILED(device_->CreateTexture2D(&desc, &init, texture.GetAddressOf()))) {
            return false;
        }
        return SUCCEEDED(device_->CreateShaderResourceView(texture.Get(), nullptr, out_srv));
    }

    bool EnsurePipeline()
    {
        if (vertex_shader_ && pixel_shader_ && sampler_) {
            return true;
        }

        static const char kShader[] =
            "Texture2D<float> texY : register(t0);\n"
            "Texture2D<float> texU : register(t1);\n"
            "Texture2D<float> texV : register(t2);\n"
            "SamplerState smp : register(s0);\n"
            "struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };\n"
            "VSOut VSMain(uint id : SV_VertexID) {\n"
            "  VSOut o;\n"
            "  float2 uv = float2((id << 1) & 2, id & 2);\n"
            "  o.uv = uv;\n"
            "  o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);\n"
            "  return o;\n"
            "}\n"
            "float4 PSMain(VSOut i) : SV_TARGET {\n"
            "  float y = texY.Sample(smp, i.uv);\n"
            "  float u = texU.Sample(smp, i.uv) - 0.501961;\n"
            "  float v = texV.Sample(smp, i.uv) - 0.501961;\n"
            "  float yy = 1.164 * (y - 0.062745);\n"
            "  float r = yy + 1.596 * v;\n"
            "  float g = yy - 0.391 * u - 0.813 * v;\n"
            "  float b = yy + 2.018 * u;\n"
            "  return float4(saturate(r), saturate(g), saturate(b), 1.0);\n"
            "}\n";

        Microsoft::WRL::ComPtr<ID3DBlob> vs_blob;
        Microsoft::WRL::ComPtr<ID3DBlob> ps_blob;
        Microsoft::WRL::ComPtr<ID3DBlob> errors;
        if (FAILED(::D3DCompile(kShader, sizeof(kShader) - 1, nullptr, nullptr, nullptr,
                                "VSMain", "vs_5_0", 0, 0, vs_blob.GetAddressOf(), errors.GetAddressOf()))) {
            return false;
        }
        if (FAILED(::D3DCompile(kShader, sizeof(kShader) - 1, nullptr, nullptr, nullptr,
                                "PSMain", "ps_5_0", 0, 0, ps_blob.GetAddressOf(), errors.ReleaseAndGetAddressOf()))) {
            return false;
        }
        if (FAILED(device_->CreateVertexShader(
                vs_blob->GetBufferPointer(), vs_blob->GetBufferSize(), nullptr, vertex_shader_.GetAddressOf()))) {
            return false;
        }
        if (FAILED(device_->CreatePixelShader(
                ps_blob->GetBufferPointer(), ps_blob->GetBufferSize(), nullptr, pixel_shader_.GetAddressOf()))) {
            return false;
        }

        D3D11_SAMPLER_DESC sampler_desc = {};
        sampler_desc.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
        sampler_desc.AddressU = D3D11_TEXTURE_ADDRESS_CLAMP;
        sampler_desc.AddressV = D3D11_TEXTURE_ADDRESS_CLAMP;
        sampler_desc.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
        sampler_desc.MaxLOD = D3D11_FLOAT32_MAX;
        return SUCCEEDED(device_->CreateSamplerState(&sampler_desc, sampler_.GetAddressOf()));
    }

    // Task #84 increment 2: draw an NV12 texture (BT.601 NV12->RGB shader) to `rtv`, fitted whole into
    // target_width x target_height. Two plane SRVs over the same NV12 texture: Y via R8_UNORM, UV
    // via R8G8_UNORM. HW-decoder output is typically a texture ARRAY — select the slice with
    // FirstArraySlice = subresource. The NV12 texture must carry D3D11_BIND_SHADER_RESOURCE
    // (the caller copies to an SRV-capable NV12 texture first when the decoder output lacks it).
    bool DrawNv12(
        ID3D11Texture2D* nv12, UINT subresource, int width, int height,
        ID3D11RenderTargetView* rtv, int target_width = 0, int target_height = 0)
    {
        if (!device_ || !context_ || nv12 == nullptr || width <= 0 || height <= 0 || rtv == nullptr) {
            return false;
        }
        if (!EnsureNv12PixelShader()) {
            return false;
        }
        if (target_width <= 0 || target_height <= 0) {
            target_width = width;
            target_height = height;
        }

        D3D11_TEXTURE2D_DESC desc = {};
        nv12->GetDesc(&desc);
        const bool is_array = desc.ArraySize > 1;

        D3D11_SHADER_RESOURCE_VIEW_DESC srv_y_desc = {};
        srv_y_desc.Format = DXGI_FORMAT_R8_UNORM;
        D3D11_SHADER_RESOURCE_VIEW_DESC srv_uv_desc = {};
        srv_uv_desc.Format = DXGI_FORMAT_R8G8_UNORM;
        if (is_array) {
            srv_y_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
            srv_y_desc.Texture2DArray.FirstArraySlice = subresource;
            srv_y_desc.Texture2DArray.ArraySize = 1;
            srv_y_desc.Texture2DArray.MipLevels = 1;
            srv_uv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
            srv_uv_desc.Texture2DArray.FirstArraySlice = subresource;
            srv_uv_desc.Texture2DArray.ArraySize = 1;
            srv_uv_desc.Texture2DArray.MipLevels = 1;
        } else {
            srv_y_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
            srv_y_desc.Texture2D.MipLevels = 1;
            srv_uv_desc.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
            srv_uv_desc.Texture2D.MipLevels = 1;
        }

        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> srv_y;
        Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> srv_uv;
        if (FAILED(device_->CreateShaderResourceView(nv12, &srv_y_desc, srv_y.GetAddressOf())) ||
            FAILED(device_->CreateShaderResourceView(nv12, &srv_uv_desc, srv_uv.GetAddressOf()))) {
            return false;
        }

        const float clear[4] = {0.0f, 0.0f, 0.0f, 1.0f};
        context_->ClearRenderTargetView(rtv, clear);
        const D3D11_VIEWPORT viewport = FitViewport(width, height, target_width, target_height);
        context_->RSSetViewports(1, &viewport);
        ID3D11RenderTargetView* rtvs[1] = {rtv};
        context_->OMSetRenderTargets(1, rtvs, nullptr);
        context_->IASetInputLayout(nullptr);
        context_->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        context_->VSSetShader(vertex_shader_.Get(), nullptr, 0);
        context_->PSSetShader(nv12_pixel_shader_.Get(), nullptr, 0);
        ID3D11ShaderResourceView* srvs[2] = {srv_y.Get(), srv_uv.Get()};
        context_->PSSetShaderResources(0, 2, srvs);
        ID3D11SamplerState* samplers[1] = {sampler_.Get()};
        context_->PSSetSamplers(0, 1, samplers);
        context_->Draw(3, 0);
        // Unbind the SRVs — the input NV12 texture may be reused by the decoder as an output slot.
        ID3D11ShaderResourceView* null_srvs[2] = {nullptr, nullptr};
        context_->PSSetShaderResources(0, 2, null_srvs);
        return true;
    }

    bool EnsureNv12PixelShader()
    {
        if (!EnsurePipeline()) {  // shares the fullscreen-triangle VS + the linear sampler
            return false;
        }
        if (nv12_pixel_shader_) {
            return true;
        }
        // BT.601 NV12 -> RGB. VSOut semantics (SV_POSITION + TEXCOORD0) match the shared VSMain.
        static const char kNv12Shader[] =
            "Texture2D<float> texY : register(t0);\n"
            "Texture2D<float2> texUV : register(t1);\n"
            "SamplerState smp : register(s0);\n"
            "struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };\n"
            "float4 PSMain(VSOut i) : SV_TARGET {\n"
            "  float y = texY.Sample(smp, i.uv);\n"
            "  float2 c = texUV.Sample(smp, i.uv) - 0.501961;\n"
            "  float yy = 1.164 * (y - 0.062745);\n"
            "  float r = yy + 1.596 * c.y;\n"
            "  float g = yy - 0.391 * c.x - 0.813 * c.y;\n"
            "  float b = yy + 2.018 * c.x;\n"
            "  return float4(saturate(r), saturate(g), saturate(b), 1.0);\n"
            "}\n";
        Microsoft::WRL::ComPtr<ID3DBlob> ps_blob;
        Microsoft::WRL::ComPtr<ID3DBlob> errors;
        if (FAILED(::D3DCompile(kNv12Shader, sizeof(kNv12Shader) - 1, nullptr, nullptr, nullptr,
                                "PSMain", "ps_5_0", 0, 0, ps_blob.GetAddressOf(), errors.GetAddressOf()))) {
            return false;
        }
        return SUCCEEDED(device_->CreatePixelShader(
            ps_blob->GetBufferPointer(), ps_blob->GetBufferSize(), nullptr, nv12_pixel_shader_.GetAddressOf()));
    }

    Microsoft::WRL::ComPtr<ID3D11Device> device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11VertexShader> vertex_shader_;
    Microsoft::WRL::ComPtr<ID3D11PixelShader> pixel_shader_;
    Microsoft::WRL::ComPtr<ID3D11PixelShader> nv12_pixel_shader_;
    Microsoft::WRL::ComPtr<ID3D11SamplerState> sampler_;
    Microsoft::WRL::ComPtr<IDXGISwapChain1> swap_chain_;
    int swap_chain_width_ = 0;
    int swap_chain_height_ = 0;
    // Task #87: PCI vendor of the GPU the user picked for the receive path (0 = Auto). Must be set
    // before Initialize()/the first swapchain create, as Initialize() is idempotent and caches the
    // device. Honoured first in Initialize()'s adapter fallback chain.
    UINT preferred_adapter_vendor_id_ = 0;
};

}  // namespace omni_media

#endif  // _WIN32
