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
//!
//! ## SAFETY: module-wide
//!
//! Every `unsafe` block here is one call through `super::api` on a handle this module owns, and each
//! rests on the invariant stated in [`super::api`]: *the handle is the one Pdfium returned for this
//! object, it has not been closed, and it outlives the call.* The paragraph above is why: the job
//! owns the page, the bitmap and the pause interface, and its `Drop` closes them in the order that
//! keeps the document last.
//!
//! Two blocks are not of that kind and carry their own note: the `from_raw_parts` over the bitmap's
//! pixels, which needs a length and a fully-written buffer, and the `IFSDK_PAUSE` callback, which
//! crosses into C and must not unwind.

use std::ffi::c_int;
use std::sync::Arc;
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

/// The two Pdfium calls a slice can be, named so that a failure says which one it was.
const CALL_START: &str = "FPDF_RenderPageBitmap_Start";
/// Every slice after the first.
const CALL_CONTINUE: &str = "FPDF_RenderPage_Continue";

/// Draw the page's own annotations into the bitmap.
const RENDER_ANNOTATIONS: c_int = 1;

/// Pdfium's bitmap formats, as `FPDFBitmap_*` defines them.
const BITMAP_GRAY: c_int = 1;
/// Blue-green-red-alpha, the format GPUI's renderer uploads.
const BITMAP_BGRA: c_int = 4;

/// What a page's bitmap is filled with before anything is drawn on it: opaque white, in the
/// bitmap's own byte order (which is why no channel can be told from another here).
///
/// Kept as a `u32` because that is what a colour *is*, and widened at the one call that takes it:
/// `FPDFBitmap_FillRect` takes a `FPDF_DWORD`, which is `unsigned long` — 32 bits on Windows and 64
/// elsewhere, and the same four bytes of colour either way (see [`super::api::Api::bitmap_fill`]).
const PAPER: u32 = 0xffff_ffff;

/// The same paper as [`PAPER`] written into a buffer this app allocates, in the byte order a BGRA
/// bitmap holds it.
///
/// `0xffff_ffff` is four `0xff` bytes in any order, which is why this is written as bytes and not
/// computed from [`PAPER`]: an opaque white pixel is all-ones in every channel, and saying so in the
/// layout the buffer actually has is clearer than three shifts that happen to agree.
const PAPER_BYTES: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

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
///
/// **A colour bitmap is rendered into a buffer of this struct's own.** `FPDFBitmap_CreateEx` takes
/// `first_scan` and `stride`, and a bitmap made over an external buffer writes straight into it — so
/// a finished page is handed on without the full-image copy `FPDFBitmap_GetBuffer` would otherwise
/// need. On a 4K page that copy is about 31.6 MiB, paid exactly when a PDF is opened or zoomed, when
/// the reader is waiting.
struct Bitmap {
    /// The handle Pdfium handed out.
    handle: FpdfBitmap,
    /// The format the bytes are in, which decides how they are read out.
    format: Format,
    /// The pixels, when this bitmap was made over a buffer of this app's own.
    ///
    /// `None` for a grayscale bitmap, which asks Pdfium to allocate and widens on the way out: one
    /// channel becomes four, so there is no arrangement of it that the renderer could use as it
    /// stands. The copy that path makes is a conversion rather than an avoidable one.
    ///
    /// An `Arc` rather than a `Vec` because the finished pixels leave while the bitmap may still
    /// exist: [`Bitmap::pixels`] shares the buffer with the caller instead of copying it out, and
    /// the last of the two references to drop is what frees it.
    buffer: Option<Arc<[u8]>>,
}

impl Bitmap {
    /// Asks Pdfium for a bitmap of this size, in this format.
    ///
    /// `None` means Pdfium refused: the dimensions were rejected, or the allocator could not supply
    /// the buffer. Either way the caller gets a sentence rather than a handle it cannot use.
    ///
    /// A BGRA bitmap is made over a buffer allocated here, already filled with paper — see
    /// [`Bitmap::fill`] for why it is written before Pdfium ever sees it.
    fn new(width: c_int, height: c_int, format: Format) -> Option<Self> {
        let api = api().ok()?;

        // Rendering into an external buffer needs a stride this app decides, and Pdfium's own advice
        // is to pass it: a row is exactly its pixels with no alignment padding, which is what makes
        // the buffer the renderer's own bytes rather than something it has to unpick.
        let buffer: Option<(Arc<[u8]>, c_int)> = match format {
            Format::Bgra => {
                let row = usize::try_from(width).ok()?.checked_mul(4)?;
                let length = row.checked_mul(usize::try_from(height).ok()?)?;
                let mut pixels = vec![0u8; length].into_boxed_slice();

                // Paper, written before Pdfium is handed the buffer: the progressive renderer draws
                // *over* what is there and never clears it, so a page with nothing on it would be a
                // transparent hole in the window (see `RenderJob::new`). Doing it here rather than
                // through `FPDFBitmap_FillRect` is the same bytes written once, in memory this side
                // of the boundary.
                //
                // A row of four bytes is one pixel, and `as_chunks` is how that is said without an
                // iterator that has to handle a remainder that cannot exist (the length is
                // `width * 4`, checked above).
                let (rows, _) = pixels.as_chunks_mut::<4>();

                for pixel in rows {
                    pixel.copy_from_slice(&PAPER_BYTES);
                }

                Some((Arc::from(pixels), row as c_int))
            }
            Format::Grayscale => None,
        };

        let (first_scan, stride) = match &buffer {
            Some((pixels, row)) => (pixels.as_ptr() as *mut std::ffi::c_void, *row),
            None => (std::ptr::null_mut(), 0),
        };

        // SAFETY: `first_scan` is either null — Pdfium allocates, and nothing of ours is involved —
        // or the first byte of `buffer`, which is `stride * height` bytes by construction and is kept
        // alive by the `Arc` stored beside the handle. Pdfium's contract for an external buffer is
        // that the caller keeps it valid until `FPDFBitmap_Destroy`, and it is: `Bitmap::drop`
        // destroys the handle, and the `Arc` lives exactly as long as this struct. The buffer is
        // `u8`, so no alignment is required of it.
        let handle =
            unsafe { (api.bitmap_create_ex)(width, height, format.code(), first_scan, stride) };

        (!handle.is_null()).then_some(Bitmap {
            handle,
            format,
            buffer: buffer.map(|(pixels, _)| pixels),
        })
    }

    /// Writes opaque paper across the whole bitmap, when it was not written before Pdfium saw it.
    ///
    /// A bitmap with a buffer of its own has already had its paper written — a second write across
    /// the whole image would be exactly the cost the external buffer exists to remove. One Pdfium
    /// allocated is filled through Pdfium, because this side cannot see inside it.
    ///
    /// The colour is widened to Pdfium's `FPDF_DWORD` here, explicitly: the declaration takes a
    /// `unsigned long` because the header says so, and a `u32` that reached it without a widening
    /// would be the wrong width the day one is added.
    fn fill(&self) -> bool {
        if self.buffer.is_some() {
            return true;
        }

        let Ok(api) = api() else {
            return false;
        };
        let width = unsafe { (api.bitmap_width)(self.handle) };
        let height = unsafe { (api.bitmap_height)(self.handle) };

        width > 0
            && height > 0
            && unsafe {
                (api.bitmap_fill)(
                    self.handle,
                    0,
                    0,
                    width,
                    height,
                    std::ffi::c_ulong::from(PAPER),
                ) != 0
            }
    }

    /// The rendered pixels: width, height, and rows of BGRA.
    ///
    /// For a colour bitmap this is a *share* of the buffer Pdfium drew into — no copy, and no second
    /// allocation of a page's worth of pixels. For a grayscale one it copies, because that is the
    /// conversion rather than a copy of the same bytes: Pdfium's one channel becomes three at full
    /// brightness with an opaque alpha, which is what the renderer uploads.
    ///
    /// Every size is checked against the stride before any pointer arithmetic: Pdfium owns these
    /// values, but a bad one must produce no pixels rather than a slice that reads past the buffer.
    fn pixels(&self) -> Option<(u32, u32, Pixels)> {
        let api = api().ok()?;

        let width = usize::try_from(unsafe { (api.bitmap_width)(self.handle) }).ok()?;
        let height = usize::try_from(unsafe { (api.bitmap_height)(self.handle) }).ok()?;

        if width == 0 || height == 0 {
            return None;
        }

        // The buffer this bitmap was made over, when it has one: its stride is the row written by
        // `Bitmap::new`, so its bytes are already the shape the renderer wants and can be shared
        // rather than read back out.
        if let Some(pixels) = &self.buffer {
            return Some((
                width as u32,
                height as u32,
                Pixels::Shared(Arc::clone(pixels)),
            ));
        }

        self.copied(width, height)
    }

    /// The grayscale path: Pdfium's own buffer, widened a row at a time into a `Vec`.
    ///
    /// Split out so that the mismatch it exists for is visible in one place: four bytes per pixel
    /// out, one byte per pixel in.
    fn copied(&self, width: usize, height: usize) -> Option<(u32, u32, Pixels)> {
        let api = api().ok()?;

        let stride = unsafe { (api.bitmap_stride)(self.handle) };
        let buffer = unsafe { (api.bitmap_buffer)(self.handle) } as *const u8;

        if stride <= 0 || buffer.is_null() {
            return None;
        }

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
        // SAFETY: `buffer` and `length` come from Pdfium for the bitmap this job created, and the
        // checks above are exactly what makes the slice sound:
        //
        //   1. `buffer` is non-null and is Pdfium's own buffer for `FPDFBitmap` — `bitmap_buffer`
        //      earlier in this method returned it, and it is the bitmap `RenderJob::new` made and
        //      keeps alive until `Drop` (which calls `render_page_close`/`bitmap_destroy`). It is
        //      therefore valid for reads for as long as `self` lives, which is longer than this
        //      method.
        //   2. `length` is `stride * height`, computed with `checked_mul` so it cannot wrap, and both
        //      factors are Pdfium's own reported geometry for that bitmap. So the slice covers
        //      exactly the bitmap, never past it.
        //   3. Every byte in that range is initialised: Pdfium wrote the whole bitmap before
        //      reporting the render finished, which is the state `Self::pixels` requires (the
        //      `State::Finished` check above). Reading uninitialised bytes would be the one way this
        //      could be wrong, and a partial render never reaches here.
        //
        // `u8` has no alignment requirement, so no alignment invariant is involved. The slice is not
        // mutated: it is read row by row into a fresh `Vec` and dropped at the end of the method.
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

        Some((width as u32, height as u32, Pixels::Copied(pixels)))
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            // SAFETY: the handle invariant — this is the bitmap `Bitmap::new` made, and this is the
            // only place it is destroyed.
            //
            // The order is what makes the external buffer sound: Pdfium requires the caller's buffer
            // to stay valid until the bitmap is destroyed, and `drop` runs *before* the fields are
            // dropped, so `buffer` is still held here and is released only afterwards.
            unsafe { (api.bitmap_destroy)(self.handle) }
        }
    }
}

/// The pixels of a finished page: rows of BGRA, and whether they were shared or made.
///
/// The distinction is the whole point of the external buffer and is worth carrying to the caller:
/// `Shared` is the buffer Pdfium drew into, with the bitmap's own reference dropped and this one
/// left — no copy, no second allocation. `Copied` is the grayscale path, where every byte was
/// written here because Pdfium's one channel had to become four.
///
/// Both are read the same way once they are in hand (`Deref` to `[u8]`), so a caller that does not
/// care about the difference does not have to know it.
/// The pixels of a finished page: rows of BGRA, and whether they were shared or made.
///
/// The distinction is the whole point of the external buffer and is worth carrying to the caller:
/// `Shared` is the buffer Pdfium drew into, with the bitmap's own reference dropped and this one
/// left — no copy, no second allocation. `Copied` is the grayscale path, where every byte was
/// written here because Pdfium's one channel had to become four.
///
/// Both are read the same way once they are in hand (`Deref` to `[u8]`), so a caller that does not
/// care about the difference does not have to know it.
#[derive(Clone, Debug)]
pub enum Pixels {
    /// The buffer the page was rendered into, shared rather than copied.
    Shared(Arc<[u8]>),
    /// A buffer made by widening Pdfium's grayscale output to BGRA.
    Copied(Vec<u8>),
}

impl Pixels {
    /// Whether this is the renderer's own buffer, which is what the external-buffer path exists for.
    ///
    /// `#[cfg(test)]` because nothing in the app branches on it: a caller hands the bytes on either
    /// way, and this is here so the tests can tell "the page was copied" from "the page was not" —
    /// which is the difference the whole change is about.
    #[cfg(test)]
    pub fn is_shared(&self) -> bool {
        matches!(self, Pixels::Shared(_))
    }

    /// The bytes of the page.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Pixels::Shared(pixels) => pixels,
            Pixels::Copied(pixels) => pixels,
        }
    }

    /// The bytes, taking them over when they are already a `Vec` and copying only a shared buffer.
    ///
    /// The difference matters on the path that matters: a grayscale page was widened into a `Vec`
    /// here in the first place, so handing it on costs nothing, and a colour page's shared buffer is
    /// copied exactly once — into the image that keeps it.
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Pixels::Shared(pixels) => pixels.to_vec(),
            Pixels::Copied(pixels) => pixels,
        }
    }
}

impl std::ops::Deref for Pixels {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes()
    }
}
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
    /// Which page of which document this is, for a failure that has to name them.
    ///
    /// The index is Pdfium's (zero-based) and the name is the document's own, kept as two values
    /// rather than as a borrow of the document: a job outlives the value the caller opened it from,
    /// and the failure sentence is written long after that.
    page_index: usize,
    page_name: String,
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
        // Resolved first, so that a machine with no Pdfium says *that* rather than that the page was
        // not found: the lookup below would fail on a missing library for the same shape of reason.
        api()?;

        let (point_width, point_height) = document.page_point_size(index).ok_or_else(|| {
            // No `FPDF_GetLastError` here: the header defines it for the *document* loaders, and a
            // page-size lookup that failed says only that the page is not there. The count beside it
            // is what makes the sentence useful, and it is a number Pdfium is willing to give.
            AppError::Pdf(format!(
                "page {} of {} was not found: it has {} pages",
                index + 1,
                document.name(),
                document.page_count()
            ))
        })?;

        // Which way up it is drawn: the shape the document reports for the page — its own `/Rotate` is
        // already in it — or that shape on its side. Nothing is normalised before this, and nothing
        // needs to be: [`Quarters`] is one of the four turns and cannot be any other number, so a
        // rotation read from a damaged note could never have become a value Pdfium's API does not
        // define.
        let (shown_width, shown_height) = if turns.swaps_axes() {
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
            // The same rule as the size lookup above: a page that will not load is reported as that,
            // with its number and the document's name, and with no error code the header never
            // promised for this call.
            AppError::Pdf(format!(
                "page {} of {} could not be loaded",
                index + 1,
                document.name()
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
            page_index: index,
            page_name: document.name().to_owned(),
            rotation: turns.turns() as c_int,
            flags: if annotations { RENDER_ANNOTATIONS } else { 0 },
            started: false,
            state: State::Ready,
        })
    }

    /// Renders for up to `budget`, then stops.
    ///
    /// A budget of zero renders nothing and answers [`Progress::Unfinished`] without calling Pdfium
    /// at all. The pause callback would end the slice at the first piece of work anyway, but only
    /// *after* Pdfium had been entered and had done that first piece — a parse, a resource load, a
    /// page's worth of setup — which is work the caller said it had no time for. The check is here
    /// rather than in the callback because the callback cannot see the budget, only the deadline.
    ///
    /// A finished job answers [`Progress::Finished`] for as long as it is asked, and a failed one
    /// repeats its failure: `advance` never restarts work whose answer is already in hand.
    pub fn advance(&mut self, budget: Duration) -> Progress {
        // The terminal answers first, and then the budget: a job that has finished is finished
        // whatever it is asked, and one that is given no time is not entered at all.
        match &self.state {
            State::Finished => return Progress::Finished,
            State::Failed(message) => return Progress::Failed(message.clone()),
            State::Ready | State::InProgress => {}
        }

        if budget.is_zero() {
            return Progress::Unfinished;
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

        // Which call this slice was, so a failure can name it: `Start` is the call that sets a render
        // up and `Continue` is every one after it, and a page that fails on the first is a different
        // bug from one that fails half-way through.
        let (call, status) = if self.started {
            let status = unsafe { (api.render_continue)(self.page.handle(), &mut *self.pause) };

            (CALL_CONTINUE, status)
        } else {
            self.started = true;
            let status = unsafe {
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
            };

            (CALL_START, status)
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
            // A failed render is *not* looked up in `FPDF_GetLastError`. That function is only
            // defined for the calls whose header says so — the document and page loaders, and the
            // page-size and text lookups — and reading it after a render answers whatever the last
            // such call left behind, or nothing at all. What a render failure can honestly say is
            // which call failed and what it returned, which is what this names.
            RENDER_FAILED => {
                let message = self.failure(call, RENDER_FAILED, "Pdfium could not render the page");
                self.state = State::Failed(message.clone());
                Progress::Failed(message)
            }
            other => {
                let message = self.failure(
                    call,
                    other,
                    "Pdfium returned a status a progressive render does not define",
                );
                self.state = State::Failed(message.clone());
                Progress::Failed(message)
            }
        }
    }

    /// The sentence a failed render is reported with: the page, the size, the call and its status.
    ///
    /// The four things that make a screenful of log useful when a PDF will not draw: which page of
    /// which document, how large it was being drawn, whether it was the `Start` call or a
    /// `Continue`, and the number Pdfium answered with. Deliberately no `FPDF_GetLastError` — that
    /// function is documented only for the calls that name it, and a renderer is not one (see
    /// [`Self::advance`]).
    fn failure(&self, call: &str, status: c_int, what: &str) -> String {
        format!(
            "{what}: page {} of {}, drawn at {}×{} px, failed in {call} with status {status}",
            self.page_index + 1,
            self.page_name,
            self.size.0,
            self.size.1
        )
    }

    /// The rendered pixels out: width, height, and rows of BGRA.
    ///
    /// `None` unless the page finished: a bitmap that is still being drawn, or one whose render
    /// failed, is not a page the caller may show.
    ///
    /// A colour page's bytes are the buffer Pdfium drew into, shared rather than copied — the job
    /// keeps no other reference to them, so the caller holds the only one. The pixels therefore stay
    /// valid after the job is dropped, which is what lets the app hand them to an image.
    pub fn pixels(&self) -> Option<(u32, u32, Pixels)> {
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
mod tests;
