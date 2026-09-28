//! The device the canvas is drawn with: a Direct3D 11 device of the app's own, a swap chain it
//! presents, and the composition visual that puts it on the window.
//!
//! ## Why a second visual, and not a second window
//!
//! GPUI draws the window into a swap chain of its own and hands it to DirectComposition as the
//! **topmost** visual of a target made for the window handle. Its window carries
//! `WS_EX_NOREDIRECTIONBITMAP`, so that visual is the whole of the window's content: there is no
//! redirection surface left for anything to draw under it through.
//!
//! A second target for the same handle, made with `topmost = false`, is drawn *behind* that
//! content. That is what lets this app keep GPUI for the interface and draw the canvas with an
//! engine of its own in the one window both were made for — no second window to keep in step, and
//! therefore no z-order to chase, nothing to re-position on a move, a resize, a DPI change, a
//! minimize, or a trip through the task bar.
//!
//! Whether a `topmost = false` target really lands behind a window whose content is a topmost one
//! is what the probe below exists to look at. It is the one assumption this arrangement makes.
//!
//! ## A composition swap chain's three requirements
//!
//! Premultiplied alpha, stretch scaling, and no zero-sized buffer. They are requirements of the
//! API rather than choices made here, so they are pinned in one place — [`swap_chain_desc`] — and
//! by a test, rather than spread over a call site where one of them could be dropped in passing.
//!
//! ## What this phase draws
//!
//! An opaque red rectangle over the whole client area, presented every frame. It is not the canvas
//! and it is not meant to look like anything: it answers whether the visual is behind GPUI's
//! content, and whether a window whose background appearance is `Transparent` lets it show through
//! where GPUI paints nothing.

use anyhow::{anyhow, Context, Result};
use windows::core::Interface as _;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{
    Common::{
        DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
        DXGI_SAMPLE_DESC,
    },
    CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

/// How many buffers the swap chain is made with.
///
/// Two, so that the frame being presented is not the one being drawn: a swap chain with one buffer
/// makes the two the same and tears.
const BUFFERS: u32 = 2;

/// The colour the probe fills the canvas with. Opaque, so that nothing behind it can be mistaken
/// for it.
const PROBE_COLOR: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

/// The canvas's device, and everything that has to outlive it: the swap chain it presents, the
/// view of the buffer it clears, and the composition that shows it.
pub(crate) struct Device {
    /// The window the canvas is composed into.
    hwnd: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    /// The view of the swap chain's current back buffer.
    ///
    /// `None` only while the buffers are being resized: a view holds a reference to a buffer, and
    /// DXGI refuses to resize while one is alive.
    view: Option<ID3D11RenderTargetView>,
    /// The client area the buffers were made for, in physical pixels.
    size: (u32, u32),
    /// Held, not read: dropping the target or the visual would take the canvas off the screen.
    _composition: Composition,
}

impl Device {
    /// A device of its own, a swap chain for the window's client area, and the visual that shows it
    /// under GPUI's own content.
    ///
    /// The adapter is the system's default *hardware* one: this app is written for a machine with a
    /// GPU and does not fall back to software rendering (see [`crate::ink_layer`]).
    pub(crate) fn new(hwnd: HWND) -> Result<Self> {
        let (device, context) = create_device().context("creating a Direct3D 11 device")?;
        let factory: IDXGIFactory2 =
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }
                .context("creating a DXGI factory")?;
        let size = client_size(hwnd).context("measuring the window")?;
        let swap_chain = create_swap_chain(&factory, &device, size)
            .context("creating a composition swap chain")?;
        let view = create_view(&device, &swap_chain).context("making a render target")?;
        let dxgi_device: IDXGIDevice = device.cast().context("taking the device's DXGI side")?;
        let composition = Composition::new(hwnd, &swap_chain, &dxgi_device)
            .context("composing the canvas under the window's content")?;

        Ok(Device {
            hwnd,
            device,
            context,
            swap_chain,
            view: Some(view),
            size,
            _composition: composition,
        })
    }

    /// Fills the canvas with the probe colour and presents it.
    pub(crate) fn probe(&mut self) -> Result<()> {
        self.resize()?;

        let view = self
            .view
            .as_ref()
            .ok_or_else(|| anyhow!("the swap chain has no back buffer"))?;

        unsafe {
            self.context.ClearRenderTargetView(view, &PROBE_COLOR);
            // No interval: the probe is presented as soon as it is drawn, and nothing here waits
            // for a display's clock. What paces the finished canvas is a decision of its own phase.
            self.swap_chain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .context("presenting the canvas")?;
        }

        Ok(())
    }

    /// Resizes the swap chain when the window's client area has changed under it.
    ///
    /// A resize is not rare — every drag of an edge is one — so it is checked here rather than
    /// arranged for elsewhere, where a missed message would leave the canvas at the wrong size
    /// until something else happened to fix it.
    fn resize(&mut self) -> Result<()> {
        let size = client_size(self.hwnd)?;
        if size == self.size {
            return Ok(());
        }

        // The view goes first: DXGI refuses to resize buffers while a reference to one is alive.
        self.view = None;

        let (width, height) = (size.0.max(1), size.1.max(1));
        unsafe {
            // A buffer count of zero keeps the count the chain was made with.
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }

        self.view = Some(create_view(&self.device, &self.swap_chain)?);
        self.size = size;

        Ok(())
    }
}

/// The visual the swap chain is shown through, and the target that places it under the window's own
/// content.
struct Composition {
    /// Held, not read: each of the three keeps the next alive, and the last keeps the canvas on the
    /// screen.
    _device: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl Composition {
    fn new(hwnd: HWND, swap_chain: &IDXGISwapChain1, dxgi_device: &IDXGIDevice) -> Result<Self> {
        let device: IDCompositionDevice = unsafe { DCompositionCreateDevice(dxgi_device) }?;

        // `false`: behind the window's own content, which is GPUI's visual. This one argument is
        // what the probe exists to confirm — see the module docs.
        let target = unsafe { device.CreateTargetForHwnd(hwnd, false) }?;
        let visual = unsafe { device.CreateVisual() }?;

        unsafe {
            visual.SetContent(swap_chain)?;
            target.SetRoot(&visual)?;
            device.Commit()?;
        }

        Ok(Composition {
            _device: device,
            _target: target,
            _visual: visual,
        })
    }
}

/// A Direct3D 11 device on the system's default hardware adapter, and its immediate context.
///
/// `BGRA_SUPPORT` is asked for because every surface in this app is BGRA: the page Pdfium renders,
/// and the swap chain a composition visual takes.
///
/// No feature level is requested, and no software driver is named: Direct3D hands back the best
/// hardware device the machine has, and a machine without one gets an error here rather than a
/// canvas drawn on the processor (see [`crate::ink_layer`]).
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    /// What a Direct3D 11 device can be: 11.1 where the driver has it, and 10.1 at the floor, which
    /// is the oldest that still supports the swap chain model used here.
    const LEVELS: [windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL; 3] = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
    ];

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&LEVELS),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }

    let device = device.ok_or_else(|| anyhow!("Direct3D made no device"))?;
    let context = context.ok_or_else(|| anyhow!("Direct3D made no context"))?;

    Ok((device, context))
}

/// A swap chain that DirectComposition can show.
fn create_swap_chain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    size: (u32, u32),
) -> Result<IDXGISwapChain1> {
    let desc = swap_chain_desc(size);

    Ok(unsafe { factory.CreateSwapChainForComposition(device, &desc, None)? })
}

/// The description a composition swap chain has to be made with.
///
/// Split out from the call because three of these fields are requirements rather than preferences:
/// a composition swap chain takes premultiplied alpha (the compositor blends it into the window),
/// stretch scaling (the only scaling it accepts), and at least one buffer of a non-zero size.
fn swap_chain_desc(size: (u32, u32)) -> DXGI_SWAP_CHAIN_DESC1 {
    let (width, height) = (size.0.max(1), size.1.max(1));

    DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFERS,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    }
}

/// A view of the swap chain's current back buffer, to clear and draw into.
fn create_view(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView> {
    let buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0)? };

    let mut view: Option<ID3D11RenderTargetView> = None;
    unsafe { device.CreateRenderTargetView(&buffer, None, Some(&mut view))? };

    view.ok_or_else(|| anyhow!("Direct3D made no render target view"))
}

/// The window's client area, in physical pixels.
///
/// The client area rather than the window's bounds: the visual is composed into the client area, so
/// the frame and its shadow are the system's business and not this canvas's.
fn client_size(hwnd: HWND) -> Result<(u32, u32)> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect)? };

    Ok((
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three requirements of a composition swap chain, and the size the chain is made for.
    ///
    /// These are the fields whose loss is neither a compile error nor an error the API reports
    /// either: a chain made with `DXGI_ALPHA_MODE_IGNORE` draws as an opaque black rectangle over
    /// the window instead of letting the interface behind it through.
    #[test]
    fn a_composition_swap_chain_is_made_the_way_the_compositor_requires() {
        let desc = swap_chain_desc((1_280, 900));

        assert_eq!(desc.AlphaMode, DXGI_ALPHA_MODE_PREMULTIPLIED, "alpha mode");
        assert_eq!(desc.Scaling, DXGI_SCALING_STRETCH, "scaling");
        assert_eq!(
            desc.SwapEffect,
            DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            "swap effect"
        );
        assert_eq!(desc.BufferCount, BUFFERS, "buffer count");
        assert_eq!(desc.SampleDesc.Count, 1, "sample count");
        assert_eq!((desc.Width, desc.Height), (1_280, 900), "size");
        assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM, "format");
    }

    /// A window that has not been laid out yet reports a client area of nothing, and a swap chain
    /// made one pixel wide is accepted where one made zero pixels wide is refused.
    #[test]
    fn a_size_of_nothing_becomes_one_pixel() {
        let desc = swap_chain_desc((0, 0));

        assert_eq!((desc.Width, desc.Height), (1, 1));
    }

    /// The whole of what the canvas needs from the platform, made for a window of the test's own:
    /// a device, a composition swap chain, a render target, and a visual on that window — drawn in
    /// and presented.
    ///
    /// The window is never shown and is a stock one (`STATIC`), because what is being checked is
    /// that every piece can be *made* on this machine at all. It is the failure worth catching
    /// here rather than from a red rectangle that did not appear: a driver that will not give
    /// Direct3D 11 a device, or a session that will not compose, is not something a screenshot of
    /// the app says out loud.
    ///
    /// Whether the visual lands *behind* GPUI's is not this test's business. That is the one
    /// assumption the arrangement makes, and the probe is what looks at it.
    #[test]
    fn a_canvas_is_installed_on_a_window_that_is_never_shown() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_OVERLAPPED,
        };

        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("cheap-note canvas"),
                WS_OVERLAPPED,
                0,
                0,
                320,
                240,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a window to draw in");

        {
            let mut device = Device::new(hwnd).expect("a canvas for that window");
            device.probe().expect("a frame of it");
        }

        unsafe { DestroyWindow(hwnd).expect("the window to close") };
    }
}
