//! The raw Pdfium boundary: its opaque handles, its C types, and the symbols this app resolves.
//!
//! ## The declarations follow the vendored header, not a binding
//!
//! `vendor/include` is the header set that ships with the DLL in `vendor/lib`, and it is the
//! contract these declarations follow. That is not a formality: `FPDF_GetPageSizeByIndexF` takes
//! an `FS_SIZEF*`, while the older `FPDF_GetPageSizeByIndex` beside it takes two `double*`s.
//! Declared against the wrong one, Pdfium writes both values through the first pointer — which
//! lands on the second local or does not, depending on where the compiler put it. The header in
//! the repository is the authority; a binding generated for another Pdfium version is not.
//!
//! ## Only what the app uses is named
//!
//! Pdfium's public API is large. This app calls thirty-two entry points, and this module declares
//! exactly those: documents, pages, bitmaps, the progressive render, the outline walk, and page
//! text. Every one is resolved by name when the library is loaded, and a symbol missing from the
//! build is reported with its own name rather than becoming a null call at the worst moment.
//!
//! ## One initialisation, with a versioned configuration
//!
//! `FPDF_InitLibraryWithConfig` takes a `FPDF_LIBRARY_CONFIG` whose layout depends on the
//! `version` field. This module passes a version-2 prefix and nothing experimental: the pointer
//! to the library's own configuration ends there, so the fields that newer releases appended
//! (renderer, font backend, Brotli, per-document isolates) are never read. `FPDF_DestroyLibrary`
//! is deliberately not called: the library lives for the process, documents may still be alive,
//! and Pdfium's own documentation requires every one of them to be closed first.
//!
//! ## Where the library comes from, and where it is looked for
//!
//! The repository does not carry Pdfium: it is downloaded into `vendor/` (see `vendor/README.md`),
//! and `/vendor` is ignored by git. A build copies `vendor/lib/*.dll` next to the executable it
//! produces (`build.rs`), and that is the first place this module looks — so a release build is a
//! *pair* of files that can be started from anywhere, or carried to another machine, and PDFs
//! open.
//!
//! The rest of the search is for the arrangements a developer has instead: a `vendor/lib` under
//! the working directory (a `cargo run` from the project root), or beside the executable's own
//! directory and the few above it (an executable started from `target/release`, which is how a
//! release build is normally started). A fresh clone has none of this, and that is a state the app
//! is built for: it starts, draws, writes ink, and refuses to open a PDF with a message naming
//! where the library belongs rather than crashing.

use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::error::{AppError, Result};

/// An opaque handle Pdfium hands out.
pub(super) type FpdfDocument = *mut c_void;
/// An open page.
pub(super) type FpdfPage = *mut c_void;
/// A device-independent bitmap Pdfium renders into.
pub(super) type FpdfBitmap = *mut c_void;
/// One entry of a document's outline. Pdfium calls these bookmarks; the PDF specification calls
/// the whole tree the *outline*, and this module follows the specification because the app has its
/// own meaning for the word "bookmark" (see [`crate::bookmarks`]).
pub(super) type FpdfBookmark = *mut c_void;
/// Where an entry points, before it has been turned into a page index.
pub(super) type FpdfDest = *mut c_void;
/// What an entry does instead of pointing somewhere in this document.
pub(super) type FpdfAction = *mut c_void;
/// A page's text: the character stream Pdfium builds for a page that has been asked for its text.
pub(super) type FpdfTextPage = *mut c_void;

/// Pdfium's `FS_SIZEF`: a width and a height, in points.
///
/// The size lookup takes this *struct* rather than two `double*`s on the build this app ships.
/// The vendored header is the contract — a bindgen snapshot for a different Pdfium version is
/// not — and getting it wrong is invisible until it is not.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SizeF {
    /// Width, in points.
    pub(super) width: f32,
    /// Height, in points.
    pub(super) height: f32,
}

/// Pdfium's `IFSDK_PAUSE`: the callback that decides when a render may stop.
///
/// `user` is the hook the render job hangs its own control block on, which is why that block is
/// boxed: the callback receives a pointer to *this* struct, and the address must not move while a
/// render is in flight.
#[repr(C)]
pub(super) struct Pause {
    /// The interface version. Pdfium currently requires 1.
    pub(super) version: c_int,
    /// Called between the pieces of a page; non-zero means "stop here".
    pub(super) need_to_pause_now: Option<unsafe extern "C" fn(*mut Pause) -> c_int>,
    /// The app's own control block, as `render::Control`.
    pub(super) user: *mut c_void,
}

/// Pdfium's `FPDF_LIBRARY_CONFIG`, as far as version 2 defines it.
///
/// Deliberately a prefix rather than the whole struct: the version field tells Pdfium how much of
/// the structure it may read, and versions 3 and up added experimental fields whose layout this
/// app has no opinion about. Keeping the prefix means a new Pdfium release can add fields to the
/// end without changing what is passed here.
#[repr(C)]
pub(super) struct LibraryConfig {
    /// The interface version. `2` is the current stable one; version 1 is being retired.
    version: c_int,
    /// Font directories, or null for Pdfium's defaults.
    user_font_paths: *const *const c_char,
    /// A V8 isolate to reuse, or null to let Pdfium make its own (this build has no V8).
    isolate: *mut c_void,
    /// The embedder data slot in that isolate. Zero is the usual choice.
    v8_embedder_slot: c_uint,
}

/// A loaded shared library, as an opaque handle.
type Library = *mut c_void;

/// The file name Pdfium is built as on this platform.
///
/// Named on every platform even though only Windows loads one through `LoadLibrary`: the stub below
/// reports what it *would* have looked for, and a macOS build's first step is to load this file rather
/// than to guess what it is called.
#[cfg(windows)]
const LIBRARY_NAME: &str = "pdfium.dll";
/// The same library, built for macOS.
#[cfg(target_os = "macos")]
const LIBRARY_NAME: &str = "libpdfium.dylib";
/// The same library, built for everything else.
#[cfg(not(any(windows, target_os = "macos")))]
const LIBRARY_NAME: &str = "libpdfium.so";

/// The Pdfium release these declarations were written against, from `vendor/VERSION`.
///
/// Pdfium's public API has no version function, so a mismatch cannot be detected by calling the
/// library. Naming the intended release in a load failure is the next best thing: it tells a person
/// looking at a missing symbol that the headers and the DLL have to come from the same archive.
/// The Pdfium release this build was made against, or a sentence saying it is unpinned.
///
/// The app's build script prints the version from `vendor/VERSION`; a build of *this* crate on its own
/// has no vendor directory and no library to load either, so the message it produces says the one thing
/// that is true rather than a version it does not know.
const VENDORED_VERSION: &str = match option_env!("CHEAP_NOTE_PDFIUM_VERSION") {
    Some(version) => version,
    None => "an unpinned build",
};

/// The functions this app calls, resolved once and kept for the process.
///
/// Typed as the C API declares them rather than as the loader's untyped pointer: the cast in
/// [`resolve`] is the one place the two are made to agree, and every call site after it looks like
/// an ordinary call.
pub(super) struct Api {
    pub(super) init_library: unsafe extern "C" fn(*const LibraryConfig),
    pub(super) load_memory_document:
        unsafe extern "C" fn(*const c_void, usize, *const c_char) -> FpdfDocument,
    pub(super) close_document: unsafe extern "C" fn(FpdfDocument),
    pub(super) page_count: unsafe extern "C" fn(FpdfDocument) -> c_int,
    pub(super) page_size: unsafe extern "C" fn(FpdfDocument, c_int, *mut SizeF) -> c_int,
    pub(super) load_page: unsafe extern "C" fn(FpdfDocument, c_int) -> FpdfPage,
    pub(super) close_page: unsafe extern "C" fn(FpdfPage),
    pub(super) page_rotation: unsafe extern "C" fn(FpdfPage) -> c_int,
    pub(super) bitmap_create_ex:
        unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> FpdfBitmap,
    pub(super) bitmap_fill:
        unsafe extern "C" fn(FpdfBitmap, c_int, c_int, c_int, c_int, c_uint) -> c_int,
    pub(super) bitmap_buffer: unsafe extern "C" fn(FpdfBitmap) -> *mut c_void,
    pub(super) bitmap_stride: unsafe extern "C" fn(FpdfBitmap) -> c_int,
    pub(super) bitmap_width: unsafe extern "C" fn(FpdfBitmap) -> c_int,
    pub(super) bitmap_height: unsafe extern "C" fn(FpdfBitmap) -> c_int,
    pub(super) bitmap_destroy: unsafe extern "C" fn(FpdfBitmap),
    pub(super) render_start: unsafe extern "C" fn(
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
    pub(super) render_continue: unsafe extern "C" fn(FpdfPage, *mut Pause) -> c_int,
    pub(super) render_page_close: unsafe extern "C" fn(FpdfPage),
    pub(super) last_error: unsafe extern "C" fn() -> c_ulong,
    pub(super) bookmark_first_child:
        unsafe extern "C" fn(FpdfDocument, FpdfBookmark) -> FpdfBookmark,
    pub(super) bookmark_next_sibling:
        unsafe extern "C" fn(FpdfDocument, FpdfBookmark) -> FpdfBookmark,
    pub(super) bookmark_title: unsafe extern "C" fn(FpdfBookmark, *mut c_void, c_ulong) -> c_ulong,
    pub(super) bookmark_dest: unsafe extern "C" fn(FpdfDocument, FpdfBookmark) -> FpdfDest,
    pub(super) bookmark_action: unsafe extern "C" fn(FpdfBookmark) -> FpdfAction,
    pub(super) action_type: unsafe extern "C" fn(FpdfAction) -> c_ulong,
    pub(super) action_dest: unsafe extern "C" fn(FpdfDocument, FpdfAction) -> FpdfDest,
    pub(super) dest_page_index: unsafe extern "C" fn(FpdfDocument, FpdfDest) -> c_int,
    pub(super) text_load_page: unsafe extern "C" fn(FpdfPage) -> FpdfTextPage,
    pub(super) text_close_page: unsafe extern "C" fn(FpdfTextPage),
    pub(super) text_count_chars: unsafe extern "C" fn(FpdfTextPage) -> c_int,
    pub(super) text_unicode: unsafe extern "C" fn(FpdfTextPage, c_int) -> c_uint,
    pub(super) text_char_box:
        unsafe extern "C" fn(FpdfTextPage, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int,
}

/// The process-wide library, or the message explaining why there is none.
///
/// The failure is cached as well as the success: a missing `pdfium.dll` is a state the app is
/// designed to run in, and looking for it again on every frame would be a per-frame scan of the
/// filesystem to reach the same answer.
static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();

/// The library, loaded and initialised on first use.
pub(super) fn api() -> Result<&'static Api> {
    match API.get_or_init(|| load_api().map_err(|error| error.to_string())) {
        Ok(api) => Ok(api),
        Err(message) => Err(AppError::PdfiumLibrary(message.clone())),
    }
}

/// Whether Pdfium is available, for the tests that skip work without it.
///
/// Loading the library is the check: it happens once for the process either way, so asking is not
/// a second attempt, and a missing `pdfium.dll` is a supported state the app reports rather than a
/// reason not to start.
#[cfg(test)]
pub(crate) fn available() -> bool {
    api().is_ok()
}

/// The right to use Pdfium, for a test that rasterises.
///
/// One process, one Pdfium library, and `cargo test` runs tests on parallel threads: Pdfium is
/// not thread-safe, so every test that touches a document takes this, wherever it lives. Without
/// it the failures are intermittent and look like bugs in the renderer.
#[cfg(test)]
pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // A test that panicked while holding the lock must not poison it for the others: the panic is
    // reported by the test that caused it, and the rest still have to run.
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Finds the library, resolves the functions, and initialises Pdfium.
fn load_api() -> Result<Api> {
    let library = open_library()?;

    unsafe {
        let api = Api {
            init_library: resolve(library, "FPDF_InitLibraryWithConfig")?,
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
            // Closing the render is not the same as closing the page: Pdfium keeps resources for
            // a progressive render until this is called, whether it finished or was abandoned.
            render_page_close: resolve(library, "FPDF_RenderPage_Close")?,
            last_error: resolve(library, "FPDF_GetLastError")?,
            // The outline: what the document's own table of contents is made of. Reading it is not
            // rendering, so it needs no page handle — a title, a destination, and the two ways of
            // walking the tree.
            bookmark_first_child: resolve(library, "FPDFBookmark_GetFirstChild")?,
            bookmark_next_sibling: resolve(library, "FPDFBookmark_GetNextSibling")?,
            bookmark_title: resolve(library, "FPDFBookmark_GetTitle")?,
            bookmark_dest: resolve(library, "FPDFBookmark_GetDest")?,
            bookmark_action: resolve(library, "FPDFBookmark_GetAction")?,
            action_type: resolve(library, "FPDFAction_GetType")?,
            action_dest: resolve(library, "FPDFAction_GetDest")?,
            dest_page_index: resolve(library, "FPDFDest_GetDestPageIndex")?,
            // The text of a page: what a highlighter snaps to. Reading it is not rendering — the
            // char boxes are in the page's own coordinates — so this needs no bitmap and no
            // render, which is what lets a page's text be read the moment it is turned to.
            text_load_page: resolve(library, "FPDFText_LoadPage")?,
            text_close_page: resolve(library, "FPDFText_ClosePage")?,
            text_count_chars: resolve(library, "FPDFText_CountChars")?,
            text_unicode: resolve(library, "FPDFText_GetUnicode")?,
            text_char_box: resolve(library, "FPDFText_GetCharBox")?,
        };

        // Once, for the process: every document lives inside this initialisation, and Pdfium's own
        // documentation is explicit that it is library-wide state rather than a per-document one.
        //
        // Version 2 is passed rather than the whole newer structure: the version field is what
        // tells Pdfium which fields exist, and passing every field a future release adds would
        // make this declaration depend on that release's layout.
        let config = LibraryConfig {
            version: 2,
            user_font_paths: std::ptr::null(),
            isolate: std::ptr::null_mut(),
            v8_embedder_slot: 0,
        };
        (api.init_library)(&config);

        // The library handle is not stored: Windows keeps a loaded module in the process until it
        // exits, and `FPDF_DestroyLibrary` is deliberately not called (see the module docs).
        Ok(api)
    }
}

/// Loads the shared library, from the places the app ships it or a build puts it.
#[cfg(windows)]
fn open_library() -> Result<Library> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;

    let candidates = library_candidates();
    let mut tried: Vec<String> = Vec::new();

    for directory in &candidates {
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
        // What was looked *in* rather than only what was found: a missing library is fixed by putting
        // it in one of these places, and this message is what a person has to work from.
        format!(
            "{LIBRARY_NAME} (Pdfium {VENDORED_VERSION}) was not found. It belongs next to the \
             executable ({}) or in a vendor/lib beside it, the working directory, or a project it \
             was built in — see vendor/README.md",
            candidates.first().map_or_else(
                || String::from("an unknown directory"),
                |directory| directory.display().to_string()
            )
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
    let executable = std::env::current_exe().ok();
    let working_directory = std::env::current_dir().ok();

    candidates_for(executable.as_deref(), working_directory.as_deref())
}

/// The places a library is looked for, given the two things that say where this process is.
///
/// Split out from the search so it can be tested: what is worth pinning is not that a running process
/// knows its own path, but that the layout of a build tree leads the search back to the repository's
/// own `vendor/lib` (see the tests at the foot of this module).
///
/// The order is what makes it work. The executable's **own directory** is first, because a build puts
/// the library there and two files that belong together are the whole of what a release build ships;
/// then the few directories **above** it, which is where a build tree keeps the repository's copy; and
/// then the **working directory**, which is what a `cargo run` from the project root has.
fn candidates_for(executable: Option<&Path>, working_directory: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(directory) = executable.and_then(Path::parent) {
        candidates.push(directory.to_path_buf());

        // `target/release`, `target/debug` and `target/debug/deps` are two or three directories below
        // the project root, which is where `vendor/lib` is.
        for ancestor in directory.ancestors().skip(1).take(4) {
            candidates.push(ancestor.join("vendor").join("lib"));
            candidates.push(ancestor.join("vendor"));
        }
    }

    if let Some(working_directory) = working_directory {
        candidates.push(working_directory.join("vendor").join("lib"));
        candidates.push(working_directory.join("vendor"));
        candidates.push(working_directory.to_path_buf());
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
    use windows::core::PCSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::GetProcAddress;

    let spelled = CString::new(name)
        .map_err(|_| AppError::PdfiumLibrary(format!("{name} is not a valid symbol name")))?;

    let address = unsafe { GetProcAddress(HMODULE(library), PCSTR(spelled.as_ptr() as *const u8)) };

    match address {
        Some(address) => Ok(unsafe { std::mem::transmute_copy(&address) }),
        None => Err(AppError::PdfiumLibrary(format!(
            "{name} is missing from this build of {LIBRARY_NAME}; the library and vendor/include \
             should both be Pdfium {VENDORED_VERSION}"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The project root the tests pretend to have been built in. Nothing is read from it: what is
    /// being pinned is which directories a *shape* of build tree leads the search to.
    const ROOT: &str = "C:/projects/cheap-note";

    /// A release executable finds the repository's own library — which is where a release build is
    /// normally started from, and what it used to fail to find.
    ///
    /// The regression in full: `target/release/cheap-note.exe`, double-clicked, is started *by* its own
    /// directory and has the project root two directories above it, with `pdfium.dll` in `vendor/lib`
    /// there. The search looked next to the executable and in the working directory — which for that
    /// launch *is* the executable's directory — so the one arrangement a release build is used in was
    /// the one arrangement that could not find the library.
    #[test]
    fn a_release_executable_finds_the_repositorys_library() {
        let root = Path::new(ROOT);
        let release = root.join("target").join("release");
        let candidates = candidates_for(Some(&release.join("cheap-note.exe")), None);

        assert_eq!(
            candidates.first(),
            Some(&release),
            "next to the executable comes first: that is where a build puts the library"
        );
        assert!(
            candidates.contains(&root.join("vendor").join("lib")),
            "and the repository's copy is found by walking up from it: {candidates:?}"
        );
    }

    /// The same for the two executables a development tree has: the debug one, and the one the tests
    /// themselves are run from, which sits a directory deeper.
    #[test]
    fn a_debug_executable_and_a_test_executable_find_it_too() {
        let root = Path::new(ROOT);
        let wanted = root.join("vendor").join("lib");

        for executable in [
            root.join("target").join("debug").join("cheap-note.exe"),
            root.join("target")
                .join("debug")
                .join("deps")
                .join("cheap_note-1a2b3c4d.exe"),
        ] {
            let candidates = candidates_for(Some(&executable), None);

            assert!(
                candidates.contains(&wanted),
                "{}: {candidates:?}",
                executable.display()
            );
        }
    }

    /// The working directory is what a `cargo run` from the project root has — and a process that knows
    /// neither its own path nor a working directory is told so rather than handed a guess.
    #[test]
    fn the_working_directory_is_searched_with_nothing_better_to_go_on() {
        let root = Path::new(ROOT);

        assert!(
            candidates_for(None, None).is_empty(),
            "nowhere to look, and no pretending otherwise"
        );
        assert_eq!(
            candidates_for(None, Some(root)),
            vec![
                root.join("vendor").join("lib"),
                root.join("vendor"),
                root.to_path_buf(),
            ],
            "the working directory's own vendor/lib is what a run from the project root finds"
        );
    }

    /// The C layouts are part of the ABI, and nothing else checks them: a field inserted in the
    /// wrong place would still compile and then be read by Pdfium as a different value. These are
    /// the layouts `vendor/include` declares on the x64 Windows build this app ships.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_c_types_have_the_layout_the_vendored_header_declares() {
        use std::mem::{offset_of, size_of};

        assert_eq!(size_of::<SizeF>(), 8, "two f32s, no padding");
        assert_eq!(offset_of!(SizeF, width), 0);
        assert_eq!(offset_of!(SizeF, height), 4);

        assert_eq!(size_of::<Pause>(), 24, "int, callback, user pointer");
        assert_eq!(offset_of!(Pause, version), 0);
        assert_eq!(offset_of!(Pause, need_to_pause_now), 8);
        assert_eq!(offset_of!(Pause, user), 16);

        assert_eq!(
            size_of::<LibraryConfig>(),
            32,
            "the version-2 prefix: int, paths, isolate, slot"
        );
        assert_eq!(offset_of!(LibraryConfig, version), 0);
        assert_eq!(offset_of!(LibraryConfig, user_font_paths), 8);
        assert_eq!(offset_of!(LibraryConfig, isolate), 16);
        assert_eq!(offset_of!(LibraryConfig, v8_embedder_slot), 24);
    }
}
