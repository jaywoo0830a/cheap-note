//! The canvas: a view of ours below GPUI's, drawn by the engine.
//!
//! Everything about *what* to draw is `cheap-note-engine`: it turns the view's readings and strokes into
//! bands, in the paper's own coordinates. Everything about *how* the pixels are made is
//! `cheap-note-engine-metal`: a `MetalSurface` over the device. What is here is the part that belongs to a
//! window — the subview, its size, the placement, and the translation of the view's `Canvas` into the
//! engine's description.
//!
//! ## Where each thing comes from
//!
//! | The engine needs | Here it comes from |
//! |---|---|
//! | the sheet, the turns, the zoom, the pan | `Canvas::ink` ([`placement`]) |
//! | the ink of a page | `Canvas::ink.strokes`, in `PageBands` ([`ink_of`]) |
//! | what a reading laid | `InkSurface::lay`, one band a reading ([`laid`]) |
//! | the desk, the shadow, the ruling | `Canvas::fills` and `Canvas::rules` ([`base_of`]) |
//! | the pen's mark | the ghost, as [`engine_metal::Mark`] ([`mark_of`]) |
//!
//! The two lists that were *the whole* of the old canvas — `ink_key`/`refresh_ink` for the finished ink
//! and `open`/`open_stroke` for the stroke under the pen — are gone: the engine draws the bands it is
//! handed, and the frame's whole canvas is the same bands, built fresh. A pan or a zoom changes the
//! placement and nothing else, which is why neither rebuilds a triangle.

use core_graphics::base::CGFloat;
use note_ui::ink_layer::Canvas;
use note_ui::platform::{InkSurface, Laid};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
use objc2_quartz_core::CAMetalLayer;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Arc;

use cheap_note_engine::band::Band;
use cheap_note_engine::engine::{Engine, FrameCap};
use cheap_note_engine::footprint::Footprint;
use cheap_note_engine::page::{cull_of, PageBands};
use cheap_note_engine::place::Placement;
use cheap_note_engine::surface::{BandDraw, FillRule, Frame, InkDraw, PresentResult, Surface};
use cheap_note_engine_metal::{BaseLayer, Mark, MetalSurface, RectFill, Target};

use crate::platform::ghost::Ghost;

/// The ink, drawn by the engine.
pub struct InkCanvas {
    /// The view the canvas is shown in: a subview of the window's content view, below GPUI's.
    view: Retained<NSView>,
    /// The engine's half: the plan, the device, the present.
    surface: MetalSurface<cheap_note_engine_metal::Device>,
    /// The engine's head: what changed, and what is owed (E1's "does this draw?").
    engine: Engine,
    /// Where the pen is, for the mark this canvas draws over the ink.
    ghost: Arc<Ghost>,
    /// The placement the last frame was drawn at: a reading arrives with the paper's coordinates, and
    /// the band it lays has to land where the *sheet* is drawn.
    placement: Placement,
    /// The damage of the readings since the last frame drew the whole page: what a present between
    /// frames repairs otherwise.
    laid: Footprint,
    /// The band a batch laid, kept for the present its own wake asks for: the damage to repair, the
    /// colour it was written in, and how many readings it stands for.
    pending: Option<(Footprint, Band, u32)>,
}

impl InkCanvas {
    /// Installs the canvas in a window that has just been made: a view of ours under GPUI's.
    pub fn install(window: &gpui_kit::Window, ghost: Arc<Ghost>) -> Option<Self> {
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

        // The layer the engine's device draws into: a `CAMetalLayer` of the app's own, under GPUI's view
        // (`doc/ENGINE.md` §2.1).
        let layer = CAMetalLayer::layer();
        view.setWantsLayer(true);
        view.setLayer(Some(&layer));

        let target = target(frame.size.width, frame.size.height, scale);

        let device = cheap_note_engine_metal::Device::new(layer.clone(), target)?;

        // Below GPUI's view: the interface is painted over the ink, never the other way round.
        parent.addSubview_positioned_relativeTo(
            &view,
            NSWindowOrderingMode::Below,
            Some(gpui_view),
        );

        let surface = MetalSurface::new(device, target);

        Some(InkCanvas {
            view,
            surface,
            engine: Engine::new(FrameCap::Unlimited),
            ghost,
            placement: Placement::new(
                (0.0, 0.0),
                note_format::pages::Quarters::UPRIGHT,
                1.0,
                (0.0, 0.0),
            ),
            laid: Footprint::empty(),
            pending: None,
        })
    }

    /// Matches the surface to the view: the drawable's size and the display's scale.
    fn follow(&mut self, scale: f64) {
        let frame = self.view.frame();
        let target = target(frame.size.width, frame.size.height, scale);

        self.surface.device_mut().resize(target);
        self.surface.resize(target.width, target.height);
    }
}

/// The target for a view of this size at this display scale: whole device pixels, and the logical size
/// they came from.
///
/// The logical size is the device size divided back by the scale, not the view's own width, so that the
/// two numbers [`Target`] holds describe the same rectangle exactly — a target of 1000.5 logical pixels
/// at 2× is a texture of 2001, and a shader told 1000.5 would place the sheet half a pixel from where the
/// texture ends.
fn target(width: CGFloat, height: CGFloat, scale: f64) -> Target {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale as f32
    } else {
        1.0
    };

    let device_width = device(width, f64::from(scale));
    let device_height = device(height, f64::from(scale));

    Target::new(
        device_width as f32 / scale,
        device_height as f32 / scale,
        scale,
    )
}

impl InkSurface for InkCanvas {
    /// Draws one description of the canvas: the base under the ink, the page's bands, the pen's mark.
    ///
    /// Called twice in a frame's life: once per pen batch, by the pump, and once per UI frame, by the
    /// view. Nothing here caches: the engine's bands are cheap to build (a stroke's pieces are already
    /// computed), and what a pan or a zoom changes is the *placement*, not the geometry.
    fn draw(&mut self, canvas: &Canvas) -> Result<bool, String> {
        // Nothing has described a canvas yet: a reading that arrived before the first frame.
        if canvas.sheet.is_none() {
            return Ok(false);
        }

        let scale = if canvas.scale > 0.0 {
            f64::from(canvas.scale)
        } else {
            1.0
        };

        self.follow(scale);

        let moved = placement(canvas) != self.placement;
        self.placement = placement(canvas);

        let base = base_of(canvas);
        let mark = mark_of(self.ghost.mark().as_ref());

        // A reading laid since the last frame, and the sheet where it was: draw the band over the ink the
        // surface already holds rather than the page again. This is E2 in the app — a reading costs its
        // band — and it holds because the surface keeps what it drew (assumption 2, `doc/ENGINE.md` §2.4).
        //
        // A moved sheet, a page turn or an edit is the whole page: the placement changed under everything
        // on it, or the ink is not the ink that was drawn.
        if !moved && self.engine.state() == cheap_note_engine::engine::State::Drawing {
            if let Some((repair, band, colour)) = self.pending.take() {
                let frame = Frame {
                    base: Default::default(),
                    ink: InkDraw {
                        repair: repair.rects().to_vec(),
                        bands: vec![BandDraw {
                            triangles: band.pieces().to_vec(),
                            colour: packed(colour),
                            rule: FillRule::NonZero,
                        }],
                    },
                };

                // The mark comes with it: a band laid is a nib that moved.
                match self
                    .surface
                    .present(&BaseLayer::default(), &frame, mark.as_ref())
                {
                    Ok(PresentResult::Presented) => {
                        self.engine.settle();
                        return Ok(true);
                    }
                    Ok(PresentResult::NotReady) => return Ok(false),
                    Err(error) => return Err(error.to_string()),
                }
            }
        }

        let frame = Frame {
            base: Default::default(),
            ink: ink_of(canvas, &self.placement),
        };

        // A present that finds no drawable is `false`, and the app tries again on its next wake.
        match self.surface.present(&base, &frame, mark.as_ref()) {
            Ok(PresentResult::Presented) => {
                self.laid = Footprint::empty();
                self.pending = None;
                self.engine.settle();
                Ok(true)
            }
            Ok(PresentResult::NotReady) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Lays the band a batch of readings swept, before any frame describes the canvas again.
    ///
    /// The engine's reading-sized path (`doc/ENGINE.md`, E2): what the model appended becomes the band
    /// between the newest discs, placed on the sheet, and kept for the very next present — the pen's own
    /// wake, a millisecond or two later — which draws *this band* over the ink already on the surface
    /// rather than the whole page. The frame that follows draws the page anyway, so this is the fast path
    /// and not the only one; when it cannot be taken the page is drawn, which is correct and slower.
    fn lay(&mut self, laid: &[Laid]) -> Result<bool, String> {
        if laid.is_empty() {
            return Ok(false);
        }

        let band = laid_between(laid, &self.placement);
        let now = std::time::Instant::now();
        let work = self.engine.on_batch(band.clone(), laid.len() * 96, now);

        if !work.layed_ink {
            return Ok(false);
        }

        // The band itself, not a rectangle standing in for it: the next present draws exactly the ink
        // these readings laid, and the damage is what it repaints under them.
        self.pending = Some((
            band.damage(),
            band,
            laid.last()
                .map(|reading| reading.colour)
                .unwrap_or(0x1B_1B1F),
        ));

        Ok(true)
    }

    /// Waits for the display's clock, or does not.
    ///
    /// `CAMetalLayer.displaySyncEnabled` is the whole of it on this platform: Metal itself has no
    /// present-with-an-interval, and the layer is what turns a frame into a scan-out. Turning it off is
    /// the experiment `CHEAP_NOTE_NO_VSYNC` used to be — a frame goes out the moment it is drawn, which
    /// is lower latency and can tear.
    fn set_vsync(&mut self, on: bool) {
        self.surface.device_mut().set_vsync(on);
    }
}

/// Where the paper is drawn, and how large, for a canvas: the placement the engine bakes bands through.
///
/// **Logical pixels, and the display's density only once.** A [`Placement`] is in the engine's own unit —
/// the one `Target::device_point` multiplies into device pixels when the half draws. So this is built
/// from what the canvas holds *undensified*: paper, rotation, zoom and origin exactly as the view
/// computed them. Multiplying by `canvas.scale` here as well gave one point two densities, and it drew
/// the ink at twice its distance from the origin while the sheet, which arrives logical, stayed put.
pub(crate) fn placement(canvas: &Canvas) -> Placement {
    Placement::new(
        canvas.ink.paper,
        canvas.ink.rotation,
        canvas.ink.zoom,
        canvas.ink.origin,
    )
}

/// The desk, the shadow's steps, the sheet and the ruling, as the base layer draws them.
pub(crate) fn base_of(canvas: &Canvas) -> BaseLayer {
    let mut fills = Vec::with_capacity(canvas.fills.len() + canvas.rules.len());

    for fill in canvas.fills.iter().chain(canvas.rules.iter()) {
        if fill.rect.is_empty() {
            continue;
        }

        fills.push(RectFill {
            rect: cheap_note_engine::footprint::Rect {
                x: fill.rect.x,
                y: fill.rect.y,
                width: fill.rect.width,
                height: fill.rect.height,
            },
            colour: colour_of(fill.colour),
        });
    }

    BaseLayer {
        desk: Some(colour_of(canvas.desk)),
        fills,
    }
}

/// The page's ink, as the engine draws it: one band a stroke, culled to what is on screen.
///
/// The stroke under the pen is the last band, because it is the newest ink on the page — which is what
/// `lay` and `draw` both rely on.
pub(crate) fn ink_of(canvas: &Canvas, placement: &Placement) -> InkDraw {
    let detail = note_format::ink::detail_zoom(canvas.ink.zoom);
    let page = PageBands::of(
        canvas.ink.strokes.as_slice(),
        cull_of(canvas.ink.visible),
        placement,
        detail,
    );

    let mut bands: Vec<BandDraw> = Vec::with_capacity(page.bands().len() + 1);

    for (colour, band) in page.bands() {
        bands.push(BandDraw {
            triangles: band.pieces().to_vec(),
            colour: packed(*colour),
            rule: FillRule::NonZero,
        });
    }

    if let Some(open) = &canvas.ink.open {
        let band = Band::of_stroke(open, placement);

        if !band.is_empty() {
            bands.push(BandDraw {
                triangles: band.pieces().to_vec(),
                colour: packed(open.color),
                rule: FillRule::NonZero,
            });
        }
    }

    InkDraw {
        repair: page.damage().rects().to_vec(),
        bands,
    }
}

/// The band a batch of readings laid: the span between each pair of consecutive points of one stroke,
/// and a disc for a reading that began one.
///
/// Only what *this batch* added — never the stroke, never the page (E2).
fn laid_between(laid: &[Laid], placement: &Placement) -> Band {
    let mut pieces: Vec<note_format::ink::Piece> = Vec::new();

    for (index, reading) in laid.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .map(|before| &laid[before])
            .filter(|before| before.stroke == reading.stroke);

        let from = previous.map_or(reading.point, |before| before.point);
        let to = reading.point;
        let radius = (from.width + to.width) * 0.25;

        if (from.x - to.x).abs() < 1e-4 && (from.y - to.y).abs() < 1e-4 {
            // A first reading, or a pen that did not move: the disc the nib left, as four wedges.
            for step in 0..4 {
                let at = std::f32::consts::TAU * step as f32 / 4.0;
                let next = std::f32::consts::TAU * (step + 1) as f32 / 4.0;

                pieces.push(note_format::ink::Piece::Fan {
                    centre: [from.x, from.y],
                    from: [from.x + radius * at.cos(), from.y + radius * at.sin()],
                    to: [from.x + radius * next.cos(), from.y + radius * next.sin()],
                });
            }

            continue;
        }

        // The span: the outer tangents of the two discs.
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        let length = (dx * dx + dy * dy).sqrt();
        let (nx, ny) = (-dy / length * radius, dx / length * radius);

        pieces.push(note_format::ink::Piece::Quad {
            left_from: [from.x + nx, from.y + ny],
            left_to: [to.x + nx, to.y + ny],
            right_to: [to.x - nx, to.y - ny],
            right_from: [from.x - nx, from.y - ny],
        });
    }

    Band::of(&pieces, placement)
}

/// The pen's mark, from what the ghost last said.
fn mark_of(mark: Option<&crate::platform::ghost::Mark>) -> Option<Mark> {
    let mark = mark?;
    let position = mark.cursor.position();

    Some(Mark {
        at: (position[0], position[1]),
        colour: mark.colour,
        halo: mark.halo,
        nib_radius: mark.nib_radius,
        lean: 1.0 - mark.cursor.body_fade(),
        unit: mark.unit,
    })
}

/// A colour as the device takes it: `0xAARRGGBB`, from an `Hsla` the view wrote.
fn colour_of(colour: gpui_kit::Hsla) -> u32 {
    let rgba = gpui_kit::Rgba::from(colour);
    let byte = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u32;

    (byte(rgba.a) << 24) | (byte(rgba.r) << 16) | (byte(rgba.g) << 8) | byte(rgba.b)
}

/// A stroke's colour as the device takes it: already `0xAARRGGBB` in the note.
fn packed(colour: u32) -> u32 {
    colour
}

/// A length in logical pixels, in device pixels: at least one, because a texture of none is no texture.
fn device(length: CGFloat, scale: f64) -> usize {
    ((length * scale).round() as usize).max(1)
}
