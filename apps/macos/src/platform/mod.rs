//! The services the view asks for, on macOS, and the assembly of them.

pub mod canvas;
pub mod ghost;
pub mod metal;
pub mod pen;
pub mod pointer;

use std::sync::Arc;

use gpui_kit::base::{Root, RootPlugin};
use gpui_kit::{
    div, rgba, App, Context, Div, IntoElement, Refineable as _, Render, Stateful, StyleRefinement,
    Styled as _, Window, WindowBackgroundAppearance,
};
use note_ui::platform::{GhostFeed, Installer, Platform};

use crate::platform::canvas::InkCanvas;
use crate::platform::ghost::Ghost;
use crate::platform::pen::PenService;
use crate::platform::pointer::SystemCursor;

/// What this platform hands the view.
pub static INSTALLER: Installer = Installer {
    before_window,
    install,
};

/// Leaves the window's own surface unpainted, so that what GPUI leaves alone is the canvas.
///
/// The same plugin the Windows build registers, for the same reason: GPUI Kit's window state is a
/// `RootPlugin` that refines the root surface with `.bg(theme.background)` — an opaque rectangle over
/// everything the app's own elements do not cover. A plugin registered **after** that one is applied
/// after it, so the last refinement of the background is the one that stands.
fn before_window(cx: &mut App) {
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
    /// No background: the desk is the canvas layer's to paint (see [`canvas`]).
    fn style(&self, surface: &mut Stateful<Div>, _window: &mut Window, _cx: &mut App) {
        surface
            .style()
            .refine(&StyleRefinement::default().bg(rgba(0x0000_0000)));
    }
}

/// Installs the services on a window that has just been made.
fn install(window: &mut Window) -> Platform {
    // The window's own surface has to be transparent before a view of ours can be seen under it: GPUI
    // then makes its own layer non-opaque and the window clear, which is what the canvas needs. Without
    // this the canvas is painted behind an opaque surface and nothing shows.
    window.set_background_appearance(WindowBackgroundAppearance::Transparent);

    // The pen's mark first: the pen's tap is handed it, and the tap has to exist when the capture starts.
    let ghost = Arc::new(Ghost::install().expect("the pen's state"));

    // The capture must be attached on the thread that owns the window, which is this one: the monitor's
    // block is AppKit's to call, on that thread, and it converts coordinates with the window's view.
    let pen = PenService::attach(window, Some(Arc::clone(&ghost)));

    // After the capture, so the pointer has the last word on a window that is activated or entered.
    let pointer = SystemCursor::install().map(|cursor| Box::new(cursor) as Box<_>);

    // The canvas, which is the platform's own renderer here rather than the frame's. The app cannot run
    // without one: the frame leaves the desk to it (see `NoteApp::render`).
    // with the stroke under the pen.
    let ink = Some(
        Box::new(InkCanvas::install(window, Arc::clone(&ghost)).expect("a canvas for the window"))
            as Box<_>,
    );

    Platform {
        ink,
        ghost: Some(Arc::clone(&ghost) as Arc<dyn GhostFeed>),
        pointer,
        pen: Box::new(pen),
    }
}
