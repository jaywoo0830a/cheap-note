//! The parts of Pdfium that belong to a document: its bytes, its pages, its outline, and its text.
//!
//! ## A document owns the bytes Pdfium reads
//!
//! `FPDF_LoadMemDocument64` does not copy the buffer it is given: Pdfium reads the file from that
//! memory for as long as the document is open. [`DocumentInner`] therefore keeps the bytes beside
//! the handle, and closes the handle before the vector is freed.
//!
//! ## A page keeps its document alive
//!
//! A page handle belongs to a document, and closing the document underneath it leaves Pdfium with
//! a pointer it must not use. [`Page`] holds an `Arc` clone of the document's inner state, so this
//! is a fact of the types rather than a rule every caller has to remember: a page — and the render
//! job that owns one — cannot outlive the document it came from. This is what lets the app keep a
//! job and its document in one struct without a self-referential lifetime.
//!
//! ## Text and outline are copies, not handles
//!
//! Both readers release every Pdfium handle before returning: what comes back is plain Rust data.
//! That is deliberate. A page's chars are read when the page is turned to, and an outline is read
//! when the document is opened, so neither may hold a handle across frames — and a caller cannot
//! accidentally keep one alive.

use std::ffi::{c_int, c_ulong};
use std::marker::PhantomData;
use std::sync::Arc;

use super::api::{
    api, FpdfAction, FpdfBookmark, FpdfDest, FpdfDocument, FpdfPage, FpdfTextPage, SizeF,
};
use crate::error::{AppError, Result};

/// Pdfium's `PDFACTION_GOTO`: an action that jumps to a destination in this document.
///
/// The other kinds — a URI, a file, another document — are the ones an outline entry can carry and
/// this app cannot follow, and they are why an entry's page is an `Option`.
const ACTION_GOTO: c_ulong = 1;

/// How deep an outline is followed.
///
/// A real table of contents is two or three levels; a malformed file can describe a tree that never
/// ends, and Pdfium's own documentation puts the handling of circular bookmark references on the
/// caller. A cap is therefore not a formatting opinion — it is what keeps a broken document from
/// being an unbounded walk.
const OUTLINE_DEPTH: usize = 32;

/// How many entries an outline may hold, across the whole tree.
///
/// The second bound on the same walk: depth catches a cycle that goes *down*, and this catches one that
/// goes *sideways* — a sibling chain that loops, or a file that simply describes a hundred thousand
/// entries. A table of contents larger than this is not a table of contents.
const OUTLINE_ENTRIES: usize = 4096;

/// The most bytes a single outline title may occupy, including its UTF-16 terminator.
///
/// A title is a line in a list, not a document of its own. The cap is generous enough for a chapter
/// heading in any language, and it keeps a malformed length field from becoming an allocation the
/// size of the file.
const OUTLINE_TITLE_BYTES: c_ulong = 64 * 1024;

/// The most characters one page may contribute to the highlighter.
///
/// Pdfium reports the number before the caller asks for the characters, and a crafted file can
/// make that number large. A page with more than a million characters is not a page a person is
/// going to highlight by hand; the cap keeps the reader's cost proportional to what it can use.
const TEXT_CHARS: c_int = 1_000_000;

/// One entry of a document's outline: the table of contents the document itself carries.
///
/// A tree rather than a list, because that is what a table of contents is: chapters with sections under
/// them, and the depth is the document's meaning rather than a formatting choice. The app draws it
/// indented and flattened (see [`outline`](mod@crate::outline)), which is the one thing this type does not do for
/// itself.
///
/// `page` is `None` for an entry this app cannot follow: one that opens a link, a file, or a destination
/// in another document. Such an entry is still *kept* — it is a line of the document's own contents, and
/// dropping it would quietly shorten what the document says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutlineEntry {
    /// The title the document gives the entry, in its own words.
    pub title: String,
    /// The page of *this* document the entry opens, when it opens one.
    pub page: Option<usize>,
    /// What is nested under it, in the document's order.
    pub children: Vec<OutlineEntry>,
}

/// One character of a page, and the box it is drawn in: what a highlighter snaps to.
///
/// The box is in the page's own points, with the origin at its **bottom-left** — Pdfium's convention for text, as
/// it is for the page itself — so putting one on the paper means flipping the y (see `ink::paper_of_page_point`).
/// `ch` is the character as Unicode and may be a space or a line break: Pdfium counts the characters it
/// *generates* as well as the glyphs the file draws, which is what makes the stream a page's text rather than a
/// list of marks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextChar {
    /// The character, as Unicode. Zero when Pdfium cannot map one.
    pub ch: char,
    /// The box's left edge, in page points.
    pub left: f32,
    /// The box's right edge.
    pub right: f32,
    /// The box's bottom edge — nearer the page's bottom-left corner, in PDF's upward y.
    pub bottom: f32,
    /// The box's top edge.
    pub top: f32,
}

/// An open PDF document, and the bytes it was opened from.
///
/// Cloning the outer value is not offered: one document is one Pdfium handle, shared only with the
/// pages and render jobs that keep it alive internally.
pub struct Document {
    /// Shared with every [`Page`] the document hands out, so a page cannot outlive the document.
    inner: Arc<DocumentInner>,
}

/// The state that must live exactly as long as Pdfium's document handle.
pub(super) struct DocumentInner {
    /// The handle Pdfium handed out.
    handle: FpdfDocument,
    /// The name to show for it.
    name: String,
    /// The bytes the document was opened from, held for as long as the handle lives.
    ///
    /// Underscored because nothing in this crate *reads* it: it is not a cache, it is the storage
    /// Pdfium reads from in place. Dropping it while the document is open would leave Pdfium
    /// reading freed memory, which is why it is a field of the inner state rather than a local.
    _buffer: Vec<u8>,
    /// Pdfium is not thread-safe, and a handle is only meaningful on the thread that made it.
    ///
    /// A raw pointer would already make this type `!Send` and `!Sync`; the marker states the
    /// reason in one place, so a future field cannot accidentally make it movable.
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl Drop for DocumentInner {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.close_document)(self.handle) }
        }
    }
}

impl Document {
    /// Opens a document from bytes read elsewhere.
    ///
    /// The bytes are moved into the document rather than copied: Pdfium reads them in place for as
    /// long as the handle is open, and the app already has exactly one owned copy.
    pub fn open(name: String, bytes: Vec<u8>) -> Result<Self> {
        let api = api()?;

        if bytes.is_empty() {
            return Err(AppError::Pdf(format!("{name} is empty")));
        }

        let handle = unsafe {
            (api.load_memory_document)(bytes.as_ptr().cast(), bytes.len(), std::ptr::null())
        };

        if handle.is_null() {
            return Err(AppError::Pdf(format!(
                "{name} could not be read (Pdfium error {})",
                unsafe { (api.last_error)() }
            )));
        }

        // The `Arc` is a lifetime handle, not a thread-sharing one: it exists so a page can keep
        // its document open, and the inner value is deliberately `!Send` because Pdfium is not
        // thread-safe. Clippy reads `Arc` as "shared between threads" and warns; that is the
        // property this code is avoiding on purpose.
        #[allow(clippy::arc_with_non_send_sync)]
        let inner = Arc::new(DocumentInner {
            handle,
            name,
            _buffer: bytes,
            _not_send_or_sync: PhantomData,
        });

        Ok(Document { inner })
    }

    /// The name to show for the document.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// How many pages the document has.
    pub fn page_count(&self) -> usize {
        self.inner.page_count()
    }

    /// A page's own size in PDF points, without loading it.
    pub fn page_point_size(&self, index: usize) -> Option<(f32, f32)> {
        self.inner.page_point_size(index)
    }

    /// A page's rotation, in quarter turns clockwise.
    ///
    /// Loading the page to ask is the only way Pdfium offers: the rotation belongs to the page
    /// object, not to the document's page table.
    pub fn page_rotation(&self, index: usize) -> u8 {
        let Ok(api) = api() else {
            return 0;
        };
        let Some(page) = Page::load(self, index) else {
            return 0;
        };

        unsafe { (api.page_rotation)(page.handle()) }.clamp(0, 3) as u8
    }

    /// The characters of a page, with the box each is drawn in.
    ///
    /// A page with no text — a scan, a drawing, a blank sheet — answers with nothing rather than failing: that is
    /// the common case for the pages a note is written on, and the highlighter's own answer to it (a freehand
    /// band) is not an error.
    ///
    /// Costs one page load and one text-page load, both released before this returns: what comes back is copies of
    /// numbers, so nothing here outlives the call and no handle is kept between frames.
    pub fn text_chars(&self, index: usize) -> Result<Vec<TextChar>> {
        let api = api()?;

        let Some(page) = Page::load(self, index) else {
            return Ok(Vec::new());
        };

        let text = unsafe { (api.text_load_page)(page.handle()) };
        if text.is_null() {
            return Ok(Vec::new());
        }

        // The guard releases the text page on every path out, including a panic in the loop.
        let text = TextPage { handle: text };
        let count = unsafe { (api.text_count_chars)(text.handle) }.clamp(0, TEXT_CHARS);
        let mut chars = Vec::with_capacity(count as usize);

        for at in 0..count {
            let (mut left, mut right, mut bottom, mut top) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);

            let ok = unsafe {
                (api.text_char_box)(
                    text.handle,
                    at,
                    &mut left,
                    &mut right,
                    &mut bottom,
                    &mut top,
                )
            };

            if ok == 0 {
                continue;
            }

            let code = unsafe { (api.text_unicode)(text.handle, at) };

            chars.push(TextChar {
                ch: char::from_u32(code).unwrap_or('\u{0}'),
                left: left as f32,
                right: right as f32,
                bottom: bottom as f32,
                top: top as f32,
            });
        }

        Ok(chars)
    }

    /// The document's outline — the table of contents it carries — as a tree.
    ///
    /// Read once, when the document is opened, rather than when the app asks: an outline is a few dozen
    /// titles and page numbers, walking it is a handful of calls into Pdfium, and asking for it on a
    /// frame (which is when the app wants it) is exactly the kind of work a frame must not do. It is
    /// also read from the *document*, so it needs no page handles and survives any render in flight.
    ///
    /// A document with no outline answers with nothing, which is the common case and not a failure: most
    /// PDFs have no contents. An entry that points out of the document is kept with no page (see
    /// [`OutlineEntry`]), and the walk is bounded twice: Pdfium hands out circular references as easily
    /// as it hands out titles, and the caller is the one who has to notice.
    pub fn outline(&self) -> Vec<OutlineEntry> {
        self.inner.outline()
    }

    /// One more owner of the same document, for a page to keep alive.
    ///
    /// Private and infallible: it is the `Arc` clone, not a second Pdfium handle. A page holds one
    /// of these so that pdfium's page handle can never outlive its document.
    fn shared(&self) -> Self {
        Document {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl DocumentInner {
    /// How many pages the document has.
    pub(super) fn page_count(&self) -> usize {
        match api() {
            Ok(api) => unsafe { (api.page_count)(self.handle) }.max(0) as usize,
            Err(_) => 0,
        }
    }

    /// A page's own size in PDF points, without loading it.
    pub(super) fn page_point_size(&self, index: usize) -> Option<(f32, f32)> {
        let api = api().ok()?;
        let index = c_int::try_from(index).ok()?;

        let mut size = SizeF::default();
        let ok = unsafe { (api.page_size)(self.handle, index, &mut size) };

        (ok != 0
            && size.width.is_finite()
            && size.height.is_finite()
            && size.width > 0.0
            && size.height > 0.0)
            .then_some((size.width, size.height))
    }

    /// The document's outline, as a tree.
    fn outline(&self) -> Vec<OutlineEntry> {
        let Ok(api) = api() else {
            return Vec::new();
        };

        let mut budget = OUTLINE_ENTRIES;
        unsafe { outline_children(api, self.handle, std::ptr::null_mut(), 0, &mut budget) }
    }
}

/// An open page, kept alive by the document it belongs to.
///
/// Private to this module and the renderer: a caller gets a page by asking [`Document`] for
/// something — text, rotation, pixels — and the handle is closed before the answer is returned, or
/// owned by a [`RenderJob`](super::RenderJob) that closes it when the job ends.
pub(super) struct Page {
    /// Not read: the document is held so that the page handle cannot outlive it. Pdfium's API is
    /// explicit that page handles belong to their document.
    _document: Document,
    /// The page handle.
    handle: FpdfPage,
}

impl Page {
    /// Loads one page, keeping the document that owns it alive.
    ///
    /// `None` means Pdfium would not load the page: an index past the end, or a page object a
    /// damaged file describes but cannot open. The caller decides whether that is an empty answer
    /// (a rotation of zero, no text) or an error (a render that was asked for).
    pub(super) fn load(document: &Document, index: usize) -> Option<Self> {
        let api = api().ok()?;
        let index = c_int::try_from(index).ok()?;
        let handle = unsafe { (api.load_page)(document.inner.handle, index) };

        if handle.is_null() {
            return None;
        }

        Some(Page {
            _document: document.shared(),
            handle,
        })
    }

    /// The handle this page owns.
    pub(super) fn handle(&self) -> FpdfPage {
        self.handle
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.close_page)(self.handle) }
        }
    }
}

/// A page's text object: the character stream loaded from a [`Page`].
///
/// It is a guard rather than a bare handle so every early return releases it; the [`Page`] it came
/// from is dropped afterwards, which is the order Pdfium's API expects.
struct TextPage {
    handle: FpdfTextPage,
}

impl Drop for TextPage {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            unsafe { (api.text_close_page)(self.handle) }
        }
    }
}

/// One level of the outline: the children of `parent`, and everything below them.
///
/// `parent` is null at the top, which is how Pdfium spells "the first top-level entry". The budget is
/// shared across the whole walk rather than per level, so a tree that is wide *and* deep cannot get
/// around it.
unsafe fn outline_children(
    api: &super::api::Api,
    document: FpdfDocument,
    parent: FpdfBookmark,
    depth: usize,
    budget: &mut usize,
) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    if depth >= OUTLINE_DEPTH {
        return entries;
    }

    let mut node = unsafe { (api.bookmark_first_child)(document, parent) };
    while !node.is_null() && *budget > 0 {
        *budget -= 1;

        let title = unsafe { bookmark_title(api, node) };
        let page = unsafe { bookmark_page(api, document, node) };
        let children = unsafe { outline_children(api, document, node, depth + 1, budget) };

        entries.push(OutlineEntry {
            title,
            page,
            children,
        });

        node = unsafe { (api.bookmark_next_sibling)(document, node) };
    }

    entries
}

/// One entry's title.
///
/// Pdfium answers in UTF-16LE with the length in *bytes*, including the terminating NUL, and it answers
/// that length whether or not a buffer is handed over — so the title is measured first and collected
/// second, which is what keeps a fixed-size buffer from truncating a long chapter name. The measured
/// length is capped: a title is one line of a list, and a malformed file must not turn its length into
/// an allocation the size of the file.
unsafe fn bookmark_title(api: &super::api::Api, bookmark: FpdfBookmark) -> String {
    let bytes = unsafe { (api.bookmark_title)(bookmark, std::ptr::null_mut(), 0) };
    if bytes == 0 {
        return String::new();
    }

    // The length is a byte count of UTF-16 code units plus a terminator, and a malformed file may make
    // it odd: rounded up, because the buffer has to hold what Pdfium is about to write into it.
    let bytes = bytes.min(OUTLINE_TITLE_BYTES);
    let mut buffer = vec![0u16; bytes.div_ceil(2) as usize];
    let written = unsafe {
        (api.bookmark_title)(
            bookmark,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 2) as c_ulong,
        )
    };

    // Trimming the terminator rather than sizing around it: the two lengths can disagree, and the one
    // that says how much was written is the one to trust. A title with a NUL in the middle keeps it —
    // this only trims the end.
    let units = (written as usize / 2).min(buffer.len());
    buffer.truncate(units);

    while buffer.last() == Some(&0) {
        buffer.pop();
    }

    String::from_utf16_lossy(&buffer)
}

/// The page an entry opens, when it opens one of this document's own pages.
///
/// Two ways to say the same thing, and a table of contents uses both: an entry can carry a
/// *destination*, or an *action* that has one inside it (a link out of the document is the other kind of
/// action, and it is what makes this an `Option`). A destination that resolves to no page — a name that
/// is not in the document, a file that has been edited since its contents were written — is answered the
/// same way, as no page, rather than as page zero.
unsafe fn bookmark_page(
    api: &super::api::Api,
    document: FpdfDocument,
    bookmark: FpdfBookmark,
) -> Option<usize> {
    let dest: FpdfDest = unsafe { (api.bookmark_dest)(document, bookmark) };

    let dest = if dest.is_null() {
        let action: FpdfAction = unsafe { (api.bookmark_action)(bookmark) };
        if action.is_null() || unsafe { (api.action_type)(action) } != ACTION_GOTO {
            return None;
        }

        unsafe { (api.action_dest)(document, action) }
    } else {
        dest
    };

    if dest.is_null() {
        return None;
    }

    let index = unsafe { (api.dest_page_index)(document, dest) };
    (index >= 0).then_some(index as usize)
}
