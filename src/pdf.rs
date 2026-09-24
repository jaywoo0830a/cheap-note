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
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::RenderImage;

use crate::error::{AppError, Result};
use crate::pdfium::{Document, RenderJob};

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
    /// The page's width in PDF points.
    pub point_width: f32,
    /// The page's height in PDF points.
    pub point_height: f32,
    /// The width this bitmap was actually rendered at, in pixels.
    ///
    /// The requested width when the cache had it, and a *different* one when this is standing in
    /// for a rung that is not ready yet: the caller compares the two to know whether what it is
    /// drawing is the real thing.
    pub pixels: u32,
}

impl RenderedPage {
    /// The logical size to draw this page at when it is shown `logical_width` logical pixels
    /// wide, preserving the page's own aspect ratio.
    pub fn display_size(&self, logical_width: f32) -> (f32, f32) {
        if self.point_width <= 0.0 {
            return (logical_width, logical_width);
        }
        let scale = logical_width / self.point_width;
        (logical_width, self.point_height * scale)
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
    /// The bitmap width, in pixels, on the quality ladder.
    pub pixels: u32,
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
        self.colour = if on {
            Colour::Grayscale
        } else {
            Colour::Bgra
        };
        self
    }
}

/// What one cached bitmap weighs: its pixels, four bytes each.
fn weight(page: &RenderedPage) -> u64 {
    let width = page.pixels as u64;
    let height = if page.point_width > 0.0 {
        (width as f32 * page.point_height / page.point_width) as u64
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
    /// Declared *first* because Rust drops fields in declaration order, and a render job holds a
    /// page handle that belongs to the document below it: the job has to go first, or Pdfium is
    /// asked to close a page of a document that is already closed.
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
        }
    }
}

impl PdfDocumentView {
    /// A view with no document: the app draws on a blank page.
    pub fn empty() -> Self {
        PdfDocumentView::default()
    }

    /// Opens a PDF from a file and renders its first page.
    ///
    /// `render_pixel_width` is the bitmap width the first page is rendered at; it should be the
    /// page width in logical pixels multiplied by the display's scale factor.
    ///
    /// The file is read here and handed to Pdfium as bytes: Pdfium's own file handling takes a path
    /// it interprets itself, and a note carries the document as bytes anyway — so there is one way
    /// in, and it is the one that does not care what the path is spelled like.
    pub fn open(path: &Path, render_pixel_width: u32) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|error| {
            AppError::Other(format!("{} could not be read: {error}", path.display()))
        })?;

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("document.pdf"));

        PdfDocumentView::open_bytes(name, bytes, render_pixel_width)
    }

    /// Opens a PDF that is already in memory — what a saved note carries — and renders its first
    /// page.
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

        // Render the first page now, so an open PDF shows something other than a blank page. This
        // is the one blocking render left in the app, and it is deliberate: the frame that follows
        // an open has nothing to show until it has happened.
        view.render_page(0, render_pixel_width)?;
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

    /// The page's own size in PDF points, without rendering it.
    ///
    /// The page bitmap carries this too, but only for the page that is rendered; this answers for
    /// any page, which is what the app needs to describe the sheet it is writing on.
    pub fn page_point_size(&self, index: usize) -> Option<(f32, f32)> {
        self.document.as_ref()?.page_point_size(index)
    }

    /// The bytes of the open document, for saving it into a note.
    ///
    /// The bytes Pdfium is reading from, which is why they are kept: what a note carries is the
    /// document as it was opened, byte for byte, and saving needs no second read of the file.
    pub fn document_bytes(&self) -> Result<Vec<u8>> {
        self.document
            .as_ref()
            .map(|document| document.bytes().to_vec())
            .ok_or_else(|| AppError::Other(String::from("no PDF is open")))
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
        let key = self.key_for(request.page, request.pixels)?;

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
        let key = self.key_for(request.page, request.pixels)?;
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
                    .key_for(request.page, request.pixels)
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
        })
    }

    /// The rendered page at `index`, rasterising it if the cache does not have it.
    ///
    /// Blocking, and deliberately so: it runs on the calling thread *inside* a frame, which is what
    /// the `pdf` timing measures. The frame path uses [`Self::page_for_frame`] and [`Self::plan`]
    /// instead, so a rasterisation that is not ready shows the previous rung of the ladder rather
    /// than stalling the frame that asked for it.
    pub fn render_page(&mut self, index: usize, pixel_width: u32) -> Result<RenderedPage> {
        let request = PageRequest::new(index, pixel_width);
        let Some(key) = self.key_for(request.page, request.pixels) else {
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
    /// The page's own rotation is part of the key, and reading it is a Pdfium call: cheap, but not
    /// free, and a page's rotation does not change under the app's feet — so it is read once per
    /// page and remembered.
    fn key_for(&mut self, page: usize, pixels: u32) -> Option<PageKey> {
        if self.document.is_none() {
            return None;
        }

        Some(PageKey {
            document: self.document_id,
            page,
            pixels: pixels.max(1),
            rotation: self.rotation_of(page),
            annotations: false,
            colour: Colour::Bgra,
        })
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
                left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Equal)
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
            .key_for(request.page, request.pixels)
            .ok_or_else(|| AppError::Other(String::from("no PDF is open")))?;

        self.cache
            .get(&key)
            .cloned()
            .ok_or_else(|| AppError::Pdf(String::from("the rendered page was not cached")))
    }
}


#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// A one-page A4 PDF with a grid of black rectangles on it, built object by object.
    ///
    /// Built here rather than taken from a library so the tests can assert on *content*: a blank
    /// page renders the same whatever a renderer gets wrong, and what these tests are here to catch
    /// is a wrong stride, format, rotation, background, or slice boundary. The grid is also what
    /// gives Pdfium a page with many *runs* to render, which is the only way a sliced render can
    /// show that its slices are real.
    ///
    /// The cross-reference table carries real byte offsets, because a table Pdfium silently repairs
    /// would let a broken fixture pass for the wrong reason.
    fn a_page_with_rectangles() -> Vec<u8> {
        let mut content = String::from("0 0 0 rg\n");
        for column in 0..4 {
            for row in 0..12 {
                // A grid of 300×40 pt blocks with gaps, spread over the page: the last one is the
                // rectangle the content assertions look for.
                content.push_str(&format!(
                    "{} {} 60 30 re f\n",
                    40 + column * 130,
                    40 + row * 60
                ));
            }
        }

        let objects = [
            String::from("<< /Type /Catalog /Pages 2 0 R >>"),
            String::from("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            String::from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R >>"),
            format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
        ];

        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();

        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
        }

        let xref = out.len();
        out.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
        out.push_str("0000000000 65535 f \n");
        for offset in &offsets {
            out.push_str(&format!("{offset:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));

        out.into_bytes()
    }

    /// The right to use Pdfium, for a test that rasterises.
    ///
    /// One process, one Pdfium library, and `cargo test` runs tests on parallel threads: Pdfium is
    /// not thread-safe, so the tests that render take this and the ones that do not (a cache key, a
    /// file format) do not. Without it the failures are intermittent and look like bugs in the
    /// renderer — which is exactly the kind of thing this suite is not allowed to produce.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        // A test that panicked while holding the lock must not poison it for the others: the panic
        // is reported by the test that caused it, and the rest still have to run.
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// The fixture on disk, or `None` when Pdfium is not available.
    ///
    /// Written once for the process and never deleted: the tests run in parallel threads, and a
    /// fixture that one of them removed would fail the others for a reason that has nothing to do
    /// with what they are testing. The name carries the process id, so two test runs at once do not
    /// tread on each other either.
    fn a_one_page_pdf() -> Option<PathBuf> {
        static FIXTURE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

        FIXTURE
            .get_or_init(|| {
                if !crate::pdfium::available() {
                    return None;
                }

                let path = std::env::temp_dir().join(format!(
                    "cheap-note-fixture-{}.pdf",
                    std::process::id()
                ));
                std::fs::write(&path, a_page_with_rectangles()).ok()?;
                Some(path)
            })
            .clone()
    }

    /// The colour at one pixel of a rendered page, as `(blue, green, red, alpha)`.
    ///
    /// The pixels are BGRA, which is why the channels are named rather than indexed as RGB.
    fn pixel(page: &RenderedPage, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let bytes = page.image.as_bytes(0).expect("the page has pixels");
        let stride = page.pixels as usize * 4;
        let offset = y as usize * stride + x as usize * 4;

        (
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        )
    }

    /// A document that arrives as bytes — what a saved note carries — opens and renders.
    ///
    /// This is the path a note takes: the PDF never touches the disk, so nothing is written to a
    /// temporary file and a note opens from anywhere the app can read it.
    #[test]
    fn a_document_opens_from_memory() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let bytes = std::fs::read(&path).expect("the fixture reads");
        let mut view = PdfDocumentView::open_bytes(String::from("from-memory.pdf"), bytes, 200)
            .expect("the document opens from bytes");

        assert_eq!(view.page_count(), 1);
        assert_eq!(view.file_name(), "from-memory.pdf");

        let page = view.render_page(0, 200).expect("the page renders");
        assert_eq!(page.pixels, 200, "the rung it was asked for");
        assert!(page.point_height > page.point_width, "A4 is portrait");

        // And its bytes come back out, which is what saving a note needs.
        assert_eq!(
            view.document_bytes().expect("the bytes are kept").len(),
            std::fs::metadata(&path).expect("the fixture is there").len() as usize,
            "a document opened from memory can be saved again without the file"
        );

    }

    /// Every field of the cache key is in it, because every field changes the pixels.
    ///
    /// This is the test that would have caught the original bug: keyed by page index alone, with a
    /// separate "the width changed, throw the cache away" rule, a zoom step re-rasterised a page
    /// that had just been on screen.
    #[test]
    fn a_cache_key_carries_everything_that_changes_the_pixels() {
        let base = PageKey {
            document: 1,
            page: 0,
            pixels: 1_024,
            rotation: 0,
            annotations: false,
            colour: Colour::Bgra,
        };

        for variant in [
            PageKey {
                document: 2,
                ..base
            },
            PageKey { page: 1, ..base },
            PageKey {
                pixels: 1_536,
                ..base
            },
            PageKey {
                rotation: 1,
                ..base
            },
            PageKey {
                annotations: true,
                ..base
            },
            PageKey {
                colour: Colour::Grayscale,
                ..base
            },
        ] {
            assert_ne!(
                variant, base,
                "a field that changes the pixels must change the key"
            );
        }
    }

    /// A rung of the ladder that has been rendered once is never rendered again.
    #[test]
    fn a_zoom_step_returns_to_a_cached_rung() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut view = PdfDocumentView::open(&path, 1_024).expect("the document opens");
        view.render_page(0, 1_536).expect("the second rung");
        let rendered = view.stats().rendered;

        // Out to the first rung, and back to the second: neither is a rasterisation.
        let first = view.render_page(0, 1_024).expect("the first rung again");
        let second = view.render_page(0, 1_536).expect("the second rung again");

        assert_eq!(
            view.stats().rendered,
            rendered,
            "a rung already rasterised must not be rasterised again"
        );
        assert_eq!(first.pixels, 1_024);
        assert_eq!(second.pixels, 1_536);
    }

    /// The frame path never rasterises, whatever it is asked for.
    #[test]
    fn the_frame_path_never_rasterises() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut view = PdfDocumentView::open(&path, 1_024).expect("the document opens");
        let rendered = view.stats().rendered;

        // A rung nobody has rendered: the frame gets the one that is there, and what it asked for
        // is left owed to the caller.
        let wanted = PageRequest::new(0, 3_456);
        let before = view.stats();
        let page = view.page_for_frame(wanted).expect("the cached rung stands in");

        assert_eq!(page.pixels, 1_024, "a placeholder, not the rung asked for");
        assert!(view.plan(wanted).is_some(), "and the rung asked for is owed");
        assert_eq!(view.stats().rendered, rendered, "the frame rasterised nothing");
        assert_eq!(view.stats().placeholders, before.placeholders + 1);
        assert_eq!(view.stats().misses, before.misses + 1);
    }

    /// Inside the budget the cache keeps everything; past it the oldest bitmap goes first.
    #[test]
    fn the_least_recently_used_bitmap_goes_first() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut view = PdfDocumentView::open(&path, 200).expect("the document opens");

        // Room for about two of these bitmaps, rather than the 128 MB the app allows.
        let rung = weight(&view.render_page(0, 200).expect("a rung"));
        view.budget = rung * 2;

        view.render_page(0, 300).expect("a second rung");
        view.render_page(0, 400).expect("a third rung");

        assert!(view.stats().evicted >= 1, "the oldest bitmap went");

        let newest = view.render_page(0, 400).expect("the newest rung");
        assert_eq!(newest.pixels, 400, "the bitmap in use stays");
        assert!(
            view.stats().bytes <= view.budget + weight(&newest),
            "the budget is held, but for the one bitmap that is in use: {} bytes",
            view.stats().bytes
        );
        assert!(
            view.plan(PageRequest::new(0, 200)).is_some(),
            "and the rung that went is the least recently used one"
        );
    }


    use super::*;

    /// The whole PDF path end to end: find the library, open a document through the same function
    /// the app uses, and rasterise a page.
    ///
    /// This is the only test that needs `pdfium.dll`. It skips itself when the library is not
    /// present — a missing optional DLL is a supported state, not a test failure — so the suite
    /// stays meaningful on a machine without the vendored library.
    ///
    /// The assertions are about *content*: the rectangle in the fixture is black and the paper
    /// around it is white, where the page's own coordinate system puts them. A renderer that got the
    /// stride, the byte order, the background or the vertical flip wrong would pass a test that only
    /// counted pixels, and would not light up the sheet the way this one does.
    #[test]
    fn a_document_round_trips_through_pdfium_and_renders() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut view = PdfDocumentView::open(&path, 300).expect("the document opens");
        assert_eq!(view.page_count(), 1);
        assert_eq!(
            view.file_name(),
            path.file_name().expect("the fixture has a name").to_string_lossy()
        );

        let page = view.render_page(0, 300).expect("the page renders");
        assert!(page.point_width > 0.0, "an A4 page has a width");
        assert!(page.point_height > page.point_width, "A4 is portrait");
        assert_eq!(page.pixels, 300, "the bitmap is the width that was asked for");

        // A second request at the same width must be served from the cache, not re-rasterised.
        let cached = view.render_page(0, 300).expect("the cached page");
        assert_eq!(
            Arc::as_ptr(&page.image),
            Arc::as_ptr(&cached.image),
            "the same bitmap comes back"
        );

        // The grid: 60×30 pt blocks whose lower-left corners are at 40 + column·130, 40 + row·60. At
        // 300 px wide that is 0.504 px/pt, and the page is measured from the top, so a block in the
        // bottom row is 56..71 px down and a gap between columns is 50..86 px across.
        let width = page.pixels;
        let height = (page.pixels as f32 * page.point_height / page.point_width) as u32;
        assert_eq!(height, 424, "300 px wide is 424 px tall for A4");

        let inside = pixel(&page, 35, 63);
        assert!(
            inside.0 < 40 && inside.1 < 40 && inside.2 < 40,
            "a block of the grid is black, where the page puts it: got {inside:?}"
        );
        assert_eq!(inside.3, 255, "and it is opaque");

        let paper = pixel(&page, 70, 63);
        assert!(
            paper.0 > 240 && paper.1 > 240 && paper.2 > 240,
            "the gap between two blocks is paper, not a hole in the window: got {paper:?}"
        );

        let corner = pixel(&page, width - 5, height - 5);
        assert!(
            corner.0 > 240 && corner.1 > 240 && corner.2 > 240,
            "and so is the corner: got {corner:?}"
        );

    }

    /// A render sliced across several calls is the same page as one that was not.
    ///
    /// This is what the progressive path is for, and it is the property that makes it safe to use:
    /// the budget changes *when* the work happens, never what it produces. A slice boundary that
    /// lost a band, or a bitmap read out before it was finished, shows up here.
    #[test]
    fn a_sliced_render_is_the_whole_page() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let whole = {
            let mut view = PdfDocumentView::open(&path, 1_200).expect("the document opens");
            view.render_page(0, 1_200).expect("the page renders")
        };

        let mut view = PdfDocumentView::open(&path, 1_200).expect("the document opens");
        view.begin(PageRequest::new(0, 1_200)).expect("the render starts");

        // Four milliseconds at a time: several slices for a page this size, and the same budget the
        // pump uses between frames.
        let mut slices = 0;
        loop {
            match view.advance(Duration::from_millis(4)) {
                Progress::Unfinished => slices += 1,
                Progress::Finished => break,
                other => panic!("the render ended as {other:?}"),
            }

            assert!(slices < 1_000, "the render is not making progress");
        }

        let sliced = view
            .render_page(0, 1_200)
            .expect("the finished page is cached");
        assert_eq!(sliced.pixels, whole.pixels);
        assert_eq!(
            sliced.image.as_bytes(0),
            whole.image.as_bytes(0),
            "a sliced render must be the same page, byte for byte"
        );
        assert_eq!(
            view.stats().rendered,
            2,
            "and the sliced one counted as a render"
        );
    }

    /// A slice with no time in it renders nothing, and says so.
    ///
    /// The budget is what keeps the pump's wake short, so "no time" has to mean "no work": Pdfium
    /// is asked before the first piece of the page, which is why this can be answered without
    /// rendering anything at all.
    #[test]
    fn a_slice_with_no_budget_renders_nothing() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut view = PdfDocumentView::open(&path, 1_200).expect("the document opens");
        view.begin(PageRequest::new(0, 3_456))
            .expect("a bigger render starts");
        assert!(view.rendering(), "and it is in flight");

        let started = Instant::now();
        let progress = view.advance(Duration::ZERO);

        assert_eq!(
            progress,
            Progress::Unfinished,
            "no time was given, so none was used"
        );
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "a slice with no budget must not render the page"
        );
        assert!(view.rendering(), "and the job is still there to carry on");
    }

    /// Abandoning a render in flight releases it, and says so.
    #[test]
    fn an_abandoned_render_is_counted_and_forgotten() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut view = PdfDocumentView::open(&path, 1_200).expect("the document opens");
        let before = view.stats();

        view.begin(PageRequest::new(0, 3_456))
            .expect("a render starts");
        assert!(view.rendering());

        view.abandon();
        assert!(!view.rendering(), "the job is gone");
        assert_eq!(view.stats().cancelled, before.cancelled + 1, "and it was counted");
        assert_eq!(
            view.stats().rendered, before.rendered,
            "an abandoned render is not a rendered page"
        );
    }

    ///
    /// This is the only part of a frame that can take *milliseconds*, and it runs on the frame's
    /// own thread: whenever the requested bitmap width changes — opening a document, turning a
    /// page, zooming, resizing to a different DPI — the next frame waits for Pdfium. The cache is
    /// what keeps it from being a per-frame cost: an unchanged width is a map lookup.
    ///
    /// Run `cargo test --release -- --nocapture measures_the_pdf_costs`.
    /// What rasterising a page costs, what the cache saves, and what slicing costs.
    ///
    /// Rasterising is the one part of a frame that can take *milliseconds*, which is why it is
    /// sliced: this measures both the whole cost and how many slices a page takes at the budget the
    /// pump uses, so the pump's budget can be argued about with numbers rather than a guess.
    ///
    /// Run `cargo test --release -- --nocapture measures_the_pdf_costs`.
    #[test]
    fn measures_the_pdf_costs() {
        let _pdfium = exclusive();

        let Some(path) = a_one_page_pdf() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        eprintln!("\n── pdf, measured ──────────────────────────────────────────────");
        for width in [720u32, 1_440, 2_880, 5_760] {
            // Opened at a different width, so the request below is a genuine rasterisation rather
            // than the cache hit that `open` has already left behind.
            let mut view = PdfDocumentView::open(&path, 100).expect("the document reopens");

            let started = std::time::Instant::now();
            let page = view.render_page(0, width).expect("the page renders");
            let rasterised = started.elapsed();

            // And the same page in slices of the pump's budget, counting what it takes. The setup is
            // measured apart from the slices, because it is the part no budget can interrupt: it
            // allocates the bitmap and writes the paper into it.
            let started = std::time::Instant::now();
            view.begin(PageRequest::new(0, width)).expect("a sliced render");
            let setup = started.elapsed();

            let started = std::time::Instant::now();
            let mut slices = 0u32;
            let mut longest = std::time::Duration::ZERO;
            loop {
                let slice = std::time::Instant::now();
                match view.advance(SLICE_BUDGET) {
                    Progress::Unfinished => {
                        slices += 1;
                        longest = longest.max(slice.elapsed());
                    }
                    Progress::Finished => {
                        slices += 1;
                        longest = longest.max(slice.elapsed());
                        break;
                    }
                    other => panic!("the sliced render ended as {other:?}"),
                }
            }
            let sliced = started.elapsed();

            let started = std::time::Instant::now();
            let rounds = 1_000;
            for _ in 0..rounds {
                std::hint::black_box(view.render_page(0, width).expect("the cached page"));
            }
            let cached = started.elapsed() / rounds;

            let height = (width as f32 * page.point_height / page.point_width.max(1.0)) as u64;
            let megabytes = (width as u64 * height * 4) / (1024 * 1024);
            eprintln!(
                "  a page {width:>4} px wide, {megabytes:>3} MB  rasterise {rasterised:>9.1?}   or setup {setup:>8.1?} + {slices:>3} slices, longest {longest:>8.1?} ({sliced:>8.1?} total)   then {cached:>7.1?} per frame"
            );

            // A slice is not the whole render: the paper write and the allocation happen in the
            // setup, and what is left is what a budget can stop between.
            assert!(
                longest < rasterised,
                "the longest slice ({longest:?}) was no shorter than the whole render ({rasterised:?})"
            );
            assert!(
                cached.as_micros() < 500,
                "a cached page cost {cached:?} per frame; it is meant to be a map lookup"
            );
        }
        eprintln!("───────────────────────────────────────────────────────────────");

        // What the cache did with all of that, and what the first frame of a page costs when the
        // only thing on offer is the cheapest rung — the number that decides whether a page turn
        // is a stall or a soft page for a frame or two.
        let mut view = PdfDocumentView::open(&path, 100).expect("the document reopens");
        let started = std::time::Instant::now();
        let preview = view
            .render_page(0, 1_024)
            .expect("the cheapest rung renders");
        let preview_time = started.elapsed();

        // Every frame after the first is a hit, and the ratio says so.
        for _ in 0..100 {
            std::hint::black_box(view.page_for_frame(PageRequest::new(0, 1_024)));
        }

        eprintln!(
            "  the preview rung ({} px wide, {} KB)  {preview_time:>9.1?}",
            preview.pixels,
            weight(&preview) / 1024
        );
        eprintln!("  {}", view.stats().summary());
        eprintln!("───────────────────────────────────────────────────────────────\n");

        assert!(
            preview_time < std::time::Duration::from_millis(5),
            "the preview rung took {preview_time:?}; it is meant to be the cheap one"
        );

    }
}
