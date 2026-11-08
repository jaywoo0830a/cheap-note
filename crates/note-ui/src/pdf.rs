//! PDF pages, rendered by Pdfium and handed to GPUI as images.
//!
//! ## Why the Pdfium handle is leaked
//!
//! `pdfium-render`'s `PdfDocument<'a>` borrows the `Pdfium` that opened it, so storing both in
//! one struct would be a self-reference. The `Pdfium` handle is a tiny wrapper over a
//! process-wide library binding that lives for the whole process anyway, so the app leaks it
//! once (`Box::leak`) and keeps the document as `PdfDocument<'static>`. This is the documented
//! shape of the problem — a self-referential struct — and leaking is the one safe way out of
//! it that does not need `unsafe`.
//!
//! ## The two coordinate systems
//!
//! A page has a size in **PDF points** (1/72 inch) and a size in **bitmap pixels** once it is
//! rendered. The app draws the bitmap at a chosen logical width, so both are kept: the point
//! size decides the aspect ratio at any display width, and the pixel size decides how sharp the
//! bitmap is.
//!
//! ## Colour order
//!
//! Pdfium renders into a BGRA buffer, and GPUI's renderer uploads BGRA. Requesting Pdfium's
//! *native* byte order (`set_reverse_byte_order(false)`) means the rendered bytes can be handed
//! to GPUI untouched — no per-pixel conversion on the way to the screen.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::RenderImage;

use crate::error::{AppError, Result};
use crate::pages::{swaps_axes, Quarters};
use crate::pdfium::{Document, OutlineEntry, RenderJob};

/// What one slice of a render did, re-exported because it is part of this module's own API.
pub use crate::pdfium::Progress;

/// Which page, at what size, with which render options.
///
/// Everything a rendered bitmap depends on, in one value. This is the *cache key*, and the rule
/// about it is short: a field that changes the pixels must be in the key, or a bitmap rendered for
/// one request will be served for another. The failures that hide in a short key are all quiet
/// ones — a page re-rendered on every zoom step, ink drawn on top of a stale overlay, a rotated
/// page shown the wrong way up — and each of them is a field that was left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageKey {
    /// Which document: bumped on every open, so a *reopened* file is never served the old pixels.
    pub document: u64,
    /// The page index in the document.
    pub page: usize,
    /// The bitmap width, quantised onto the quality ladder.
    pub pixels: u32,
    /// The page's own rotation, in quarter turns clockwise.
    pub rotation: u8,
    /// Whether annotations are drawn *into* the page bitmap.
    pub annotations: bool,
    /// The colour format the bytes are in.
    pub colour: Colour,
}

/// The colour format a page bitmap is rendered in.
///
/// The app asks for BGRA in Pdfium's native byte order because that is the order GPUI's renderer
/// uploads, so the rendered bytes need no conversion — but the *choice* is part of the cache key,
/// because a bitmap rendered in one format is not the bitmap another request asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Colour {
    /// 8 bits per channel, blue-green-red-alpha, Pdfium's own byte order.
    Bgra,
    /// 8 bits per pixel, one channel.
    Grayscale,
}

/// What the cache did, and what rasterising cost.
///
/// Point 5 of any renderer optimisation: the numbers come before the work. A cache that is never
/// hit and a cache that is never *missed* look identical from the outside — both serve a page —
/// and only these counters tell them apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PdfStats {
    /// Requests served from the cache.
    pub hits: u64,
    /// Requests that needed a page rasterised.
    pub misses: u64,
    /// Pages rasterised since the document was opened.
    pub rendered: u64,
    /// Renders abandoned because the view moved on before they finished.
    pub cancelled: u64,
    /// Bitmaps dropped to stay inside the memory budget.
    pub evicted: u64,
    /// A request answered with another rung of the ladder because the exact one was not ready.
    pub placeholders: u64,
    /// What the cached bitmaps weigh, in bytes.
    pub bytes: u64,
}

impl PdfStats {
    /// The cache's hit ratio, or `None` before anything has been asked for.
    pub fn hit_ratio(&self) -> Option<f32> {
        let requests = self.hits + self.misses;
        (requests > 0).then(|| self.hits as f32 / requests as f32)
    }

    /// A one-line summary for the status bar.
    pub fn summary(&self) -> String {
        let ratio = match self.hit_ratio() {
            Some(ratio) => format!("{:.0}%", ratio * 100.0),
            None => String::from("—"),
        };

        format!(
            "pdf cache {ratio} ({} hit {} miss) {} render {} placeholder {} cancel, {:.0} MB",
            self.hits,
            self.misses,
            self.rendered,
            self.placeholders,
            self.cancelled,
            self.bytes as f64 / (1024.0 * 1024.0)
        )
    }
}

/// A page that has been rendered to a bitmap GPUI can draw.
///
/// The bitmap's own pixel size is not stored: GPUI scales the image to the bounds it is painted
/// into, so the only size the app needs from the page is its aspect ratio, which the point size
/// already carries.
#[derive(Clone)]
pub struct RenderedPage {
    /// The rendered bitmap.
    pub image: Arc<RenderImage>,
    /// The page's width in PDF points, as the *document* has it — which already has the page's own
    /// `/Rotate` in it, because Pdfium reports a page's size the way it draws it (`/Rotate 90` on an
    /// A4 page answers `842 × 595`). It is the size the *reader's* turn is not in yet.
    pub point_width: f32,
    /// The page's height in PDF points, likewise.
    pub point_height: f32,
    /// The width this bitmap was actually rendered at, in pixels.
    ///
    /// The requested width when the cache had it, and a *different* one when this is standing in
    /// for a rung that is not ready yet: the caller compares the two to know whether what it is
    /// drawing is the real thing.
    ///
    /// The width of the *drawn* page, so it is the width a page the reader has turned is drawn at (see
    /// [`RenderedPage::display_size`]) and matches the bitmap Pdfium produced for that turn.
    pub pixels: u32,
    /// How far the reader has turned the page, in quarter turns clockwise.
    ///
    /// The *reader's* turn and not the sum: a page's own `/Rotate` is already in `point_width` and
    /// `point_height`, and Pdfium applied it to the pixels. This is the half that is the app's, and it
    /// is what says whether the page it describes is the other way round.
    pub rotation: Quarters,
}

impl RenderedPage {
    /// The logical size to draw this page at when it is shown `logical_width` logical pixels
    /// wide, preserving the aspect ratio of the page *as it is drawn*.
    ///
    /// The width asked for is the width of the drawn page, so a page the reader has turned comes back
    /// taller than it is wide: the shape the reader sees is the shape these two numbers describe, and
    /// the bitmap behind them was rendered at the same turn.
    pub fn display_size(&self, logical_width: f32) -> (f32, f32) {
        let (width, height) = if swaps_axes(self.rotation) {
            (self.point_height, self.point_width)
        } else {
            (self.point_width, self.point_height)
        };

        if width <= 0.0 {
            return (logical_width, logical_width);
        }

        let scale = logical_width / width;
        (logical_width, height * scale)
    }
}

/// What the app is asking to see: one page, at one size, with one set of options.
///
/// Separate from the [`PageKey`] derived from it, because the key also carries what only the
/// document knows — which document it is, and which way the page is turned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// The page index.
    pub page: usize,
    /// The bitmap width, in pixels, on the quality ladder. The width of the page *as it is drawn*.
    pub pixels: u32,
    /// Which way up the reader has this page, in quarter turns clockwise. The page's own `/Rotate`
    /// belongs to the document and is added to this where the key is made and the render started.
    pub rotation: Quarters,
    /// Whether annotations are drawn into the bitmap.
    pub annotations: bool,
    /// The colour format the bytes should be in.
    pub colour: Colour,
}

impl PageRequest {
    /// A request for a page at a size, with the app's default options.
    pub fn new(page: usize, pixels: u32) -> Self {
        PageRequest {
            page,
            pixels,
            // The right way up: the reader has not turned it. A page's own `/Rotate` belongs to the
            // document, and is added to this where the key is made and the render is started.
            rotation: 0,
            // Annotations are not drawn *into* the page: the ink the user writes is the app's own
            // layer above it, and a page's own annotations belong there too — on top, not baked in.
            // Baking them in would also make a rasterised page wrong the moment one changed.
            annotations: false,
            colour: Colour::Bgra,
        }
    }

    /// The same request, rendered in grayscale or in colour.
    ///
    /// A render *option*, which is why it is a method on the request rather than a detail of the
    /// rasteriser: it is part of what identifies the pixels, and the cache key is built from this.
    pub fn grayscale(mut self, on: bool) -> Self {
        self.colour = if on { Colour::Grayscale } else { Colour::Bgra };
        self
    }

    /// The same request with the reader's turn on it.
    ///
    /// A render option like the one above, and part of the key for the same reason: a bitmap of a page
    /// the right way up is not the bitmap of that page on its side, and a page is turned *after* it has
    /// been read once, so the two live in the cache together.
    pub fn turned(mut self, turns: Quarters) -> Self {
        self.rotation = turns;
        self
    }
}

/// What one cached bitmap weighs: its pixels, four bytes each.
///
/// The height is derived from the point size at the turn the bitmap was rendered at: the same relation
/// [`crate::pdfium::RenderJob`] sizes the bitmap with, which is what keeps this a budget rather than a
/// guess.
fn weight(page: &RenderedPage) -> u64 {
    let width = page.pixels as u64;
    let (point_width, point_height) = if swaps_axes(page.rotation) {
        (page.point_height, page.point_width)
    } else {
        (page.point_width, page.point_height)
    };

    let height = if point_width > 0.0 {
        (width as f32 * point_height / point_width) as u64
    } else {
        width
    };

    width * height * 4
}

/// How far apart two rungs of the ladder are, as a ratio.
///
/// A ratio rather than a difference, because the ladder is geometric: measuring by difference would
/// always call the larger rung the nearer one.
fn ratio(pixels: u32, wanted: u32) -> f32 {
    if pixels == 0 || wanted == 0 {
        return f32::INFINITY;
    }

    (pixels as f32 / wanted as f32).max(wanted as f32 / pixels as f32)
}

/// The document the user is annotating, if one is open.
pub struct PdfDocumentView {
    /// The render in flight, if any.
    ///
    /// Declared *first* so cancellation is the first thing that happens when the view is dropped.
    /// It is not a safety requirement: a render job keeps the document it came from alive itself
    /// (see [`crate::pdfium::RenderJob`]), so either field order would close in a valid order.
    job: Option<RenderJob>,
    /// What the job in flight is rendering, for the cache key it will be filed under.
    job_request: Option<PageRequest>,
    /// The open document.
    document: Option<Document>,
    /// Rendered pages, keyed by everything their pixels depend on.
    ///
    /// Keyed properly rather than by page index with a "width changed, throw it all away" rule:
    /// that rule is what made a zoom gesture cost a rasterisation per rung *crossed*, in both
    /// directions, however recently that rung had been on screen.
    cache: HashMap<PageKey, RenderedPage>,
    /// The keys in the order they were last used, oldest first: the eviction order.
    ///
    /// A `Vec` with a linear scan is not an LRU cache for a large set — but this set is a handful
    /// of pages at a few rungs each, and a scan of a dozen keys is cheaper than the bookkeeping
    /// that would replace it.
    order: Vec<PageKey>,
    /// Which document the keys above belong to; every open bumps it.
    document_id: u64,
    /// How many bytes of cached bitmaps are allowed before the oldest are dropped.
    ///
    /// A field rather than a bare constant so that a test can set a budget small enough to actually
    /// exceed — the eviction path is the one piece of a cache that is never exercised by the code
    /// that is trying to avoid evicting.
    budget: u64,
    /// Each page's rotation in quarter turns clockwise, read once and remembered.
    rotations: Vec<u8>,
    /// What rasterising has cost and what the cache has saved.
    stats: PdfStats,
    /// The document's own table of contents, read once when it was opened.
    ///
    /// Held rather than asked for: it belongs to the document rather than to any page of it, it is a
    /// handful of kilobytes at worst, and the app asks for it while drawing a *screen* (see
    /// [`crate::outline`]) — where a walk into Pdfium would be work done on the wrong side of a frame.
    outline: Vec<OutlineEntry>,
}

/// How much of a page the pump renders per wake, when the pen is quiet.
///
/// Four milliseconds is a budget rather than a deadline: Pdfium is asked whether to stop between
/// the pieces of a page, so a slice ends near this, not at it. It is chosen against the frame rate —
/// at 165 Hz a frame is 6 ms, so a slice has to be shorter than that to stay out of the way, and
/// four leaves room for the pen queue to be drained in the same wake.
pub const SLICE_BUDGET: Duration = Duration::from_millis(4);

/// How much of the memory budget is spent on cached page bitmaps.
///
/// A 1650-pixel-wide A4 page is about 11 MB and the top rung about 44 MB, so 128 MB holds the
/// current page at two or three qualities — which is what a zoom gesture and a page turn actually
/// need. Beyond that the oldest bitmaps go, in use order.
const CACHE_BUDGET_BYTES: u64 = 128 * 1024 * 1024;

impl Default for PdfDocumentView {
    fn default() -> Self {
        PdfDocumentView {
            job: None,
            job_request: None,
            document: None,
            cache: HashMap::new(),
            order: Vec::new(),
            document_id: 0,
            budget: CACHE_BUDGET_BYTES,
            rotations: Vec::new(),
            stats: PdfStats::default(),
            outline: Vec::new(),
        }
    }
}

impl PdfDocumentView {
    /// A view with no document: the app draws on a blank page.
    pub fn empty() -> Self {
        PdfDocumentView::default()
    }

    /// Opens a PDF that is already in memory — what a note carries — and renders its first page.
    ///
    /// This is the only way in. A note keeps its document *inside* itself and remembers what the
    /// document was called, so the app opens bytes with a name rather than a path: there is no file
    /// to open, and no second copy of the document to hold.
    ///
    /// Nothing is written to a temporary file, so a note opens from anywhere the app can read, and
    /// opening one leaves nothing behind on disk.
    pub fn open_bytes(name: String, bytes: Vec<u8>, render_pixel_width: u32) -> Result<Self> {
        let mut view = PdfDocumentView {
            document: Some(Document::open(name, bytes)?),
            ..PdfDocumentView::default()
        };

        // A new document: the keys of the old one must never match this one's bitmaps.
        view.document_id = 1;

        // The document's own table of contents, read here rather than when the app asks for it: it is
        // a walk of a few dozen titles and destinations, it needs no page handle, and the moment the
        // app asks is a frame that is drawing a screen (see [`crate::outline`]). A document without one
        // answers with nothing, which is what most documents do.
        view.outline = view
            .document
            .as_ref()
            .map(|document| document.outline())
            .unwrap_or_default();

        // Render the first page now, so an open PDF shows something other than a blank page. This
        // is the one blocking render left in the app, and it is deliberate: the frame that follows
        // an open has nothing to show until it has happened.
        view.render_page(PageRequest::new(0, render_pixel_width))?;
        Ok(view)
    }

    /// Whether a document is open.
    pub fn is_loaded(&self) -> bool {
        self.document.is_some()
    }

    /// How many pages the document has.
    pub fn page_count(&self) -> usize {
        self.document
            .as_ref()
            .map(|document| document.page_count())
            .unwrap_or(0)
    }

    /// The file name of the open document, for the status bar.
    pub fn file_name(&self) -> String {
        self.document
            .as_ref()
            .map(|document| document.name().to_string())
            .unwrap_or_else(|| String::from("untitled page"))
    }

    /// The document's own table of contents, as it was read when the document was opened.
    ///
    /// A tree in the document's own order, each entry carrying the page of *this* document it opens — no
    /// page handle, no rasterising, nothing that depends on a render having finished. Empty for a
    /// document that has none, which is most documents and is not a failure.
    pub fn outline(&self) -> &[OutlineEntry] {
        &self.outline
    }

    /// Whether the document carries a table of contents at all.
    ///
    /// What the bar's button asks before offering to open it: an offer to open a screen that says "this
    /// document has no contents" is an offer that was better off not made.
    pub fn has_outline(&self) -> bool {
        !self.outline.is_empty()
    }

    /// The page's own size in PDF points, without rendering it.
    ///
    /// The page bitmap carries this too, but only for the page that is rendered; this answers for
    /// any page, which is what the app needs to describe the sheet it is writing on.
    pub fn page_point_size(&self, index: usize) -> Option<(f32, f32)> {
        self.document.as_ref()?.page_point_size(index)
    }

    /// The best bitmap available *now* for a request, without rasterising anything.
    ///
    /// This is what a frame calls, and it must never block on Pdfium: the frame gets the exact
    /// bitmap if the cache has it, and otherwise the *nearest* rung of the ladder already rendered
    /// for this page — a coarser one, usually, which is a slightly soft page in the next frame
    /// instead of a blank one for twenty milliseconds. `RenderedPage::pixels` says which of the two
    /// happened, so the caller can tell whether it is looking at the real thing.
    ///
    /// A miss is *recorded* here but nothing is scheduled: what is missing is
    /// [`Self::plan`]'s answer, and that separation is what lets the caller decide when to pay.
    pub fn page_for_frame(&mut self, request: PageRequest) -> Option<RenderedPage> {
        let key = self.key_for(request)?;

        // Cloned before the counters move: a `RenderedPage` is an `Arc` and two floats, and the
        // alternative is holding a borrow of the map across a mutation of its owner.
        if let Some(cached) = self.cache.get(&key).cloned() {
            self.stats.hits += 1;
            self.touch(key);
            return Some(cached);
        }

        self.stats.misses += 1;

        let nearest = self.nearest_key(key)?;
        self.stats.placeholders += 1;
        let page = self.cache.get(&nearest).cloned();
        if page.is_some() {
            self.touch(nearest);
        }
        page
    }

    /// What still has to be rasterised for this request, if anything.
    ///
    /// `None` means the frame already has the bitmap it asked for.
    pub fn plan(&mut self, request: PageRequest) -> Option<PageRequest> {
        let key = self.key_for(request)?;
        (!self.cache.contains_key(&key)).then_some(request)
    }

    /// Starts rendering `request`, abandoning whatever was in flight.
    ///
    /// There is one job at a time, always for the page in front of the user: Pdfium renders on the
    /// thread that calls it, so two pages at once is not something it offers, and a render the view
    /// has moved past is work nobody will ever see.
    pub fn begin(&mut self, request: PageRequest) -> Result<()> {
        self.abandon();

        let Some(document) = self.document.as_ref() else {
            return Err(AppError::Other(String::from("no PDF is open")));
        };

        let job = RenderJob::new(
            document,
            request.page,
            request.pixels,
            // The reader's turn alone: a page's own `/Rotate` is the document's, and Pdfium applies
            // that itself (see [`crate::pdfium::RenderJob::new`]).
            request.rotation,
            match request.colour {
                Colour::Bgra => crate::pdfium::Format::Bgra,
                Colour::Grayscale => crate::pdfium::Format::Grayscale,
            },
            request.annotations,
        )?;
        self.job_request = Some(request);
        self.job = Some(job);
        Ok(())
    }

    /// Renders the job in flight for up to `budget`, then hands control back.
    ///
    /// A finished page lands in the cache under the key it was asked for, counted like any other
    /// render: this is where a sliced render becomes a bitmap the frames can use. The budget is what
    /// keeps the call short — Pdfium checks the pause callback between the pieces of a page, so the
    /// slice ends on time rather than on the page.
    pub fn advance(&mut self, budget: Duration) -> Progress {
        let Some(mut job) = self.job.take() else {
            return Progress::Unfinished;
        };

        let progress = job.advance(budget);

        match progress {
            Progress::Finished => {
                let request = self.job_request.take().expect("a job always has a request");
                let key = self
                    .key_for(request)
                    .expect("a job is only started for an open document");

                match self.page_from_job(&job, request) {
                    Ok(page) => {
                        self.stats.rendered += 1;
                        self.insert(key, page);
                    }
                    Err(error) => return Progress::Failed(error.to_string()),
                }
            }
            Progress::Unfinished => self.job = Some(job),
            Progress::Failed(_) => {
                self.job_request = None;
            }
        }

        progress
    }

    /// Abandons the render in flight, if there is one.
    ///
    /// This is the whole of cancellation: the job is dropped, which releases the page and the
    /// unfinished bitmap and is what tells Pdfium to stop, and the counter says it happened. Nothing
    /// has to be signalled, because a render only runs inside [`Self::advance`] — so between calls,
    /// "not wanted any more" and "never advanced again" are the same thing.
    pub fn abandon(&mut self) {
        if self.job.take().is_some() {
            self.stats.cancelled += 1;
        }
        self.job_request = None;
    }

    /// Whether a render is in flight.
    pub fn rendering(&self) -> bool {
        self.job.is_some()
    }

    /// What the render in flight is for, if there is one.
    pub fn job_request(&self) -> Option<PageRequest> {
        self.job_request
    }

    /// Turns a finished job's pixels into a page the frames can draw.
    fn page_from_job(&self, job: &RenderJob, request: PageRequest) -> Result<RenderedPage> {
        let (width, height, pixels) = job
            .pixels()
            .ok_or_else(|| AppError::Pdf(String::from("the rendered page had no pixels")))?;

        let (point_width, point_height) = self
            .page_point_size(request.page)
            .unwrap_or((width as f32, height as f32));

        let buffer = image::RgbaImage::from_raw(width, height, pixels).ok_or_else(|| {
            AppError::Pdf(String::from(
                "the rendered page did not match the buffer size Pdfium reported",
            ))
        })?;

        Ok(RenderedPage {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
            point_width,
            point_height,
            pixels: width,
            rotation: request.rotation,
        })
    }

    /// The rendered page a request asks for, rasterising it if the cache does not have it.
    ///
    /// Blocking, and deliberately so: it runs on the calling thread *inside* a frame, which is what
    /// the `pdf` timing measures. The frame path uses [`Self::page_for_frame`] and [`Self::plan`]
    /// instead, so a rasterisation that is not ready shows the previous rung of the ladder rather
    /// than stalling the frame that asked for it.
    pub fn render_page(&mut self, request: PageRequest) -> Result<RenderedPage> {
        let Some(key) = self.key_for(request) else {
            return Err(AppError::Other(String::from("no PDF is open")));
        };

        if let Some(cached) = self.cache.get(&key).cloned() {
            self.stats.hits += 1;
            self.touch(key);
            return Ok(cached);
        }

        self.stats.misses += 1;
        // The render itself counts itself, when it finishes: `advance` is the one place a page is
        // known to be complete, and counting here as well would count every blocking render twice.
        self.render(request)
    }

    /// What rasterising has cost and what the cache has saved.
    pub fn stats(&self) -> PdfStats {
        self.stats
    }

    /// Counts a render the view moved on from before it was ever started.
    ///
    /// The app owns the *when* (see `NoteApp::plan_pdf`); the counter lives here so that every
    /// number about the renderer is in one place, and so that "cancelled" means the same thing
    /// wherever it is reported.
    pub fn count_cancelled(&mut self) {
        self.stats.cancelled += 1;
    }

    /// How many pages have been rasterised since the document was opened.
    ///
    /// Rasterising runs on the calling thread — inside a frame — so this is what lets the frame
    /// report the time it spent waiting on Pdfium rather than on itself.
    pub fn rasterised(&self) -> u64 {
        self.stats.rendered
    }

    /// The key for a request against the open document, or `None` when nothing is open.
    ///
    /// **Everything in the request that changes the pixels is in the key**, which is why this takes
    /// the *request* rather than a field or two of it: a render option left out of the key is a bitmap
    /// served for a request that asked for something else, and every such failure is a quiet one (see
    /// [`PageKey`]).
    ///
    /// The rotation in the key is the *drawn* rotation — the page's own `/Rotate` and the reader's
    /// turn, added — and reading the page's own is a Pdfium call: cheap, but not free, and it does not
    /// change under the app's feet, so it is read once per page and remembered.
    fn key_for(&mut self, request: PageRequest) -> Option<PageKey> {
        if self.document.is_none() {
            return None;
        }

        Some(PageKey {
            document: self.document_id,
            page: request.page,
            pixels: request.pixels.max(1),
            rotation: self.effective_rotation(request.page, request.rotation),
            annotations: request.annotations,
            colour: request.colour,
        })
    }

    /// Which way up a page is drawn: the document's own rotation for it, and the reader's turn, added.
    ///
    /// One place, because the same answer is needed three times — by the cache key, by the render, and
    /// by the sheet the app lays out — and two ways of adding them is a page drawn one way up, cached
    /// as another, and laid out as a third.
    fn effective_rotation(&mut self, page: usize, turns: Quarters) -> Quarters {
        crate::pages::turned(self.rotation_of(page), turns as i32)
    }

    /// The rotation of a page, in quarter turns clockwise, remembered after the first read.
    fn rotation_of(&mut self, index: usize) -> u8 {
        if let Some(rotation) = self.rotations.get(index) {
            return *rotation;
        }

        let rotation = self
            .document
            .as_ref()
            .map(|document| document.page_rotation(index))
            .unwrap_or(0);

        self.rotations.resize(index + 1, 0);
        self.rotations[index] = rotation;
        rotation
    }

    /// Marks a key as the most recently used.
    fn touch(&mut self, key: PageKey) {
        if let Some(position) = self.order.iter().position(|candidate| *candidate == key) {
            self.order.remove(position);
        }
        self.order.push(key);
    }

    /// Stores a page, dropping the least recently used bitmaps while the budget is exceeded.
    fn insert(&mut self, key: PageKey, page: RenderedPage) {
        self.touch(key);

        let bytes = weight(&page);
        if let Some(previous) = self.cache.insert(key, page) {
            self.stats.bytes = self.stats.bytes.saturating_sub(weight(&previous));
        }
        self.stats.bytes += bytes;

        // The last key is never evicted: it is the one that was just asked for, and dropping it
        // would mean a cache that cannot hold even the bitmap whose render it just paid for.
        while self.stats.bytes > self.budget && self.order.len() > 1 {
            let oldest = self.order.remove(0);
            if let Some(dropped) = self.cache.remove(&oldest) {
                self.stats.bytes = self.stats.bytes.saturating_sub(weight(&dropped));
                self.stats.evicted += 1;
            }
        }
    }

    /// The closest rung of the ladder already rendered for this page and document.
    ///
    /// Closest by *ratio*, not by difference: the ladder is geometric, so this treats 1024 and 2304
    /// as equally far from 1536 — picking by difference would always prefer the larger rung and its
    /// larger bitmap, which is the more expensive mistake of the two.
    fn nearest_key(&self, key: PageKey) -> Option<PageKey> {
        self.cache
            .keys()
            .filter(|candidate| {
                candidate.document == key.document
                    && candidate.page == key.page
                    && candidate.rotation == key.rotation
                    && candidate.annotations == key.annotations
                    && candidate.colour == key.colour
            })
            .min_by(|left, right| {
                let left = ratio(left.pixels, key.pixels);
                let right = ratio(right.pixels, key.pixels);
                left.partial_cmp(&right)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
    }

    /// Renders one page to a GPUI image, blocking until it is finished.
    ///
    /// The same job the pump slices, advanced with a budget generous enough to finish it: one code
    /// path for both, so a page that arrives from a blocking render and a page that arrives from the
    /// pump cannot disagree about what the page looks like.
    ///
    /// Only two callers are worth blocking for — the first page of a document, and the cheapest rung
    /// of a page the frames have nothing at all for — and both are a few milliseconds.
    fn render(&mut self, request: PageRequest) -> Result<RenderedPage> {
        self.begin(request)?;

        // Bounded, because a page that cannot finish rendering must not hang the app: Pdfium has no
        // timeout of its own, and a minute is far past anything a page has ever taken here.
        let give_up_at = Instant::now() + Duration::from_secs(60);

        loop {
            match self.advance(Duration::from_secs(1)) {
                Progress::Finished => break,
                Progress::Failed(message) => return Err(AppError::Pdf(message)),
                Progress::Unfinished if Instant::now() >= give_up_at => {
                    self.abandon();
                    return Err(AppError::Pdf(format!(
                        "page {} of {} took longer than a minute to render",
                        request.page + 1,
                        self.file_name()
                    )));
                }
                Progress::Unfinished => {}
            }
        }

        // `advance` has just filed it under the key this request makes.
        let key = self
            .key_for(request)
            .ok_or_else(|| AppError::Other(String::from("no PDF is open")))?;

        self.cache
            .get(&key)
            .cloned()
            .ok_or_else(|| AppError::Pdf(String::from("the rendered page was not cached")))
    }
}

#[cfg(test)]
mod tests;
