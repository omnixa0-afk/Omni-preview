using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// COM interop for binding a native DXGI swapchain to a WinUI 3 <c>SwapChainPanel</c>
/// (#5 increment 3b, 2026-06-05 — GPU video render path).
/// </summary>
/// <remarks>
/// The IID (<c>63aad0b8-7c24-40ff-85a8-640d944cc325</c>) comes from the Windows App SDK header
/// <c>microsoft.ui.xaml.media.dxinterop.h</c> — the WinUI 3 <c>ISwapChainPanelNative</c>, obtained
/// via <c>swapChainPanel.As&lt;ISwapChainPanelNative&gt;()</c>. <see cref="SetSwapChain"/> takes the
/// raw <c>IDXGISwapChain*</c> the native renderer hands back (already AddRef'd); the panel AddRefs
/// it internally, so the caller must <c>Marshal.Release</c> its own reference afterward.
/// </remarks>
[ComImport]
[Guid("63aad0b8-7c24-40ff-85a8-640d944cc325")]
[InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
internal interface ISwapChainPanelNative
{
    [PreserveSig]
    int SetSwapChain(IntPtr swapChain);
}
