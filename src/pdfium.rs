//! The Pdfium C API, as far as this app needs it — including the progressive render.
//!
//! ## Why this app declares the API itself
//!
//! `pdfium-render` wraps Pdfium well, and its *synchronous* `render_with_config` would have been
//! enough — except for one thing the app needs: a render that can be **interrupted**. Pdfium
//! exposes that through `FPDF_RenderPageBitmap_Start` / `FPDF_RenderPage_Continue` and an
//! `IFSDK_PAUSE` callback, and the wrapper does not surface any of it. The wrapper also keeps its
//! document and page handles private, so the raw calls cannot be reached from outside it.
//!
//! So this module resolves the handful of functions the app calls from the library directly. There
//! are twenty of them, each with the signature Pdfium documents, and every one of them is checked
//! for existence as it is resolved: a Pdfium build without the progressive entry points fails with
//! a message naming the missing symbol rather than crashing the process.
//!
//! ## The header that ships with the library is the contract
//!
//! `vendor/include/fpdf_progressive.h` — the header that comes with the DLL in `vendor/lib` — is
//! what these declarations follow, and it is worth saying why. `FPDF_GetPageSizeByIndexF` takes an
//! `FS_SIZEF*` on this build, not the two `float*`s a newer Pdfium declares. Declared the newer
//! way, Pdfium writes *both* floats from the first pointer, which lands on the second local or does
//! not, depending on where the compiler put it — the call worked in a debug build and failed in a
//! release one. The header in the repository is the authority; a binding generated for another
//! Pdfium version is not.
//!
//! ## Why documents are always opened from memory
//!
//! `FPDF_LoadDocument` takes a path that Pdfium interprets itself, which is a different (and less
//! forgiving) encoding story than Rust's own path handling. Reading the file here and opening it
//! with `FPDF_LoadMemDocument64` keeps paths entirely in Rust's hands — and it is what a saved note
//! needs anyway, because a note carries the document as bytes. Pdfium does *not* copy that buffer,
//! so the bytes are kept in [`Document`] for as long as it lives.
//!
//! ## What the progressive render buys
//!
//! A full-quality page at the top of the quality ladder costs tens of milliseconds — a measured
//! 12 ms at 2880 px wide and 55 ms at 5760. Synchronous, that is a frame that never arrives; sliced
//! by [`RenderJob::advance`] with a few-millisecond budget, it is the same work spread over a few
//! pump wakes, with the frames in between showing the rung that is already cached. `NeedToPauseNow`
//! is what makes the slices real: Pdfium asks it between the pieces of a page, so a slice ends when
//! its budget does rather than when the page does.
//!
//! Cancellation follows from the slicing, and needs no flag of its own: the app owns the job, and a
//! render only runs *inside* an `advance` call — so a view that has moved on simply never advances
//! again and drops the job, which is where Pdfium releases the page and the unfinished bitmap.
//!
//! ## What a slice can and cannot bound
//!
//! Measured on the one-page fixture the tests build, at the pump's four-millisecond budget: a
//! 2880-pixel-wide page is 19 ms in one blocking call, or 13 ms of slicing after its setup; a
//! 5760-pixel one is 88 ms, or 52 ms in two slices. The setup is deliberately *outside* the slice —
//! allocating the bitmap and writing the paper into it is a pass over the whole buffer, and Pdfium
//! offers no place to pause inside it — so the app starts a job in one pump wake and slices it in
//! the next, which keeps any single wake bounded by whichever of the two is larger. What the budget
//! bounds is the drawing, which is the part that grows with the *content* of a page; a page's own
//! pixels, allocated and cleared once per rung of the quality ladder, are a floor under it.


use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::{AppError, Result};

/// The opaque handles Pdfium hands out.
type FpdfDocument = *mut c_void;
/// An open page.
type FpdfPage = *mut c_void;
/// A device-independent bitmap Pdfium renders into.
type FpdfBitmap = *mut c_void;

/// Pdfium's `IFSDK_PAUSE`: the callback that decides when a render may stop.
///
/// `user` is the hook the app hangs its own control block on, which is why that block is boxed in
/// [`RenderJob`]: the callback receives a pointer to *this* struct, and the address must not move
/// while a render is in flight.
#[repr(C)]
struct Pause {
    /// The interface version. Pdfium currently requires 1.
    version: c_int,
    /// Called between the pieces of a page; non-zero means "stop here".
    need_to_pause_now: Option<unsafe extern "C" fn(*mut Pause) -> c_int>,
    /// The app's own control block, as [`Control`].
    user: *mut c_void,
}

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

/// What Pdfium's progressive calls return.
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


/// A loaded shared library, as an opaque handle.
type Library = *mut c_void;

/// The file name Pdfium is built as on this platform.
#[cfg(windows)]
const LIBRARY_NAME: &str = "pdfium.dll";

/// The functions this app calls, resolved once and kept for the process.
///
/// Typed as the C API declares them rather than as the loader's untyped pointer: the cast in
/// [`resolve`] is the one place the two are made to agree, and every call site after it looks like
/// an ordinary call.
struct Api {
    init_library: unsafe extern "C" fn(),
    load_memory_document: unsafe extern "C" fn(*const c_void, usize, *const c_char) -> FpdfDocument,
    close_document: unsafe extern "C" fn(FpdfDocument),
    page_count: unsafe extern "C" fn(FpdfDocument) -> c_int,
    page_size: unsafe extern "C" fn(FpdfDocument, c_int, *mut SizeF) -> c_int,
    load_page: unsafe extern "C" fn(FpdfDocument, c_int) -> FpdfPage,
    close_page: unsafe extern "C" fn(FpdfPage),
    page_rotation: unsafe extern "C" fn(FpdfPage) -> c_int,
    bitmap_create_ex: unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> FpdfBitmap,
    bitmap_fill: unsafe extern "C" fn(FpdfBitmap, c_int, c_int, c_int, c_int, c_uint) -> c_int,
    bitmap_buffer: unsafe extern "C" fn(FpdfBitmap) -> *mut c_void,
    bitmap_stride: unsafe extern "C" fn(FpdfBitmap) -> c_int,
    bitmap_width: unsafe extern "C" fn(FpdfBitmap) -> c_int,
    bitmap_height: unsafe extern "C" fn(FpdfBitmap) -> c_int,
    bitmap_destroy: unsafe extern "C" fn(FpdfBitmap),
    render_start: unsafe extern "C" fn(
        FpdfBitmap,
        FpdfPage,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        *mut Pause,
    ) -> c_int,
    render_continue: unsafe extern "C" fn(FpdfPage, *mut Pause) -> c_int,
    last_error: unsafe extern "C" fn() -> c_uint,
}

/// The process-wide library, or the message explaining why there is none.
///
/// The failure is cached as well as the success: a missing `pdfium.dll` is a state the app is
/// designed to run in, and looking for it again on every frame would be a per-frame scan of the
/// filesystem to reach the same answer.
static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();

/// The library, loaded and initialised on first use.
fn api() -> Result<&'static Api> {
    match API.get_or_init(|| load_api().map_err(|error| error.to_string())) {
        Ok(api) => Ok(api),
        Err(message) => Err(AppError::PdfiumLibrary(message.clone())),
    }
}

/// Whether Pdfium is available, for the tests that skip work without it.
///
/// Loading the library is the check: it happens once for the process either way, so asking is not a
/// second attempt, and a missing `pdfium.dll` is a supported state the app reports rather than a
/// reason not to start.
#[cfg(test)]
pub fn available() -> bool {
    api().is_ok()
}

/// Finds the library, resolves the functions, and initialises Pdfium.
fn load_api() -> Result<Api> {
    let library = open_library()?;

    unsafe {
        let api = Api {
            init_library: resolve(library, "FPDF_InitLibrary")?,
            load_memory_document: resolve(library, "FPDF_LoadMemDocument64")?,
            close_document: resolve(library, "FPDF_CloseDocument")?,
            page_count: resolve(library, "FPDF_GetPageCount")?,
            page_size: resolve(library, "FPDF_GetPageSizeByIndexF")?,
            load_page: resolve(library, "FPDF_LoadPage")?,
            close_page: resolve(library, "FPDF_ClosePage")?,
            page_rotation: resolve(library, "FPDFPage_GetRotation")?,
            bitmap_create_ex: resolve(library, "FPDFBitmap_CreateEx")?,
            bitmap_fill: resolve(library, "FPDFBitmap_FillRect")?,
            bitmap_buffer: resolve(library, "FPDFBitmap_GetBuffer")?,
            bitmap_stride: resolve(library, "FPDFBitmap_GetStride")?,
            bitmap_width: resolve(library, "FPDFBitmap_GetWidth")?,
            bitmap_height: resolve(library, "FPDFBitmap_GetHeight")?,
            bitmap_destroy: resolve(library, "FPDFBitmap_Destroy")?,
            render_start: resolve(library, "FPDF_RenderPageBitmap_Start")?,
            render_continue: resolve(library, "FPDF_RenderPage_Continue")?,
            last_error: resolve(library, "FPDF_GetLastError")?,
        };

        // Once, for the process: every document lives inside this initialisation, and Pdfium's own
        // documentation is explicit that it is library-wide state rather than a per-document one.
        (api.init_library)();
        Ok(api)
    }
}

/// Loads the shared library, from the places the app ships it or a build puts it.
#[cfg(windows)]
fn open_library() -> Result<Library> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::core::PCWSTR;

    let mut tried: Vec<String> = Vec::new();

    for directory in library_candidates() {
        let candidate = directory.join(LIBRARY_NAME);
        if !candidate.is_file() {
            continue;
        }

        let wide: Vec<u16> = candidate
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
            Ok(module) => return Ok(module.0 as Library),
            Err(error) => tried.push(format!("{}: {error}", candidate.display())),
        }
    }

    Err(AppError::PdfiumLibrary(if tried.is_empty() {
        format!(
            "{LIBRARY_NAME} was not found next to the executable, in the working directory, or in vendor/lib"
        )
    } else {
        tried.join("; ")
    }))
}

/// The platforms this build does not ship a library for: there is nothing to load.
#[cfg(not(windows))]
fn open_library() -> Result<Library> {
    Err(AppError::PdfiumLibrary(format!(
        "Pdfium is loaded from {LIBRARY_NAME}, which is the Windows build's library"
    )))
}

/// Where the app looks for the shared library, nearest first.
fn library_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.to_path_buf());
        }
    }

    if let Ok(working_directory) = std::env::current_dir() {
        // The repository ships the library under `vendor/lib`, which is where a `cargo run` from
        // the project root finds it.
        candidates.push(working_directory.join("vendor").join("lib"));
        candidates.push(working_directory.join("vendor"));
        candidates.push(working_directory);
    }

    candidates
}

/// One exported function, by the name Pdfium exports it under.
///
/// `T` is the function's own type, and the cast from the loader's untyped pointer is the assertion
/// that the declaration above matches the library — which is why every name is spelled out exactly
/// once, in [`load_api`], and checked for existence as it is resolved.
#[cfg(windows)]
unsafe fn resolve<T: Copy>(library: Library, name: &str) -> Result<T> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    use windows::core::PCSTR;

    let spelled = CString::new(name)
        .map_err(|_| AppError::PdfiumLibrary(format!("{name} is not a valid symbol name")))?;

    let address = unsafe { GetProcAddress(HMODULE(library), PCSTR(spelled.as_ptr() as *const u8)) };

    match address {
        Some(address) => Ok(unsafe { std::mem::transmute_copy(&address) }),
        None => Err(AppError::PdfiumLibrary(format!(
            "{name} is missing from this build of {LIBRARY_NAME}"
        ))),
    }
}

/// A symbol that cannot be resolved without a library to resolve it from.
#[cfg(not(windows))]
unsafe fn resolve<T: Copy>(_library: Library, name: &str) -> Result<T> {
    Err(AppError::PdfiumLibrary(format!(
        "{name} cannot be resolved without the Windows library"
    )))
}


/// Pdfium's `FS_SIZEF`: a width and a height, in points.
///
/// The size lookup takes this *struct* rather than two `float*`s on the build this app ships. The
/// vendored header is the contract — a bindgen snapshot for a different pdfium version is not — and
/// getting it wrong is invisible until it is not: with two pointers declared, Pdfium writes both
/// floats from the first one, which lands correctly or not at all depending on where the compiler
/// put the second local.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SizeF {
    /// Width, in points.
    width: f32,
    /// Height, in points.
    height: f32,
}

/// An open PDF document, and the bytes it was opened from.
///
/// Pdfium reads the document from that buffer *in place* — it does not take a copy — so the bytes
/// are held here for exactly as long as the document is open. They are also what saving a note
/// writes out, which is why they are kept rather than dropped after opening.
pub struct Document {
    /// The handle Pdfium handed out.
    handle: FpdfDocument,
    /// The name to show for it.
    name: String,
    /// The bytes the document was opened from, held for as long as the handle lives.
    ///
    /// Underscored because nothing in this crate *reads* it: it is not a cache, it is the storage
    /// Pdfium reads from in place. Dropping it while the document is open would leave Pdfium
    /// reading freed memory, which is why it is a field of the document rather than a local.
    _buffer: Vec<u8>,
}

impl Document {
    /// Opens a document from bytes read elsewhere.
    pub fn open(name: String, bytes: Vec<u8>) -> Result<Self> {
        let api = api()?;

        let handle = unsafe {
            (api.load_memory_document)(bytes.as_ptr().cast(), bytes.len(), std::ptr::null())
        };

        if handle.is_null() {
            return Err(AppError::Pdf(format!(
                "{name} could not be read (Pdfium error {})",
                unsafe { (api.last_error)() }
            )));
        }

        Ok(Document {
            handle,
            name,
            _buffer: bytes,
        })
    }

    /// The name to show for the document.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many pages the document has.
    pub fn page_count(&self) -> usize {
        match api() {
            Ok(api) => unsafe { (api.page_count)(self.handle) }.max(0) as usize,
            Err(_) => 0,
        }
    }

    /// A page's own size in PDF points, without loading it.
    pub fn page_point_size(&self, index: usize) -> Option<(f32, f32)> {
        let api = api().ok()?;

        let mut size = SizeF::default();
        let ok = unsafe { (api.page_size)(self.handle, index as c_int, &mut size) };

        (ok != 0 && size.width > 0.0 && size.height > 0.0).then_some((size.width, size.height))
    }

    /// A page's rotation, in quarter turns clockwise.
    ///
    /// Loading the page to ask is the only way Pdfium offers: the rotation belongs to the page
    /// object, not to the document's page table.
    pub fn page_rotation(&self, index: usize) -> u8 {
        let Ok(api) = api() else {
            return 0;
        };

        let page = unsafe { (api.load_page)(self.handle, index as c_int) };
        if page.is_null() {
            return 0;
        }

        let rotation = unsafe { (api.page_rotation)(page) }.clamp(0, 3) as u8;
        unsafe { (api.close_page)(page) };
        rotation
    }

    /// The handle a render job needs.
    fn handle(&self) -> FpdfDocument {
        self.handle
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.close_document)(self.handle) };
        }
    }
}

/// What a page's bitmap is filled with before anything is drawn on it: opaque white, in the
/// bitmap's own byte order (which is why no channel can be told from another here).
const PAPER: c_uint = 0xffff_ffff;


/// A page being rendered in pieces.
///
/// The job owns everything Pdfium needs between calls — the page handle, the bitmap, and the pause
/// interface — and hands the finished pixels over exactly once. Dropping it abandons a render that
/// has not finished, which is what makes cancellation cheap: Pdfium's bitmap destructor and page
/// close release the partial work, and no further `Continue` call has to be made.
///
/// ## Field order is the drop order
///
/// The page handle belongs to the document, so a job must be dropped *before* the document it came
/// from. The app keeps the job in the same struct as the document and declares the job's field
/// first for exactly this reason — see `PdfDocumentView`.
pub struct RenderJob {
    /// The page being rendered.
    page: FpdfPage,
    /// The bitmap the page is rendered into.
    bitmap: FpdfBitmap,
    /// What the pause callback reads. Boxed, because its address is handed to Pdfium.
    control: Box<Control>,
    /// The pause interface itself. Boxed for the same reason.
    pause: Box<Pause>,
    /// The size of the bitmap, which is the size the page was asked to fill.
    size: (u32, u32),
    /// The page's rotation, in quarter turns, as Pdfium wants it.
    rotation: c_int,
    /// The render flags: whether the page's own annotations are drawn in.
    flags: c_int,
    /// The format the bitmap is in, which decides how its pixels are read out.
    format: Format,
    /// Whether `render_start` has been called.
    started: bool,
    /// Whether the job has finished, failed, or been abandoned.
    settled: bool,
}

impl RenderJob {
    /// Prepares a job for one page at a target *bitmap width*.
    ///
    /// The height follows from the page's own aspect ratio, and the two are swapped for the quarter
    /// turns: a rotated page needs its bitmap the other way round to fill it.
    pub fn new(
        document: &Document,
        index: usize,
        width: u32,
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

        let rotation = document.page_rotation(index);
        let width = width.max(1) as f32;
        let scale = width / point_width.max(1.0);
        let (mut pixel_width, mut pixel_height) = (width, point_height * scale);

        if rotation == 1 || rotation == 3 {
            std::mem::swap(&mut pixel_width, &mut pixel_height);
        }

        let (pixel_width, pixel_height) = (
            pixel_width.round().max(1.0) as c_int,
            pixel_height.round().max(1.0) as c_int,
        );

        let page = unsafe { (api.load_page)(document.handle(), index as c_int) };
        if page.is_null() {
            return Err(AppError::Pdf(format!(
                "page {} of {} could not be loaded (Pdfium error {})",
                index + 1,
                document.name(),
                unsafe { (api.last_error)() }
            )));
        }

        // One bitmap format or the other, asked for directly: Pdfium renders the grayscale
        // conversion itself, which is better and cheaper than doing it to a colour bitmap after.
        let code = match format {
            Format::Bgra => BITMAP_BGRA,
            Format::Grayscale => BITMAP_GRAY,
        };

        let bitmap = unsafe {
            (api.bitmap_create_ex)(pixel_width, pixel_height, code, std::ptr::null_mut(), 0)
        };
        if bitmap.is_null() {
            unsafe { (api.close_page)(page) };
            return Err(AppError::Pdf(format!(
                "a {pixel_width}×{pixel_height} bitmap could not be allocated for page {}",
                index + 1
            )));
        }

        // Paper, and it has to be done here rather than left to Pdfium: the progressive renderer
        // draws the page *over* whatever is in the bitmap and never clears it, so a page with
        // nothing on it would be a transparent hole in the window. This is a write across the whole
        // buffer — the one part of a render that no budget can interrupt — which is why it belongs
        // to the job's setup: the app starts a job in one wake and slices it in later ones.
        unsafe { (api.bitmap_fill)(bitmap, 0, 0, pixel_width, pixel_height, PAPER) };

        let control = Box::new(Control {
            deadline: Instant::now(),
        });

        let pause = Box::new(Pause {
            version: 1,
            need_to_pause_now: Some(need_to_pause_now),
            user: &*control as *const Control as *mut c_void,
        });

        Ok(RenderJob {
            page,
            bitmap,
            control,
            pause,
            size: (pixel_width as u32, pixel_height as u32),
            rotation: rotation as c_int,
            flags: if annotations { RENDER_ANNOTATIONS } else { 0 },
            format,
            started: false,
            settled: false,
        })
    }

    /// Renders for up to `budget`, then stops.
    ///
    /// A budget of zero renders nothing and answers [`Progress::Unfinished`]: the pause callback is
    /// asked before the first piece of work, so "no time" and "stop now" are the same answer.
    pub fn advance(&mut self, budget: Duration) -> Progress {
        if self.settled {
            return Progress::Unfinished;
        }

        let Ok(api) = api() else {
            self.settled = true;
            return Progress::Failed(String::from("the Pdfium library is not available"));
        };

        self.control.deadline = Instant::now() + budget;

        let status = unsafe {
            if self.started {
                (api.render_continue)(self.page, &mut *self.pause)
            } else {
                self.started = true;
                (api.render_start)(
                    self.bitmap,
                    self.page,
                    0,
                    0,
                    self.size.0 as c_int,
                    self.size.1 as c_int,
                    self.rotation,
                    self.flags,
                    &mut *self.pause,
                )
            }
        };

        match status {
            RENDER_DONE => {
                self.settled = true;
                Progress::Finished
            }
            RENDER_TO_BE_CONTINUED => Progress::Unfinished,
            RENDER_FAILED => {
                self.settled = true;
                Progress::Failed(format!(
                    "Pdfium could not render the page (error {})",
                    unsafe { (api.last_error)() }
                ))
            }
            other => {
                self.settled = true;
                Progress::Failed(format!("Pdfium returned {other} for a progressive render"))
            }
        }
    }

    /// Copies the rendered pixels out: width, height, and rows of BGRA.
    ///
    /// Copied rather than borrowed because the bitmap belongs to Pdfium, and the app hands the
    /// pixels to GPUI, which keeps them for as long as the image is on screen. A grayscale page is
    /// widened to BGRA here, because that is what the renderer uploads: its one channel becomes all
    /// three, at full brightness, with an opaque alpha.
    pub fn pixels(&self) -> Option<(u32, u32, Vec<u8>)> {
        let api = api().ok()?;

        let width = unsafe { (api.bitmap_width)(self.bitmap) };
        let height = unsafe { (api.bitmap_height)(self.bitmap) };
        let stride = unsafe { (api.bitmap_stride)(self.bitmap) };
        let buffer = unsafe { (api.bitmap_buffer)(self.bitmap) } as *const u8;

        if width <= 0 || height <= 0 || stride <= 0 || buffer.is_null() {
            return None;
        }

        let (width, height, stride) = (width as usize, height as usize, stride as usize);
        // Row by row, because the stride may be wider than a row: Pdfium aligns scan lines, and
        // handing GPUI the padding as pixels would shear the page.
        let source_row = match self.format {
            Format::Bgra => width * 4,
            Format::Grayscale => width,
        };

        let target_row = width * 4;
        let mut pixels = Vec::with_capacity(target_row * height);

        for line in 0..height {
            let start = unsafe { buffer.add(line * stride) };
            let row = unsafe { std::slice::from_raw_parts(start, source_row) };

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

impl Drop for RenderJob {
    fn drop(&mut self) {
        let Ok(api) = api() else {
            return;
        };

        unsafe {
            // The bitmap first: destroying it is what ends a render that has not finished.
            (api.bitmap_destroy)(self.bitmap);
            (api.close_page)(self.page);
        }
    }
}
