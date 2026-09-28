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
//! A `topmost = false` target does land behind a window whose content is a topmost one. That is the
//! one assumption this arrangement makes, and the whole of the canvas depends on it: the interface
//! is GPUI's and the canvas is this layer's, in the one window both were made for.
//!
//! ## A composition swap chain's three requirements
//!
//! Premultiplied alpha, stretch scaling, and no zero-sized buffer. They are requirements of the
//! API rather than choices made here, so they are pinned in one place — [`swap_chain_desc`] — and
//! by a test, rather than spread over a call site where one of them could be dropped in passing.
//!
//! ## What is drawn with it
//!
//! Not this module's business: it makes the device, the swap chain and the visual, and hands the
//! current buffer to whatever draws a frame (see [`crate::ink_layer::render`]).

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::Interface as _;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::{
    Common::{
        DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
        DXGI_SAMPLE_DESC,
    },
    CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, IDXGISwapChain2,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use super::canvas::Canvas;
use super::render::Renderer;

/// How many buffers the swap chain is made with.
///
/// Two, so that the frame being presented is not the one being drawn: a swap chain with one buffer
/// makes the two the same and tears.
const BUFFERS: u32 = 2;

/// The canvas's device, and everything that has to outlive it: the swap chain it presents, the
/// renderer that draws into it, and the composition that shows it.
pub(crate) struct Device {
    /// The window the canvas is composed into.
    hwnd: HWND,
    /// Held, not read: the context, the renderer and the swap chain are all made from it, and the
    /// canvas is theirs for as long as it is on the screen.
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    /// What draws a frame: Direct2D, on this same device (see [`Renderer`]).
    renderer: Renderer,
    /// The client area the buffers were made for, in physical pixels.
    size: (u32, u32),
    /// Held, not read: dropping the target or the visual would take the canvas off the screen.
    _composition: Composition,
    /// The compositor's own clock: whether it has taken the frame it was given last (see
    /// [`FrameWait`]).
    frame: FrameWait,
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
        let (swap_chain, frame) = create_swap_chain(&factory, &device, size)
            .context("creating a composition swap chain")?;
        let dxgi_device: IDXGIDevice = device.cast().context("taking the device's DXGI side")?;
        let renderer = Renderer::new(&dxgi_device).context("making the canvas's renderer")?;
        let composition = Composition::new(hwnd, &swap_chain, &dxgi_device)
            .context("composing the canvas under the window's content")?;

        Ok(Device {
            hwnd,
            _device: device,
            context,
            swap_chain,
            renderer,
            size,
            _composition: composition,
            frame,
        })
    }

    /// Draws a frame of the canvas and presents it, reporting whether it reached the screen.
    ///
    /// Nothing is drawn while the compositor is still holding the frame before it: drawing one is a
    /// whole surface's work — the desk, the sheet, its ruling, the page and every stroke on it — and
    /// a canvas presented too soon is a canvas the compositor drops. A canvas that arrives too soon
    /// is not lost: whichever wake called this will call it again in a few milliseconds, with more
    /// ink on it than this one had (see [`crate::app`]).
    pub(crate) fn render(&mut self, canvas: &Canvas) -> Result<bool> {
        if !self.frame.ready() {
            return Ok(false);
        }

        self.resize()?;

        let buffer: IDXGISurface = unsafe { self.swap_chain.GetBuffer(0)? };
        self.renderer.draw(buffer, canvas)?;

        unsafe {
            // Direct2D and this device share one immediate context, so what Direct2D has just
            // ended is still a batch on it: the flush is what hands the frame over, and a present
            // is not promised to wait for work that has not been handed over.
            self.context.Flush();

            // No interval: the canvas is presented as soon as it is drawn, and nothing here waits
            // for a display's clock. What paces it is a decision of its own (see [`super`]).
            self.swap_chain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .context("presenting the canvas")?;
        }

        Ok(true)
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

        // Nothing is holding a buffer: the renderer takes one per frame and lets it go before the
        // frame ends, so a resize never finds one alive (see [`Renderer::draw`]).
        let (width, height) = (size.0.max(1), size.1.max(1));
        // The context holds the frame it last drew, and a chain cannot be resized while anything
        // holds its back buffers (see `IDXGISwapChain::ResizeBuffers`): the target is let go first.
        self.renderer.release_target();

        // The flags are the frame clock's own: a resize *resets* a chain's flags rather than keeping
        // them, and the clock it takes away is the only signal that says a frame has been taken — a
        // canvas that never draws again is what that costs (see [`FrameWait::flags`]).
        let flags = self.frame.flags();
        let mut resized = unsafe {
            self.swap_chain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, flags)
        };

        if resized.is_err() && self.frame.is_paced() {
            // A driver that will not take the clock's flag through a resize gets the resize without
            // it, and the canvas is drawn unpaced from here on: slower, and still a canvas (see
            // [`FrameWait`]). Without this it would be a resize that fails on every frame, which is
            // a canvas frozen behind a message.
            self.frame.forget();
            resized = unsafe {
                self.swap_chain.ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
            };
        }

        resized.context("resizing the canvas")?;

        // And the latency, if the clock is still there: a resize is a new chain in everything but
        // name, and there is no promise that it keeps this.
        if self.frame.is_paced() {
            let chain: IDXGISwapChain2 = self.swap_chain.cast()?;
            pace(&chain)?;
        }

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
        // what makes the canvas a canvas rather than an overlay — see the module docs.
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

/// The compositor's own clock: a handle that says the frame it was given last has been taken, or
/// nothing at all on a chain that could not be made with one.
///
/// It gives itself up if it never answers. A handle that stopped signalling would stop the canvas
/// being drawn *at all*, for as long as the window is open — which is what a resize did before the
/// chain's flags were carried through it (see [`FrameWait::flags`]) — so [`FrameWait::ready`] stops
/// asking after [`FRAME_WAIT_PATIENCE`] and the canvas is drawn unpaced from then on: a frame the
/// compositor drops is a far cheaper mistake than a canvas that never updates.
struct FrameWait {
    /// The handle, or none when the chain has no clock.
    handle: Option<HANDLE>,
    /// When it was last seen ready, which is what the patience above is measured from.
    seen: Instant,
}

/// How long a frame clock may go without answering before it is given up on.
///
/// A clock is answered by the compositor at the display's own rate, so a fifth of a second is a
/// couple of dozen vblanks — long past the point where the answer is "this handle will never signal".
const FRAME_WAIT_PATIENCE: Duration = Duration::from_millis(200);

impl FrameWait {
    /// A clock for a chain that has none.
    fn unpaced() -> Self {
        FrameWait {
            handle: None,
            seen: Instant::now(),
        }
    }

    /// Whether the compositor is ready for another frame.
    ///
    /// A zero timeout, because the answer wanted here is "now or at the next wake": the canvas is
    /// drawn again by the pen's pump a few milliseconds later, and waiting here would hold that wake
    /// instead of drawing it (see [`Device::render`]).
    ///
    /// A chain made without the handle is always ready: the compositor drops whatever it cannot
    /// show, which is what it did before the handle existed — and so is one whose handle has stopped
    /// answering (see this struct's own note).
    fn ready(&mut self) -> bool {
        let Some(handle) = self.handle else {
            return true;
        };

        if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
            self.seen = Instant::now();

            return true;
        }

        if self.seen.elapsed() > FRAME_WAIT_PATIENCE {
            self.forget();

            return true;
        }

        false
    }

    /// Gives up the clock: this chain is drawn unpaced from here on.
    ///
    /// The handle is the app's to close (see `GetFrameLatencyWaitableObject`), and the chain goes on
    /// working without anyone waiting on it — which is what "unpaced" means here.
    fn forget(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe { CloseHandle(handle) }.ok();
        }
    }

    /// Whether this chain has a clock at all.
    fn is_paced(&self) -> bool {
        self.handle.is_some()
    }

    /// The swap chain flags that go with this clock.
    ///
    /// A resize *resets* a chain's flags rather than keeping them, so the one that made this waitable
    /// has to be passed again on every resize: lose it and the chain loses its clock, and with the
    /// clock the only signal that says a frame has been taken — which is a canvas that stops drawing
    /// the moment the window is resized (see [`Device::resize`]).
    fn flags(&self) -> DXGI_SWAP_CHAIN_FLAG {
        match self.handle {
            Some(_) => DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            None => DXGI_SWAP_CHAIN_FLAG(0),
        }
    }
}

impl Drop for FrameWait {
    fn drop(&mut self) {
        self.forget();
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

/// A swap chain that DirectComposition can show, and its frame clock.
///
/// Made with the clock if the system takes it there — see [`FrameWait`] — and without it if it does
/// not: a canvas that cannot be paced is still a canvas, and refusing to draw ink because a present
/// cannot be timed would be a worse trade than the one presentation the missing argument costs.
fn create_swap_chain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    size: (u32, u32),
) -> Result<(IDXGISwapChain1, FrameWait)> {
    let desc = swap_chain_desc(size);

    if let Ok(chain) = unsafe { factory.CreateSwapChainForComposition(device, &desc, None) } {
        // A chain that cannot hand over its clock is still a chain: being paced is a favour the
        // compositor does, not a requirement of a canvas (see [`FrameWait`]).
        let frame = frame_wait(&chain).unwrap_or_else(|_| FrameWait::unpaced());

        return Ok((chain, frame));
    }

    let desc = DXGI_SWAP_CHAIN_DESC1 { Flags: 0, ..desc };
    let chain = unsafe { factory.CreateSwapChainForComposition(device, &desc, None) }
        .context("creating a composition swap chain")?;

    Ok((chain, FrameWait::unpaced()))
}

/// The frame clock of a chain, taken from it and set to one frame of latency.
fn frame_wait(swap_chain: &IDXGISwapChain1) -> Result<FrameWait> {
    let chain: IDXGISwapChain2 = swap_chain
        .cast()
        .context("the swap chain's frame latency interface")?;
    pace(&chain)?;

    Ok(FrameWait {
        handle: Some(unsafe { chain.GetFrameLatencyWaitableObject() }),
        seen: Instant::now(),
    })
}

/// One frame of latency on a chain that has a clock.
///
/// One, because that is how many frames are being shown: more would let the canvas run ahead of the
/// screen, which is the thing being avoided rather than a buffer to be filled. Set again after a
/// resize, because a resize is a new chain in everything but name and there is no promise that it
/// keeps this.
fn pace(chain: &IDXGISwapChain2) -> Result<()> {
    unsafe { chain.SetMaximumFrameLatency(1) }.context("setting the frame latency")
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
        // The compositor's clock, handed out as a waitable: without it, a present is the only way to
        // ask whether the compositor has room for another frame, and there is no way to ask *before*
        // a surface's worth of drawing has been spent on one it will drop (see [`FrameWait`]).
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
    }
}

/// A view of the swap chain's current back buffer, to clear and draw into.
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
        assert_eq!(
            desc.Flags, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            "the compositor's frame clock, which is what a present is paced by"
        );
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
    /// assumption the arrangement makes, and every screenshot of the app is what looks at it.
    #[test]
    fn a_canvas_is_installed_on_a_window_that_is_never_shown() {
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

        let hwnd = a_window();

        {
            let mut device = Device::new(hwnd).expect("a canvas for that window");
            device
                .render(&Canvas::default())
                .expect("a frame of it");
        }

        unsafe { DestroyWindow(hwnd).expect("the window to close") };
    }

    /// The window the canvas's own tests draw into.
    fn a_window() -> HWND {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WS_OVERLAPPED};

        unsafe {
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
        .expect("a window to draw in")
    }

    /// A resize keeps the frame clock.
    ///
    /// A resize is what a window is always doing — every drag of its edge, every maximize — and
    /// `ResizeBuffers` *resets* a chain's flags rather than keeping them. Losing this one costs the
    /// waitable that says a frame has been taken, and with it every frame after the resize: the
    /// canvas stops drawing, and what the window then shows is a back buffer the resize left
    /// undefined. That is a canvas that disappears rather than one that stutters, so it is a bug
    /// worth a test that runs on a real device.
    #[test]
    fn a_resize_keeps_the_frame_clock() {
        use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, MoveWindow};

        let hwnd = a_window();

        {
            let mut device = Device::new(hwnd).expect("a canvas for that window");

            // The window changes size under the canvas, as it does when a user maximizes it.
            unsafe { MoveWindow(hwnd, 0, 0, 400, 300, false) }
                .expect("the window to change size");

            device.render(&Canvas::default()).expect("a frame of it");
            // The canvas follows the window's *client* area — its size less whatever the window's own
            // frame takes — so what is asserted is that it left the size it was made for, and that it
            // is the client area it left it for.
            assert_ne!(
                device.size,
                (320, 240),
                "the canvas followed the window's new size"
            );
            assert_eq!(
                device.size,
                client_size(hwnd).expect("the window's client area"),
                "and it is the client area that it followed"
            );

            let desc = unsafe { device.swap_chain.GetDesc() }.expect("the chain's description");
            assert_eq!(
                desc.Flags,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
                "and the frame clock survived it"
            );
        }

        unsafe { DestroyWindow(hwnd).expect("the window to close") };
    }
}
