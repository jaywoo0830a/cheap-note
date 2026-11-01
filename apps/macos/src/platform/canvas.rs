//! The canvas: a view of ours below GPUI's, drawn by Metal.
//!
//! Everything about how the pixels are made lives in [`crate::platform::metal`]; this is the part that
//! belongs to a window — the subview, its size, and the frame's description of what to draw.

use core_graphics::base::CGFloat;
use note_ui::ink_layer::Canvas;
use note_ui::platform::InkSurface;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::platform::metal::Surface;

/// The ink, drawn by the platform.
pub struct InkCanvas {
    /// The view the canvas is shown in: a subview of the window's content view, below GPUI's.
    view: Retained<NSView>,
    /// The GPU surface inside it.
    surface: Surface,
}

impl InkCanvas {
    /// Installs the canvas in a window that has just been made: a view of ours under GPUI's.
    pub fn install(window: &gpui_kit::Window) -> Option<Self> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };

        // SAFETY: AppKit's own content view, alive for the window's life, used on the main thread.
        let gpui_view = unsafe { appkit.ns_view.as_ptr().cast::<NSView>().as_ref() }?;

        // SAFETY: the view is in a window — it is the one GPUI draws the interface into — and this is the
        // main thread. The parent is the window's content view, which outlives the canvas.
        let parent = unsafe { gpui_view.superview() }?;

        let scale = f64::from(window.scale_factor());
        let frame = gpui_view.frame();

        let view = NSView::new(MainThreadMarker::new()?);
        view.setFrame(frame);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let surface = Surface::install(&view, scale)?;

        // Below GPUI's view: the interface is painted over the ink, never the other way round.
        parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, Some(gpui_view));

        let mut canvas = InkCanvas { view, surface };
        canvas.follow(scale);

        Some(canvas)
    }

    /// Matches the surface to the view.
    fn follow(&mut self, scale: f64) {
        let frame = self.view.frame();
        let (width, height) = (device(frame.size.width, scale), device(frame.size.height, scale));

        self.surface.resize(width, height, scale);
    }
}

impl InkSurface for InkCanvas {
    /// Draws one description of the canvas.
    ///
    /// Called twice in a frame's life: once per pen batch, by the pump, and once per UI frame, by the
    /// view. Both are cheap: the ink's triangles are cached by the surface, and a frame that changed
    /// nothing redraws what is already there.
    fn draw(&mut self, canvas: &Canvas) -> Result<(), String> {
        // Nothing has described a canvas yet: a reading that arrived before the first frame.
        if canvas.sheet.is_none() {
            return Ok(());
        }

        let scale = if canvas.scale > 0.0 {
            f64::from(canvas.scale)
        } else {
            1.0
        };

        self.follow(scale);
        self.surface.draw(canvas);

        Ok(())
    }
}

/// A length in logical pixels, in device pixels: at least one, because a texture of none is no texture.
fn device(length: CGFloat, scale: f64) -> usize {
    ((length * scale).round() as usize).max(1)
}
