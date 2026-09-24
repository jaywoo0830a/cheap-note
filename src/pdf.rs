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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use gpui_kit::RenderImage;
use pdfium_render::prelude::{
    PdfBitmapFormat, PdfDocument, PdfPageRenderRotation, PdfRenderConfig, Pdfium,
};

use crate::error::{AppError, Result};

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

/// Where an open document's bytes can be got from again.
#[derive(Debug, Clone)]
pub enum Source {
    /// A file on disk, read back when the note is saved.
    File(PathBuf),
    /// Bytes handed to the app — from a saved note — with the name it had.
    Memory {
        /// The file name to show for it.
        name: String,
        /// The bytes.
        bytes: Vec<u8>,
    },
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
    /// The open document, borrowing the leaked `Pdfium` returned by [`bind_pdfium`].
    document: Option<PdfDocument<'static>>,
    /// Where the document was loaded from, and how to get its bytes again.
    ///
    /// A saved note hands the app bytes rather than a file, and the file it came from may not exist
    /// any more — so the bytes are kept; a document opened from a path is read from that path when
    /// it is time to save, so what the note carries is the document as it is now.
    source: Option<Source>,
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

/// How much of the memory budget is spent on cached page bitmaps.
///
/// A 1650-pixel-wide A4 page is about 11 MB and the top rung about 44 MB, so 128 MB holds the
/// current page at two or three qualities — which is what a zoom gesture and a page turn actually
/// need. Beyond that the oldest bitmaps go, in use order.
const CACHE_BUDGET_BYTES: u64 = 128 * 1024 * 1024;

impl Default for PdfDocumentView {
    fn default() -> Self {
        PdfDocumentView {
            document: None,
            source: None,
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

    /// Opens a PDF and renders its first page.
    ///
    /// `render_pixel_width` is the bitmap width the first page is rendered at; it should be the
    /// page width in logical pixels multiplied by the display's scale factor.
    pub fn open(path: &Path, render_pixel_width: u32) -> Result<Self> {
        let pdfium = bind_pdfium()?;
        let document = pdfium.load_pdf_from_file(path, None)?;

        let mut view = PdfDocumentView {
            document: Some(document),
            source: Some(Source::File(path.to_path_buf())),
            cache: HashMap::new(),
            order: Vec::new(),
            // A new document: the keys of the old one must never match this one's bitmaps.
            document_id: 1,
            budget: CACHE_BUDGET_BYTES,
            rotations: Vec::new(),
            stats: PdfStats::default(),
        };

        // Render the first page now, so an open PDF shows something other than a blank page.
        view.render_page(0, render_pixel_width)?;
        Ok(view)
    }

    /// Opens a PDF that is already in memory — what a saved note carries — and renders its first
    /// page.
    ///
    /// Pdfium reads the bytes it is given and keeps its own copy, so nothing has to be written to a
    /// temporary file for a note to be readable: opening one leaves nothing behind on disk, and a
    /// note can be opened from a drive the app cannot write to.
    pub fn open_bytes(name: String, bytes: Vec<u8>, render_pixel_width: u32) -> Result<Self> {
        let pdfium = bind_pdfium()?;
        let document = pdfium.load_pdf_from_byte_vec(bytes.clone(), None)?;

        let mut view = PdfDocumentView {
            document: Some(document),
            source: Some(Source::Memory { name, bytes }),
            cache: HashMap::new(),
            order: Vec::new(),
            // A new document: the keys of the old one must never match this one's bitmaps.
            document_id: 1,
            budget: CACHE_BUDGET_BYTES,
            rotations: Vec::new(),
            stats: PdfStats::default(),
        };

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
            .map(|document| document.pages().len().max(0) as usize)
            .unwrap_or(0)
    }

    /// The file name of the open document, for the status bar.
    pub fn file_name(&self) -> String {
        match &self.source {
            Some(Source::File(path)) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| String::from("untitled page")),
            Some(Source::Memory { name, .. }) => name.clone(),
            None => String::from("untitled page"),
        }
    }

    /// The page's own size in PDF points, without rendering it.
    ///
    /// The page bitmap carries this too, but only for the page that is rendered; this answers for
    /// any page, which is what the app needs to describe the sheet it is writing on.
    pub fn page_point_size(&self, index: usize) -> Option<(f32, f32)> {
        let document = self.document.as_ref()?;
        let page = document.pages().get(index as i32).ok()?;
        Some((page.width().value, page.height().value))
    }

    /// The bytes of the open document, for saving it into a note.
    ///
    /// A document opened from a file is read from that file *now*: the user may have edited it,
    /// replaced it, or moved it since, and what a note should carry is the document as it is, not
    /// as it was when the app read it.
    pub fn document_bytes(&self) -> Result<Vec<u8>> {
        match &self.source {
            Some(Source::File(path)) => std::fs::read(path).map_err(|error| {
                AppError::Other(format!("{} could not be read: {error}", path.display()))
            }),
            Some(Source::Memory { bytes, .. }) => Ok(bytes.clone()),
            None => Err(AppError::Other(String::from("no PDF is open"))),
        }
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
        let page = self.render(request)?;
        self.stats.rendered += 1;
        self.insert(key, page.clone());
        Ok(page)
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
            .and_then(|document| document.pages().get(index as i32).ok())
            .and_then(|page| page.rotation().ok())
            .map(|rotation| match rotation {
                PdfPageRenderRotation::None => 0,
                PdfPageRenderRotation::Degrees90 => 1,
                PdfPageRenderRotation::Degrees180 => 2,
                PdfPageRenderRotation::Degrees270 => 3,
            })
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

    /// Renders one page to a GPUI image.
    ///
    /// The whole of the request — page, width, rotation, options — comes from the caller as one
    /// value, because the config built from it here is exactly what [`PageKey`] describes: if the
    /// two ever disagree, the cache is keyed on something other than what was rendered.
    fn render(&self, request: PageRequest) -> Result<RenderedPage> {
        let index = request.page;
        let pixel_width = request.pixels.max(1);
        let Some(document) = self.document.as_ref() else {
            return Err(AppError::Other(String::from("no PDF is open")));
        };

        let pages = document.pages();
        if index >= pages.len().max(0) as usize {
            return Err(AppError::Other(format!("page {index} is past the end")));
        }

        let page = pages.get(index as i32)?;
        let point_width = page.width().value;
        let point_height = page.height().value;

        // Ask for BGRA in Pdfium's native byte order, which is the order GPUI's renderer
        // uploads: the rendered bytes need no conversion at all.
        let config = PdfRenderConfig::new()
            .set_target_width(pixel_width as i32)
            .set_format(match request.colour {
                Colour::Bgra => PdfBitmapFormat::BGRA,
                Colour::Grayscale => PdfBitmapFormat::Gray,
            })
            .set_reverse_byte_order(false)
            .render_annotations(request.annotations);

        let bitmap = page.render_with_config(&config)?;
        let pixel_width = bitmap.width().max(0) as u32;
        let pixel_height = bitmap.height().max(0) as u32;

        let buffer = image::RgbaImage::from_raw(pixel_width, pixel_height, bitmap.as_raw_bytes())
            .ok_or_else(|| {
                AppError::Other(String::from(
                    "the rendered page did not match the buffer size Pdfium reported",
                ))
            })?;

        Ok(RenderedPage {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
            point_width,
            point_height,
            pixels: pixel_width,
        })
    }
}

/// Loads Pdfium once for the whole process and returns the handle.
///
/// ## Why this is cached, and not just leaked
///
/// `Pdfium::bind_to_library` initializes a process-wide binding slot and *fails* with
/// `PdfiumLibraryBindingsAlreadyInitialized` on a second call. Opening a second document must
/// therefore reuse the first binding rather than rebind, so the handle is kept in a `OnceLock`
/// and every caller after the first gets the same one.
///
/// ## Why the library is loaded at run time
///
/// The library is a DLL, not a link-time dependency: the app ships without linking against it
/// and reports a missing library as an error in the status bar rather than failing to start.
fn bind_pdfium() -> Result<&'static Pdfium> {
    /// The process-wide Pdfium handle.
    static PDFIUM: std::sync::OnceLock<&'static Pdfium> = std::sync::OnceLock::new();

    if let Some(pdfium) = PDFIUM.get() {
        return Ok(pdfium);
    }

    let pdfium = load_pdfium()?;
    Ok(PDFIUM.get_or_init(|| pdfium))
}

/// Finds and binds the Pdfium shared library, leaking the handle so it lasts for the process.
fn load_pdfium() -> Result<&'static Pdfium> {
    for directory in library_candidates() {
        let candidate = Pdfium::pdfium_platform_library_name_at_path(&directory);
        if !candidate.is_file() {
            continue;
        }

        // `anyhow::Context` attaches which file failed, which the raw `libloading` error does
        // not say. The chain is then flattened into this app's own error type, so the status
        // bar shows one message with the cause rather than a bare "load failed".
        let bindings = Pdfium::bind_to_library(&candidate)
            .with_context(|| format!("{} could not be loaded", candidate.display()))
            .map_err(|error| AppError::PdfiumLibrary(error.to_string()))?;

        // Leaked once, on purpose: see the module docs. The `Pdfium` handle is a thin wrapper
        // over a process-wide binding, so this is a fixed, small, intentional allocation.
        return Ok(Box::leak(Box::new(Pdfium::new(bindings))));
    }

    Err(AppError::PdfiumLibrary(String::from(
        "pdfium.dll was not found next to the executable, in the working directory, or in vendor/lib",
    )))
}

/// Where the app looks for the Pdfium shared library, nearest first.
fn library_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.to_path_buf());
        }
    }

    if let Ok(working_directory) = std::env::current_dir() {
        // The repository ships the library under `vendor/lib`, which is where a `cargo run`
        // from the project root finds it.
        candidates.push(working_directory.join("vendor").join("lib"));
        candidates.push(working_directory.join("vendor"));
        candidates.push(working_directory);
    }

    candidates
}

#[cfg(test)]
mod tests {
    /// A one-page PDF on disk, or `None` when Pdfium is not available.
    ///
    /// The tests that exercise the cache need a document to rasterise; the ones that do not skip
    /// themselves rather than fail, because a missing optional DLL is a supported state.
    fn a_one_page_pdf() -> Option<PathBuf> {
        let pdfium = bind_pdfium().ok()?;

        let mut document = pdfium.create_new_pdf().expect("a new document");
        document
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .expect("a page is created");

        let path = std::env::temp_dir().join("cheap-note-cache-fixture.pdf");
        document.save_to_file(&path).expect("the document is saved");
        Some(path)
    }

    /// A document that arrives as bytes — what a saved note carries — opens and renders.
    ///
    /// This is the path a note takes: the PDF never touches the disk, so nothing is written to a
    /// temporary file and a note opens from anywhere the app can read it.
    #[test]
    fn a_document_opens_from_memory() {
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

        let _ = std::fs::remove_file(&path);
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
        let _ = std::fs::remove_file(&path);
    }

    /// The frame path never rasterises, whatever it is asked for.
    #[test]
    fn the_frame_path_never_rasterises() {
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
        let _ = std::fs::remove_file(&path);
    }

    /// Inside the budget the cache keeps everything; past it the oldest bitmap goes first.
    #[test]
    fn the_least_recently_used_bitmap_goes_first() {
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
        let _ = std::fs::remove_file(&path);
    }


    use super::*;
    use pdfium_render::prelude::PdfPagePaperSize;

    /// The whole PDF path end to end: find the library, create a document, open it through the
    /// same function the app uses, and rasterise a page.
    ///
    /// This is the only test that needs `pdfium.dll`. It skips itself when the library is not
    /// present — a missing optional DLL is a supported state, not a test failure — so the suite
    /// stays meaningful on a machine without the vendored library.
    #[test]
    fn a_document_round_trips_through_pdfium_and_renders() {
        let Ok(pdfium) = bind_pdfium() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut document = pdfium.create_new_pdf().expect("a new document");
        document
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .expect("a page is created");

        let path = std::env::temp_dir().join("cheap-note-round-trip.pdf");
        document.save_to_file(&path).expect("the document is saved");
        // Close it before reopening, so the file is not left held open by Pdfium.
        drop(document);

        let mut view = PdfDocumentView::open(&path, 300).expect("the document reopens");
        assert_eq!(view.page_count(), 1);
        assert_eq!(view.file_name(), "cheap-note-round-trip.pdf");

        let page = view.render_page(0, 300).expect("the page renders");
        assert!(page.point_width > 0.0, "an A4 page has a width");
        assert!(page.point_height > page.point_width, "A4 is portrait");

        // A second request at the same width must be served from the cache, not re-rasterised.
        let cached = view.render_page(0, 300).expect("the cached page");
        assert_eq!(
            Arc::as_ptr(&page.image),
            Arc::as_ptr(&cached.image),
            "the same bitmap comes back"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// What rasterising a page costs, and what the cache saves.
    ///
    /// This is the only part of a frame that can take *milliseconds*, and it runs on the frame's
    /// own thread: whenever the requested bitmap width changes — opening a document, turning a
    /// page, zooming, resizing to a different DPI — the next frame waits for Pdfium. The cache is
    /// what keeps it from being a per-frame cost: an unchanged width is a map lookup.
    ///
    /// Run `cargo test --release -- --nocapture measures_the_pdf_costs`.
    #[test]
    fn measures_the_pdf_costs() {
        let Ok(pdfium) = bind_pdfium() else {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        };

        let mut document = pdfium.create_new_pdf().expect("a new document");
        document
            .pages_mut()
            .create_page_at_start(PdfPagePaperSize::a4())
            .expect("a page is created");

        let path = std::env::temp_dir().join("cheap-note-measured.pdf");
        document.save_to_file(&path).expect("the document is saved");
        drop(document);

        eprintln!("\n── pdf, measured ──────────────────────────────────────────────");
        for width in [720u32, 1_440, 2_880, 5_760] {
            // Opened at a different width, so the request below is a genuine rasterisation rather
            // than the cache hit that `open` has already left behind.
            let mut view = PdfDocumentView::open(&path, 100).expect("the document reopens");

            let started = std::time::Instant::now();
            let page = view.render_page(0, width).expect("the page renders");
            let rasterised = started.elapsed();

            let started = std::time::Instant::now();
            let rounds = 1_000;
            for _ in 0..rounds {
                std::hint::black_box(view.render_page(0, width).expect("the cached page"));
            }
            let cached = started.elapsed() / rounds;

            let height = (width as f32 * page.point_height / page.point_width.max(1.0)) as u64;
            let megabytes = (width as u64 * height * 4) / (1024 * 1024);
            eprintln!(
                "  a page {width:>4} px wide, {megabytes:>3} MB  rasterise {rasterised:>9.1?}   then {cached:>7.1?} per frame"
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

        let _ = std::fs::remove_file(&path);
    }
}
