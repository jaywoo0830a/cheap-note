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
use pdfium_render::prelude::{PdfBitmapFormat, PdfDocument, PdfRenderConfig, Pdfium};

use crate::error::{AppError, Result};

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

/// The document the user is annotating, if one is open.
pub struct PdfDocumentView {
    /// The open document, borrowing the leaked `Pdfium` returned by [`bind_pdfium`].
    document: Option<PdfDocument<'static>>,
    /// Where the document was loaded from.
    path: Option<PathBuf>,
    /// Rendered pages, keyed by page index.
    cache: HashMap<usize, RenderedPage>,
    /// The bitmap width the cache was rendered at; a different width invalidates it.
    cached_pixel_width: u32,
}

impl Default for PdfDocumentView {
    fn default() -> Self {
        PdfDocumentView {
            document: None,
            path: None,
            cache: HashMap::new(),
            cached_pixel_width: 0,
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
            path: Some(path.to_path_buf()),
            cache: HashMap::new(),
            cached_pixel_width: render_pixel_width,
        };

        // Render the first page now, so an open PDF shows something other than a blank page.
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
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("untitled page"))
    }

    /// The rendered page at `index`, rendering and caching it on demand.
    ///
    /// A request at a different bitmap width than the cache was built at clears the cache: a
    /// page rendered for one zoom level is the wrong bitmap for another, and keeping both sets
    /// would trade memory for complexity this prototype does not need.
    pub fn render_page(&mut self, index: usize, pixel_width: u32) -> Result<RenderedPage> {
        if self.cached_pixel_width != pixel_width {
            self.cache.clear();
            self.cached_pixel_width = pixel_width;
        }

        if let Some(cached) = self.cache.get(&index) {
            return Ok(cached.clone());
        }

        let page = self.render(index, pixel_width.max(1))?;
        self.cache.insert(index, page.clone());
        Ok(page)
    }

    /// Renders one page to a GPUI image.
    fn render(&self, index: usize, pixel_width: u32) -> Result<RenderedPage> {
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
            .set_format(PdfBitmapFormat::BGRA)
            .set_reverse_byte_order(false);

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
}
