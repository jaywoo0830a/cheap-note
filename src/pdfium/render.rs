//! Pdfium's progressive render: a job that owns a page and a bitmap, and stops on a budget.
//!
//! ## Why progressive rather than one call
//!
//! A full-quality page at the top of the app's quality ladder costs tens of milliseconds — a
//! measured 17 ms at 2880 px wide and 67 ms at 5760. Synchronous, that is a frame that never
//! arrives; sliced by [`RenderJob::advance`] with a few-millisecond budget, it is the same work
//! spread over a few pump wakes, with the frames in between showing the rung that is already
//! cached. `NeedToPauseNow` is what makes the slices real: Pdfium asks it between the pieces of a
//! page, so a slice ends when its budget does rather than when the page does.
//!
//! Cancellation follows from the slicing, and needs no flag of its own: the app owns the job, and a
//! render only runs *inside* an `advance` call — so a view that has moved on simply never advances
//! again and drops the job, which is where Pdfium releases the page and the unfinished bitmap.
//!
//! ## Every handle has one owner
//!
//! The job owns the page (through a [`Page`], which keeps the document alive as well), the bitmap
//! and the pause interface. Dropping the job closes the render, destroys the bitmap, closes the
//! page, and only then releases the document — in that order. The explicit `FPDF_RenderPage_Close`
//! is not decoration: Pdfium keeps resources for a progressive render until it is called, whether
//! the render finished or was abandoned, and a job that skipped it would leak them on every
//! cancelled zoom.

use std::ffi::c_int;
use std::time::{Duration, Instant};

use super::api::{api, FpdfBitmap, Pause};
use super::document::{Document, Page};
use crate::error::{AppError, Result};
use crate::pages::Quarters;

/// What one call to [`RenderJob::advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The slice ended on its budget. Call `advance` again to carry on.
    Unfinished,
    /// The page is complete: [`RenderJob::pixels`] has it.
    Finished,
    /// Pdfium refused, or the render failed. The message says which.
    Failed(String),
}

/// The colour format a page is rendered in.
///
/// Both are *requests*, not details: the format is part of what identifies the resulting pixels, so
/// it belongs in the cache key with the page and the size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Four bytes per pixel, blue-green-red-alpha: Pdfium's own byte order, and the one GPUI's
    /// renderer uploads, so the bytes need no conversion on the way to the screen.
    Bgra,
    /// One byte per pixel. Pdfium does the conversion, which is why it is worth asking for rather
    /// than converting the colour result afterwards.
    Grayscale,
}

impl Format {
    /// The `FPDFBitmap_*` constant that asks for this format.
    fn code(self) -> c_int {
        match self {
            Format::Bgra => BITMAP_BGRA,
            Format::Grayscale => BITMAP_GRAY,
        }
    }
}

/// Pdfium's progressive status codes, from `fpdf_progressive.h`.
const RENDER_TO_BE_CONTINUED: c_int = 1;
/// The page is fully rendered.
const RENDER_DONE: c_int = 2;
/// The render failed and cannot be continued.
const RENDER_FAILED: c_int = 3;

/// Draw the page's own annotations into the bitmap.
const RENDER_ANNOTATIONS: c_int = 1;

/// Pdfium's bitmap formats, as `FPDFBitmap_*` defines them.
const BITMAP_GRAY: c_int = 1;
/// Blue-green-red-alpha, the format GPUI's renderer uploads.
const BITMAP_BGRA: c_int = 4;

/// What a page's bitmap is filled with before anything is drawn on it: opaque white, in the
/// bitmap's own byte order (which is why no channel can be told from another here).
const PAPER: u32 = 0xffff_ffff;

/// The largest bitmap a job will ask Pdfium to allocate, in bytes of its BGRA form.
///
/// Not a quality setting: the app's ladder tops out around 44 MB for an A4 page, so half a
/// gigabyte is far beyond anything a frame will show. It is a bound on what a malformed page size
/// and a large reader zoom can ask the allocator for at once, which is the difference between a
/// failed render and a process that cannot continue.
const MAX_BITMAP_BYTES: u64 = 512 * 1024 * 1024;

/// What `NeedToPauseNow` reads: when this slice of work is over.
///
/// A single field behind a single pointer on purpose: the callback cannot allocate, cannot block,
/// and has to answer from one load, because it runs inside Pdfium's render loop.
struct Control {
    /// The moment this slice of work is over.
    deadline: Instant,
}

/// The callback Pdfium calls between the pieces of a page.
unsafe extern "C" fn need_to_pause_now(pause: *mut Pause) -> c_int {
    if pause.is_null() {
        return 0;
    }

    let pause = unsafe { &mut *pause };
    if pause.user.is_null() {
        return 0;
    }

    let control = unsafe { &*(pause.user as *const Control) };
    if Instant::now() >= control.deadline {
        1
    } else {
        0
    }
}

/// A bitmap Pdfium renders into, and the format it was created with.
///
/// Destroying the bitmap is what ends a render that has not finished, so the owner's field order is
/// part of the cancellation story: a job declares its bitmap before its page, which is the order
/// Rust drops them in.
struct Bitmap {
    /// The handle Pdfium handed out.
    handle: FpdfBitmap,
    /// The format the bytes are in, which decides how they are read out.
    format: Format,
}

impl Bitmap {
    /// Asks Pdfium for a bitmap of this size, in this format.
    ///
    /// `None` means Pdfium refused: the dimensions were rejected, or the allocator could not supply
    /// the buffer. Either way the caller gets a sentence rather than a handle it cannot use.
    fn new(width: c_int, height: c_int, format: Format) -> Option<Self> {
        let api = api().ok()?;
        let handle = unsafe {
            (api.bitmap_create_ex)(width, height, format.code(), std::ptr::null_mut(), 0)
        };

        (!handle.is_null()).then_some(Bitmap { handle, format })
    }

    /// Writes opaque paper across the whole bitmap.
    fn fill(&self) -> bool {
        let Ok(api) = api() else {
            return false;
        };
        let width = unsafe { (api.bitmap_width)(self.handle) };
        let height = unsafe { (api.bitmap_height)(self.handle) };

        width > 0
            && height > 0
            && unsafe { (api.bitmap_fill)(self.handle, 0, 0, width, height, PAPER) != 0 }
    }

    /// Copies the rendered pixels out: width, height, and rows of BGRA.
    ///
    /// Copied rather than borrowed because the bitmap belongs to Pdfium, and the app hands the
    /// pixels to GPUI, which keeps them for as long as the image is on screen. A grayscale page is
    /// widened to BGRA here, because that is what the renderer uploads: its one channel becomes all
    /// three, at full brightness, with an opaque alpha.
    ///
    /// Every size is checked against the stride before any pointer arithmetic: Pdfium owns these
    /// values, but a bad one must produce no pixels rather than a slice that reads past the buffer.
    fn pixels(&self) -> Option<(u32, u32, Vec<u8>)> {
        let api = api().ok()?;

        let width = unsafe { (api.bitmap_width)(self.handle) };
        let height = unsafe { (api.bitmap_height)(self.handle) };
        let stride = unsafe { (api.bitmap_stride)(self.handle) };
        let buffer = unsafe { (api.bitmap_buffer)(self.handle) } as *const u8;

        if width <= 0 || height <= 0 || stride <= 0 || buffer.is_null() {
            return None;
        }

        let width = width as usize;
        let height = height as usize;
        let stride = stride as usize;

        // How many bytes of a row are pixels rather than Pdfium's alignment padding. The stride may
        // be wider than a row, and handing GPUI the padding as pixels would shear the page.
        let source_row = match self.format {
            Format::Bgra => width.checked_mul(4)?,
            Format::Grayscale => width,
        };
        if source_row > stride {
            return None;
        }

        let length = stride.checked_mul(height)?;
        let target_row = width.checked_mul(4)?;
        let mut pixels = Vec::with_capacity(target_row.checked_mul(height)?);
        let bytes = unsafe { std::slice::from_raw_parts(buffer, length) };

        for line in bytes.chunks_exact(stride) {
            let row = &line[..source_row];

            match self.format {
                Format::Bgra => pixels.extend_from_slice(row),
                Format::Grayscale => {
                    for value in row {
                        pixels.extend_from_slice(&[*value, *value, *value, 0xff]);
                    }
                }
            }
        }

        Some((width as u32, height as u32, pixels))
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.bitmap_destroy)(self.handle) }
        }
    }
}

/// What state a job is in, so that a terminal answer stays terminal.
enum State {
    /// Nothing has been rendered yet.
    Ready,
    /// Pdfium returned "continue"; another `advance` carries on.
    InProgress,
    /// The page is complete.
    Finished,
    /// The render failed, with the sentence to repeat.
    Failed(String),
}

/// A page being rendered in pieces.
///
/// The job owns everything Pdfium needs between calls — the page, the bitmap, and the pause
/// interface — and hands the finished pixels over exactly once. Dropping it abandons a render that
/// has not finished, which is what makes cancellation cheap: Pdfium's render close, bitmap destructor
/// and page close release the partial work, and no further `Continue` call has to be made.
///
/// It holds the page through a [`Page`], which in turn owns the document: a job can outlive the
/// [`Document`] value it was made from, and that is what lets the app store the two in one struct
/// without a self-referential lifetime.
pub struct RenderJob {
    /// The bitmap the page is rendered into.
    ///
    /// Declared before the page on purpose: Rust drops fields in declaration order, and a
    /// progressive render's bitmap has to go before the page it belongs to.
    bitmap: Bitmap,
    /// The page being rendered, and the document that owns it.
    page: Page,
    /// What the pause callback reads. Boxed, because its address is handed to Pdfium.
    control: Box<Control>,
    /// The pause interface itself. Boxed for the same reason.
    pause: Box<Pause>,
    /// The size of the bitmap, which is the size the page was asked to fill.
    size: (c_int, c_int),
    /// The page's rotation, in quarter turns, as Pdfium wants it.
    rotation: c_int,
    /// The render flags: whether the page's own annotations are drawn in.
    flags: c_int,
    /// Whether `FPDF_RenderPageBitmap_Start` has been called.
    started: bool,
    /// Where the render has got to.
    state: State,
}

impl RenderJob {
    /// Prepares a job for one page at a target *bitmap width*.
    ///
    /// `turns` is the *reader's* turn, in quarter turns clockwise: how much the page is turned on top
    /// of the way the document draws it. A page's own `/Rotate` is the document's business and Pdfium
    /// applies it itself — to the size it reports for the page and to what it draws — so the sum is
    /// *not* what goes here. Handing it the sum would draw a `/Rotate 90` page upside down; the test
    /// `the_rotation_handed_to_pdfium_is_the_reader_s_turn` pins that down against a page that carries
    /// one.
    ///
    /// The bitmap is the page as it is *drawn*, so `width` is the width it is drawn at: a page the
    /// reader has turned is the other way round, and the height follows from that shape. The document's
    /// size for the page already has its own `/Rotate` in it, so only the reader's turn is swapped for.
    pub fn new(
        document: &Document,
        index: usize,
        width: u32,
        turns: Quarters,
        format: Format,
        annotations: bool,
    ) -> Result<Self> {
        let api = api()?;

        let (point_width, point_height) = document.page_point_size(index).ok_or_else(|| {
            AppError::Pdf(format!(
                "page {} of {} was not found: it has {} pages, and Pdfium reports error {}",
                index + 1,
                document.name(),
                document.page_count(),
                unsafe { (api.last_error)() }
            ))
        })?;

        // Four turns are the page it started as. Normalising here means a rotation read from a
        // damaged note can never become a value Pdfium's render API does not define.
        let turns = turns % 4;

        // Which way up it is drawn: the shape the document reports for the page — its own `/Rotate` is
        // already in it — or that shape on its side.
        let (shown_width, shown_height) = if crate::pages::swaps_axes(turns) {
            (point_height, point_width)
        } else {
            (point_width, point_height)
        };

        let width = width.max(1) as f32;
        let scale = width / shown_width.max(1.0);

        let pixel_width = width.round().max(1.0);
        let pixel_height = (shown_height * scale).round().max(1.0);

        // A page larger than Pdfium's integer coordinates, or one that would allocate a bitmap no
        // reader could use, is refused before anything is allocated.
        if !pixel_width.is_finite()
            || !pixel_height.is_finite()
            || pixel_width > c_int::MAX as f32
            || pixel_height > c_int::MAX as f32
        {
            return Err(AppError::Pdf(format!(
                "page {} of {} is too large to render at this size ({width} px wide)",
                index + 1,
                document.name()
            )));
        }

        let pixel_width = pixel_width as c_int;
        let pixel_height = pixel_height as c_int;
        let bytes = pixel_width as u64 * pixel_height as u64 * 4;

        if bytes > MAX_BITMAP_BYTES {
            return Err(AppError::Pdf(format!(
                "a {pixel_width}×{pixel_height} page is too large to render ({} MB of pixels)",
                bytes / (1024 * 1024)
            )));
        }

        let page = Page::load(document, index).ok_or_else(|| {
            AppError::Pdf(format!(
                "page {} of {} could not be loaded (Pdfium error {})",
                index + 1,
                document.name(),
                unsafe { (api.last_error)() }
            ))
        })?;

        // One bitmap format or the other, asked for directly: Pdfium renders the grayscale
        // conversion itself, which is better and cheaper than doing it to a colour bitmap after.
        let bitmap = Bitmap::new(pixel_width, pixel_height, format).ok_or_else(|| {
            AppError::Pdf(format!(
                "a {pixel_width}×{pixel_height} bitmap could not be allocated for page {}",
                index + 1
            ))
        })?;

        // Paper, and it has to be done here rather than left to Pdfium: the progressive renderer
        // draws the page *over* whatever is in the bitmap and never clears it, so a page with
        // nothing on it would be a transparent hole in the window. This is a write across the whole
        // buffer — the one part of a render that no budget can interrupt — which is why it belongs
        // to the job's setup: the app starts a job in one wake and slices it in later ones.
        if !bitmap.fill() {
            return Err(AppError::Pdf(format!(
                "a {pixel_width}×{pixel_height} bitmap could not be cleared for page {}",
                index + 1
            )));
        }

        let control = Box::new(Control {
            deadline: Instant::now(),
        });

        let pause = Box::new(Pause {
            version: 1,
            need_to_pause_now: Some(need_to_pause_now),
            user: &*control as *const Control as *mut _,
        });

        Ok(RenderJob {
            bitmap,
            page,
            control,
            pause,
            size: (pixel_width, pixel_height),
            rotation: turns as c_int,
            flags: if annotations { RENDER_ANNOTATIONS } else { 0 },
            started: false,
            state: State::Ready,
        })
    }

    /// Renders for up to `budget`, then stops.
    ///
    /// A budget of zero renders nothing and answers [`Progress::Unfinished`]: the pause callback is
    /// asked before the first piece of work, so "no time" and "stop now" are the same answer.
    ///
    /// A finished job answers [`Progress::Finished`] for as long as it is asked, and a failed one
    /// repeats its failure: `advance` never restarts work whose answer is already in hand.
    pub fn advance(&mut self, budget: Duration) -> Progress {
        match &self.state {
            State::Finished => return Progress::Finished,
            State::Failed(message) => return Progress::Failed(message.clone()),
            State::Ready | State::InProgress => {}
        }

        let Ok(api) = api() else {
            let message = String::from("the Pdfium library is not available");
            self.state = State::Failed(message.clone());
            return Progress::Failed(message);
        };

        let now = Instant::now();
        self.control.deadline = now
            .checked_add(budget)
            .unwrap_or_else(|| now + Duration::from_secs(60));

        let status = unsafe {
            if self.started {
                (api.render_continue)(self.page.handle(), &mut *self.pause)
            } else {
                self.started = true;
                (api.render_start)(
                    self.bitmap.handle,
                    self.page.handle(),
                    0,
                    0,
                    self.size.0,
                    self.size.1,
                    self.rotation,
                    self.flags,
                    &mut *self.pause,
                )
            }
        };

        match status {
            RENDER_DONE => {
                self.state = State::Finished;
                Progress::Finished
            }
            RENDER_TO_BE_CONTINUED => {
                self.state = State::InProgress;
                Progress::Unfinished
            }
            RENDER_FAILED => {
                let message = format!("Pdfium could not render the page (error {})", unsafe {
                    (api.last_error)()
                });
                self.state = State::Failed(message.clone());
                Progress::Failed(message)
            }
            other => {
                let message = format!("Pdfium returned {other} for a progressive render");
                self.state = State::Failed(message.clone());
                Progress::Failed(message)
            }
        }
    }

    /// Copies the rendered pixels out: width, height, and rows of BGRA.
    ///
    /// `None` unless the page finished: a bitmap that is still being drawn, or one whose render
    /// failed, is not a page the caller may show.
    pub fn pixels(&self) -> Option<(u32, u32, Vec<u8>)> {
        if !matches!(self.state, State::Finished) {
            return None;
        }

        self.bitmap.pixels()
    }
}

impl Drop for RenderJob {
    fn drop(&mut self) {
        let Ok(api) = api() else {
            return;
        };

        // Pdfium holds resources for a progressive render until this call, whether the page
        // finished or is being abandoned. It goes before the bitmap and page are destroyed; those
        // are fields, and Rust drops them after this method returns.
        if self.started {
            unsafe { (api.render_page_close)(self.page.handle()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF with a real cross-reference table.
    ///
    /// The offsets are the bytes they claim to be: a file Pdfium had to repair would make the
    /// lifetime test pass for a reason that has nothing to do with lifetimes.
    fn a_one_page_pdf() -> Vec<u8> {
        let objects = [
            String::from("<< /Type /Catalog /Pages 2 0 R >>"),
            String::from("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            String::from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] >>"),
        ];

        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();

        for (index, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
        }

        let xref = out.len();
        out.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in offsets {
            out.push_str(&format!("{offset:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));

        out.into_bytes()
    }

    /// Renders a job to completion, or panics with Pdfium's own sentence.
    fn render(job: &mut RenderJob) -> (u32, u32, Vec<u8>) {
        let give_up_at = Instant::now() + Duration::from_secs(30);

        loop {
            match job.advance(Duration::from_millis(100)) {
                Progress::Finished => break,
                Progress::Unfinished => assert!(
                    Instant::now() < give_up_at,
                    "the fixture did not finish in thirty seconds"
                ),
                Progress::Failed(message) => panic!("{message}"),
            }
        }

        job.pixels().expect("a finished page has pixels")
    }

    /// A render job keeps its document open after the caller drops it.
    ///
    /// This is the invariant the app relies on by holding a job and its document in the same
    /// struct: the job owns a `Page`, the page owns a clone of the document, and so the Pdfium
    /// document handle is alive for as long as the page it loaded. Without the clone, dropping the
    /// document here would leave the job rendering through a closed handle.
    #[test]
    fn a_job_keeps_its_document_open() {
        let _pdfium = crate::pdfium::exclusive();
        if !crate::pdfium::available() {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        }

        let document = Document::open(String::from("one-page.pdf"), a_one_page_pdf())
            .expect("the fixture opens");

        let mut job =
            RenderJob::new(&document, 0, 100, 0, Format::Bgra, false).expect("the page loads");

        // The caller's handle goes away before a single slice is rendered.
        drop(document);

        let (width, height, pixels) = render(&mut job);
        assert_eq!(
            (width, height),
            (100, 200),
            "the page is its own shape at width 100"
        );
        assert_eq!(pixels.len(), 100 * 200 * 4);
        assert_eq!(
            &pixels[..4],
            &[0xff, 0xff, 0xff, 0xff],
            "a blank page is opaque white BGRA"
        );
    }

    /// A slice of zero time renders nothing, and a page that has not finished has no pixels to
    /// hand out. The app relies on both: its pump may wake with a budget already spent, and the
    /// frame path must keep showing the rung it has rather than a half-drawn bitmap.
    #[test]
    fn a_slice_with_no_budget_leaves_the_page_without_pixels() {
        let _pdfium = crate::pdfium::exclusive();
        if !crate::pdfium::available() {
            eprintln!("skipping: pdfium.dll is not available");
            return;
        }

        let document = Document::open(String::from("one-page.pdf"), a_one_page_pdf())
            .expect("the fixture opens");
        let mut job =
            RenderJob::new(&document, 0, 100, 0, Format::Bgra, false).expect("the page loads");

        assert!(job.pixels().is_none(), "nothing has been rendered yet");
        assert_eq!(
            job.advance(Duration::ZERO),
            Progress::Unfinished,
            "no budget means stop at the first opportunity"
        );
        assert!(
            job.pixels().is_none(),
            "a partial bitmap is not a page a frame may draw"
        );
    }
}
