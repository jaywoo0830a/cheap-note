//! The canvas's own renderer: the desk, the paper and the ink, drawn outside GPUI's frame.
//!
//! ## Why the canvas left the frame
//!
//! A frame in GPUI is a *submission*: every element's paint callback runs, every primitive is
//! batched, every path is tessellated, and the renderer throws all of it away and builds it again
//! on the next one. For an interface that is the right trade — an interface's contents are a
//! handful of rectangles and a line of text. For a page of handwriting it is the wrong one, and the
//! measurements say so: a page with 370 strokes holds ~33 fps with `paint` at 9.3 ms, while an
//! empty one holds 143 fps with `paint` at 0.0. The per-frame work grows with the ink on the page,
//! and it grows *three* times over: the app builds every stroke's polygon again, the engine
//! tessellates it again, and the engine re-uploads the vertices and pays a window-sized
//! multisampled pass for that batch — per batch, per frame.
//!
//! Caching the scene in GPUI does not help, and that was measured too: `paint` falls to `0.00 ms`
//! with a cached layer and the frame rate stays where it was, because the engine still replays and
//! re-uploads everything the cache hands it. The cost is not in the app's paint callback, so it is
//! not something a paint callback can avoid.
//!
//! ## What runs here instead
//!
//! A renderer of the app's own, in a window it shares with the interface rather than a window of
//! its own: the canvas is a swap chain of ours, placed by DirectComposition *behind* the visual
//! GPUI draws the interface into (see [`device`] for why that works and how little it costs). GPUI
//! keeps everything above the canvas — the bar, the pills, the lists, the dialogs, the keyboard,
//! the pointer — and this module keeps what is on the desk.
//!
//! Nothing about the ink is re-submitted per frame: a stroke is built once, when it is closed, and
//! what changes while a page is being written on is one stroke's worth of geometry (see the phases
//! in the project's plan). That is the whole of the fix: the frame no longer hears about a page of
//! handwriting at all.
//!
//! ## A GPU is required, and software rendering is not a fallback
//!
//! This is a note app for machines with a graphics processor, and it is written for them: there is
//! no software rasteriser here to fall back to, and no WARP device to make. A machine that cannot
//! give Direct3D 11 a hardware device cannot run this app at all — which is already true of the
//! window itself, since GPUI asks for the same device and has no software path either. What
//! [`InkLayer::install`] reports in that case is why, once, at startup.
//!
//! ## This phase
//!
//! The device, the visual and a probe: an opaque red rectangle over the whole canvas, so that the
//! one assumption the arrangement makes — that a `topmost = false` composition target lands behind
//! a window whose content is a topmost one — can be *looked at* before anything is built on it.
//! What it should look like: red where the desk shows, and no red anywhere over the interface.

use gpui_kit::base::{Root, RootPlugin};
use gpui_kit::{
    div, rgba, App, Context, Div, IntoElement, Refineable as _, Render, Stateful, Styled as _,
    StyleRefinement, Window,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;

pub mod canvas;
mod device;
mod render;

pub use canvas::{Canvas, Fill, Ink, Page, Rect};

use device::Device;

/// Leaves the window's own surface unpainted, so that what GPUI leaves alone is the canvas.
///
/// ## What is in the way, and why this is the way through it
///
/// GPUI Kit paints the root surface with the theme's background: `gpui-component`'s window state is
/// a [`RootPlugin`], and its `style` hook refines the surface with `.bg(theme.background)` — which
/// is the desk, one opaque rectangle over everything the app's own elements do not cover. Elements
/// *inside* the surface cannot undo it, because it is painted behind them.
///
/// A `RootPlugin` registered **after** that one is applied after it — plugins style the surface in
/// registration order — so the last refinement of the background is the one that stands. This
/// registers a plugin that draws nothing and refines nothing but that one field.
///
/// Registered before the window is made: a window's plugins are read when it is created.
pub fn leave_the_surface_unpainted(cx: &mut App) {
    Root::register_plugin(cx, bare);
}

/// The plugin's own entity, which exists for its `style` hook and draws nothing.
struct Bare;

fn bare(_: &mut Window, _: &mut Context<Bare>) -> Bare {
    Bare
}

impl Render for Bare {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl RootPlugin for Bare {
    /// No background: the desk is the canvas layer's to paint (see the module docs).
    ///
    /// Only the background. The surface's font and text colour are GPUI Kit's to set, and a plugin
    /// that replaced the whole style would be a plugin that had to keep following the library.
    fn style(&self, surface: &mut Stateful<Div>, _window: &mut Window, _cx: &mut App) {
        surface
            .style()
            .refine(&StyleRefinement::default().bg(rgba(0x0000_0000)));
    }
}

/// The canvas, drawn by a renderer of the app's own.
///
/// Held by the view for as long as the window is (`Option`, because a machine may refuse: see the
/// module docs). Dropping it takes the canvas off the screen and leaves the window as GPUI draws
/// it.
pub struct InkLayer {
    device: Device,
}

impl InkLayer {
    /// Takes the desk of a window, or says why it could not.
    ///
    /// The reason is returned rather than logged: it is something the person at the machine may
    /// have to act on — a driver, a remote session, a virtual machine without a graphics
    /// processor — and the status line is where this app says such things.
    pub fn install<W: HasWindowHandle>(window: &W) -> Result<Self, String> {
        let handle = window
            .window_handle()
            .map_err(|error| format!("the window has no handle: {error}"))?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err(String::from("the window is not a Win32 one"));
        };
        let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);

        let device = Device::new(hwnd).map_err(|error| format!("{error:#}"))?;

        Ok(InkLayer { device })
    }

    /// Draws a frame of the canvas and presents it.
    ///
    /// The canvas is described by the app (see [`Canvas`]) and drawn here, so the app's design — the
    /// shadow's steps, the paper's colour, the sheet's geometry — lives in one place whether the
    /// frame is drawn by this layer or by the frame's own painting.
    pub fn draw(&mut self, canvas: &Canvas) -> Result<(), String> {
        self.device
            .render(canvas)
            .map_err(|error| format!("{error:#}"))
    }
}
