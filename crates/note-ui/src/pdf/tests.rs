use std::path::PathBuf;

/// Opens a fixture document the way the app does: the file's bytes, with the file's name.
///
/// The app has no "open a path" call — see [`PdfDocumentView::open_bytes`] — so the tests go in
/// through the same door, which is also what keeps this helper honest: if opening from bytes ever
/// stops working, every test here fails rather than the one test that remembered to cover it.
fn open_fixture(path: &std::path::Path, width: u32) -> Result<PdfDocumentView> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("fixture.pdf"));

    let bytes = std::fs::read(path).map_err(|error| {
        AppError::Other(format!("{} could not be read: {error}", path.display()))
    })?;

    PdfDocumentView::open_bytes(name, bytes, width)
}

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
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
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

/// A three-page PDF that carries an outline, built object by object.
///
/// The outline is the document's *own* table of contents: a tree of titled destinations. This fixture
/// has the shapes a real one has, because each is a different branch of the walk that reads it — two
/// top-level chapters, a section nested under one of them, a destination written as a page reference and
/// another written as a *name*, a title written as UTF-16 with a Korean character in it, and an entry
/// that links *out* of the document instead of opening a page.
///
/// The cross-reference offsets are real, as in [`a_page_with_rectangles`]: a table of contents that
/// Pdfium had to repair would let a broken fixture pass for the wrong reason.
fn a_document_with_contents() -> Vec<u8> {
    let objects = [
        // 1: the catalog, which is what says the document has an outline at all — and which carries
        // the name tree the outline's named destination is looked up in.
        String::from(
            "<< /Type /Catalog /Pages 2 0 R /Outlines 4 0 R /PageMode /UseOutlines \
             /Names << /Dests 11 0 R >> >>",
        ),
        // 2: three pages, in the order the outline's page numbers are counted in.
        String::from("<< /Type /Pages /Kids [3 0 R 5 0 R 10 0 R] /Count 3 >>"),
        String::from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] >>"),
        // 4: the outline's root: the two chapters below it, and how many entries there are.
        String::from("<< /Type /Outlines /First 6 0 R /Last 7 0 R /Count 4 >>"),
        String::from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] >>"),
        // 6: a chapter that names its page, with a title written as UTF-16 — "1장".
        String::from("<< /Title <FEFF0031C7A5> /Parent 4 0 R /Dest [3 0 R /Fit] /Next 7 0 R >>"),
        // 7: a chapter that has a section under it, where the section is a *later* page.
        String::from(
            "<< /Title (Chapter two) /Parent 4 0 R /Dest [5 0 R /Fit] /Prev 6 0 R \
             /First 8 0 R /Last 9 0 R /Count 2 >>",
        ),
        // 8: a section, linked forward to the entry beside it — children are a chain like their parents
        // are, and Pdfium walks *that* chain rather than the `Last` pointer. Its destination is a
        // *name*, which is how a table of contents written by a word processor usually points at a page.
        String::from(
            "<< /Title (Two, part one) /Parent 7 0 R /Dest (chapter three) /Next 9 0 R >>",
        ),
        // 9: no destination of its own — an action that leaves the document.
        String::from(
            "<< /Title (An online appendix) /Parent 7 0 R /Prev 8 0 R \
             /A << /S /URI /URI (https://example.com/appendix) >> >>",
        ),
        String::from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] >>"),
        // 11: the name tree that `chapter three` is looked up in.
        String::from("<< /Names [(chapter three) [10 0 R /Fit]] >>"),
    ];

    let mut out = String::from("%PDF-1.7\n");
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
/// file format) do not. The lock itself lives in `pdfium` so the lifetime tests beside the
/// wrapper take the same one; two locks would serialize nothing.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    crate::pdfium::exclusive()
}

/// The fixture on disk, or `None` when Pdfium is not available.
///
/// Written once for the process and never deleted: the tests run in parallel threads, and a
/// fixture that one of them removed would fail the others for a reason that has nothing to do
/// with what they are testing. The name carries the process id, so two test runs at once do not
/// tread on each other either.
fn a_one_page_pdf() -> Option<PathBuf> {
    written_once("page", a_page_with_rectangles)
}

/// The document with an outline on disk, or `None` when Pdfium is not available.
fn the_contents_pdf() -> Option<PathBuf> {
    written_once("contents", a_document_with_contents)
}

/// A fixture written to the temp directory the first time it is asked for, and kept after that.
///
/// One mechanism for every fixture, so that a test which needs a *document* and a test which needs a
/// table of contents do not each grow their own `OnceLock`: the name is the key, and a second call
/// with the same name answers with the file already written.
fn written_once(name: &str, build: fn() -> Vec<u8>) -> Option<PathBuf> {
    static WRITTEN: std::sync::Mutex<Vec<(String, Option<PathBuf>)>> =
        std::sync::Mutex::new(Vec::new());

    // Pdfium missing is a state the suite is built for: a test that needs a document says so and
    // returns, rather than failing for a reason the person running it cannot act on.
    if !crate::pdfium::available() {
        return None;
    }

    let mut written = WRITTEN.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((_, path)) = written.iter().find(|(written, _)| written == name) {
        return path.clone();
    }

    let path = std::env::temp_dir().join(format!("cheap-note-{name}-{}.pdf", std::process::id()));

    let path = std::fs::write(&path, build()).ok().map(|()| path);
    written.push((name.to_string(), path.clone()));
    path
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

    let page = view
        .render_page(PageRequest::new(0, 200))
        .expect("the page renders");
    assert_eq!(page.pixels, 200, "the rung it was asked for");
    assert!(page.point_height > page.point_width, "A4 is portrait");
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

    let mut view = open_fixture(&path, 1_024).expect("the document opens");
    view.render_page(PageRequest::new(0, 1_536))
        .expect("the second rung");
    let rendered = view.stats().rendered;

    // Out to the first rung, and back to the second: neither is a rasterisation.
    let first = view
        .render_page(PageRequest::new(0, 1_024))
        .expect("the first rung again");
    let second = view
        .render_page(PageRequest::new(0, 1_536))
        .expect("the second rung again");

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

    let mut view = open_fixture(&path, 1_024).expect("the document opens");
    let rendered = view.stats().rendered;

    // A rung nobody has rendered: the frame gets the one that is there, and what it asked for
    // is left owed to the caller.
    let wanted = PageRequest::new(0, 3_456);
    let before = view.stats();
    let page = view
        .page_for_frame(wanted)
        .expect("the cached rung stands in");

    assert_eq!(page.pixels, 1_024, "a placeholder, not the rung asked for");
    assert!(
        view.plan(wanted).is_some(),
        "and the rung asked for is owed"
    );
    assert_eq!(
        view.stats().rendered,
        rendered,
        "the frame rasterised nothing"
    );
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

    let mut view = open_fixture(&path, 200).expect("the document opens");

    // Room for about two of these bitmaps, rather than the 128 MB the app allows.
    let rung = weight(&view.render_page(PageRequest::new(0, 200)).expect("a rung"));
    view.budget = rung * 2;

    view.render_page(PageRequest::new(0, 300))
        .expect("a second rung");
    view.render_page(PageRequest::new(0, 400))
        .expect("a third rung");

    assert!(view.stats().evicted >= 1, "the oldest bitmap went");

    let newest = view
        .render_page(PageRequest::new(0, 400))
        .expect("the newest rung");
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

    let mut view = open_fixture(&path, 300).expect("the document opens");
    assert_eq!(view.page_count(), 1);
    assert_eq!(
        view.file_name(),
        path.file_name()
            .expect("the fixture has a name")
            .to_string_lossy()
    );

    let page = view
        .render_page(PageRequest::new(0, 300))
        .expect("the page renders");
    assert!(page.point_width > 0.0, "an A4 page has a width");
    assert!(page.point_height > page.point_width, "A4 is portrait");
    assert_eq!(
        page.pixels, 300,
        "the bitmap is the width that was asked for"
    );

    // A second request at the same width must be served from the cache, not re-rasterised.
    let cached = view
        .render_page(PageRequest::new(0, 300))
        .expect("the cached page");
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
        let mut view = open_fixture(&path, 1_200).expect("the document opens");
        view.render_page(PageRequest::new(0, 1_200))
            .expect("the page renders")
    };

    let mut view = open_fixture(&path, 1_200).expect("the document opens");
    view.begin(PageRequest::new(0, 1_200))
        .expect("the render starts");

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
        .render_page(PageRequest::new(0, 1_200))
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

    let mut view = open_fixture(&path, 1_200).expect("the document opens");
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

    let mut view = open_fixture(&path, 1_200).expect("the document opens");
    let before = view.stats();

    view.begin(PageRequest::new(0, 3_456))
        .expect("a render starts");
    assert!(view.rendering());

    view.abandon();
    assert!(!view.rendering(), "the job is gone");
    assert_eq!(
        view.stats().cancelled,
        before.cancelled + 1,
        "and it was counted"
    );
    assert_eq!(
        view.stats().rendered,
        before.rendered,
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
        let mut view = open_fixture(&path, 100).expect("the document reopens");

        let started = std::time::Instant::now();
        let page = view
            .render_page(PageRequest::new(0, width))
            .expect("the page renders");
        let rasterised = started.elapsed();

        // And the same page in slices of the pump's budget, counting what it takes. The setup is
        // measured apart from the slices, because it is the part no budget can interrupt: it
        // allocates the bitmap and writes the paper into it.
        let started = std::time::Instant::now();
        view.begin(PageRequest::new(0, width))
            .expect("a sliced render");
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
            std::hint::black_box(
                view.render_page(PageRequest::new(0, width))
                    .expect("the cached page"),
            );
        }
        let cached = started.elapsed() / rounds;

        let height = (width as f32 * page.point_height / page.point_width.max(1.0)) as u64;
        let megabytes = (width as u64 * height * 4) / (1024 * 1024);
        eprintln!(
            "  a page {width:>4} px wide, {megabytes:>3} MB  rasterise {rasterised:>9.1?}   or setup {setup:>8.1?} + {slices:>3} slices, longest {longest:>8.1?} ({sliced:>8.1?} total)   then {cached:>7.1?} per frame"
        );

        // A slice is not the whole render: the paper write and the allocation happen in the
        // setup, and what is left is what a budget can stop between. On the two smallest rungs
        // the whole render is under a millisecond on this machine, which is within the noise of
        // two separate timer reads; the promise being pinned is about the rungs a slow page
        // actually uses.
        if width >= 2_880 {
            assert!(
                longest < rasterised,
                "the longest slice ({longest:?}) was no shorter than the whole render ({rasterised:?})"
            );
        }
        assert!(
            cached.as_micros() < 500,
            "a cached page cost {cached:?} per frame; it is meant to be a map lookup"
        );
    }
    eprintln!("───────────────────────────────────────────────────────────────");

    // What the cache did with all of that, and what the first frame of a page costs when the
    // only thing on offer is the cheapest rung — the number that decides whether a page turn
    // is a stall or a soft page for a frame or two.
    let mut view = open_fixture(&path, 100).expect("the document reopens");
    let started = std::time::Instant::now();
    let preview = view
        .render_page(PageRequest::new(0, 1_024))
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

/// A document's own table of contents is read as a tree, with the page each entry opens.
///
/// This is the whole of what the outline is for: a chapter's title, where it is, and what is under
/// it. The fixture carries one of each shape a real contents has — a plain chapter, a chapter with a
/// section under it, a title that is not ASCII, and an entry that leaves the document — because each
/// is a different branch of the walk.
#[test]
fn a_documents_outline_is_read_with_its_pages() {
    let _pdfium = exclusive();

    let Some(path) = the_contents_pdf() else {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    };

    let view = open_fixture(&path, 200).expect("the document opens");
    assert_eq!(view.page_count(), 3);
    assert!(view.has_outline(), "the fixture carries one");

    let outline = view.outline();
    assert_eq!(outline.len(), 2, "two top-level chapters");

    assert_eq!(outline[0].title, "1장", "a title read out of UTF-16");
    assert_eq!(
        outline[0].page,
        Some(0),
        "the chapter that opens the first page"
    );
    assert!(outline[0].children.is_empty(), "with nothing under it");

    let second = &outline[1];
    assert_eq!(second.title, "Chapter two");
    assert_eq!(second.page, Some(1));
    assert_eq!(
        second.children.len(),
        2,
        "a section and a link, in that order"
    );

    assert_eq!(second.children[0].title, "Two, part one");
    assert_eq!(
        second.children[0].page,
        Some(2),
        "a destination written as a *name*, resolved through the document's name tree"
    );

    assert_eq!(second.children[1].title, "An online appendix");
    assert_eq!(
        second.children[1].page, None,
        "an entry that links out of the document keeps its line and opens no page"
    );
}

/// A document with no outline answers with nothing at all.
///
/// The common case, and the one the app has to be able to tell apart from a document that has one:
/// the button that opens the contents is offered only when there is something to show.
#[test]
fn a_document_without_an_outline_has_none() {
    let _pdfium = exclusive();

    let Some(path) = a_one_page_pdf() else {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    };

    let view = open_fixture(&path, 200).expect("the document opens");
    assert!(!view.has_outline());
    assert!(view.outline().is_empty());
}

/// A PDF file built object by object, with a real cross-reference table.
///
/// The offsets are the bytes they claim to be, because a table Pdfium silently repairs would let
/// a broken fixture pass for the wrong reason.
fn a_pdf(objects: &[String]) -> Vec<u8> {
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

/// A one-page A4 PDF whose page is turned on its side by its own `/Rotate`, with one corner marked.
///
/// The mark is a black square in the page's own bottom-left corner — the corner that a quarter turn
/// clockwise takes to the top-left — so a render says which way up the page came out.
fn a_page_turned_by_the_document() -> Vec<u8> {
    let content = String::from("0 0 0 rg\n0 0 120 120 re f\n");

    a_pdf(&[
        String::from("<< /Type /Catalog /Pages 2 0 R >>"),
        String::from("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        String::from(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Rotate 90 /Contents 4 0 R >>",
        ),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
    ])
}

/// What Pdfium does with the rotation it is handed, and what a page's own `/Rotate` already is.
///
/// Two facts the app's page turning rests on, and neither can be settled by reading the header:
///
/// 1. **A page's size is reported the way it is drawn.** `/Rotate 90` on an A4 page answers
///    `842 × 595`, so the document's own rotation is *in* the size — which is why the app swaps only
///    for the reader's turn.
/// 2. **The rotation argument is *added* to it.** A `/Rotate 90` page rendered at `0` comes out the
///    way the document says (the marked corner at the top-left); at `1` it is a quarter further
///    round. Handing Pdfium the *sum* of the two rotations would therefore turn a `/Rotate 90` page
///    upside down, which is why the app hands it the reader's turn alone.
#[test]
fn the_rotation_handed_to_pdfium_is_the_readers_turn() {
    let _pdfium = exclusive();
    if !crate::pdfium::available() {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    }

    let document =
        crate::pdfium::Document::open(String::from("turned.pdf"), a_page_turned_by_the_document())
            .expect("the document opens");

    assert_eq!(
        document.page_point_size(0),
        Some((842.0, 595.0)),
        "the document's size for the page has the page's own /Rotate in it"
    );
    assert_eq!(
        document.page_rotation(0),
        1,
        "and it says the page is on its side"
    );

    // The mark, read at the four corners of the bitmap: top-left, top-right, bottom-left, and
    // bottom-right. The page's mark is in its own bottom-left corner, which a quarter turn
    // clockwise takes to the top-left.
    let marked_corners = |turns: crate::pages::Quarters| {
        let mut job = RenderJob::new(&document, 0, 400, turns, crate::pdfium::Format::Bgra, false)
            .expect("a job");

        // Advanced in slices until it is done, the way the app does it: Pdfium finishes when it
        // finishes, and what this test is about is *which way up* the page came out rather than a
        // budget — so a loaded machine must not be able to fail it.
        let give_up_at = Instant::now() + Duration::from_secs(60);
        let mut progress = Progress::Unfinished;
        while progress == Progress::Unfinished && Instant::now() < give_up_at {
            progress = job.advance(Duration::from_secs(1));
        }

        assert_eq!(progress, Progress::Finished, "the fixture renders");
        let (width, height, pixels) = job.pixels().expect("the page's pixels");
        let (width, height) = (width as usize, height as usize);

        // The shape the app sizes the bitmap with: the document's size for the page, swapped for
        // an odd turn. Pdfium stretched the page into exactly this, which is what makes the bitmap
        // the shape of the page the reader sees.
        assert_eq!(width, 400, "the width asked for is the bitmap's width");
        assert_eq!(
            height,
            if crate::pages::swaps_axes(turns) {
                566
            } else {
                283
            },
            "and its height is the drawn page's shape"
        );

        let marked = |x: usize, y: usize| pixels[y * width * 4 + x * 4] < 128;

        [
            marked(width / 8, height / 8),
            marked(width * 7 / 8, height / 8),
            marked(width / 8, height * 7 / 8),
            marked(width * 7 / 8, height * 7 / 8),
        ]
    };

    assert_eq!(
        marked_corners(0),
        [true, false, false, false],
        "no turn from the reader is the page the document draws"
    );
    assert_eq!(
        marked_corners(1),
        [false, true, false, false],
        "one turn is added to the page's own /Rotate, not taken instead of it"
    );
    assert_eq!(
        marked_corners(2),
        [false, false, false, true],
        "upside down from the document's own"
    );
    assert_eq!(
        marked_corners(3),
        [false, false, true, false],
        "and the other quarter turn"
    );
}
