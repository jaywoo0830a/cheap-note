use super::*;
use note_format::mapped::MappedBytes;

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

    let (width, height, pixels) = job.pixels().expect("a finished page has pixels");

    (width, height, pixels.into_vec())
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

    let document = Document::open(
        String::from("one-page.pdf"),
        MappedBytes::solid(a_one_page_pdf()),
    )
    .expect("the fixture opens");

    let mut job = RenderJob::new(&document, 0, 100, Quarters::UPRIGHT, Format::Bgra, false)
        .expect("the page loads");

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

/// Renders a job that has not started yet to completion, with a give-up time.
///
/// The tests below share this rather than each writing their own loop: what they are asking is
/// "does this finish", and three hand-written loops would be three chances to forget the limit.
fn finish(job: &mut RenderJob) {
    let give_up_at = Instant::now() + Duration::from_secs(30);

    while !matches!(job.advance(Duration::from_millis(100)), Progress::Finished) {
        assert!(Instant::now() < give_up_at, "the fixture did not finish");
    }
}

/// A colour page is rendered into the app's own buffer, and handed on without a second allocation.
///
/// The external-buffer path is only worth having if the bytes are still the page: a bitmap over a
/// buffer whose stride or format Pdfium disagreed with would render somewhere this app never looks,
/// and the page would come back blank. So this checks both halves — that the pixels are *shared*
/// rather than copied, and that they are a page, with the paper written and the length its geometry.
#[test]
fn a_colour_page_is_rendered_into_the_buffer_it_is_handed_on_in() {
    if !crate::pdfium::available() {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    }
    let _pdfium = crate::pdfium::exclusive();

    let document = Document::open(
        String::from("one-page.pdf"),
        MappedBytes::solid(a_one_page_pdf()),
    )
    .expect("the fixture opens");
    let mut job = RenderJob::new(&document, 0, 100, Quarters::UPRIGHT, Format::Bgra, false)
        .expect("the page loads");

    finish(&mut job);
    let (width, height, pixels) = job.pixels().expect("a finished page has pixels");

    assert!(
        pixels.is_shared(),
        "a colour page was copied out of the buffer it was rendered into"
    );
    assert_eq!(
        pixels.len(),
        100 * 200 * 4,
        "the buffer is four bytes a pixel with no stride padding"
    );
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [0xff; 4]),
        "the paper was not written before Pdfium drew over it"
    );
    assert_eq!((width, height), (100, 200));
}

/// A grayscale page is still widened to BGRA, and says so.
///
/// The one path the external buffer does not cover: Pdfium's gray bitmap is one byte a pixel and the
/// renderer uploads four, so that conversion is a copy by definition. The pixels therefore come back
/// `Copied`, which is the difference a caller can see — and the reason seeing it in a profile is not
/// a bug.
#[test]
fn a_grayscale_page_is_widened_and_not_shared() {
    if !crate::pdfium::available() {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    }
    let _pdfium = crate::pdfium::exclusive();

    let document = Document::open(
        String::from("one-page.pdf"),
        MappedBytes::solid(a_one_page_pdf()),
    )
    .expect("the fixture opens");
    let mut job = RenderJob::new(
        &document,
        0,
        50,
        Quarters::UPRIGHT,
        Format::Grayscale,
        false,
    )
    .expect("the page loads");

    finish(&mut job);
    let (width, height, pixels) = job.pixels().expect("a finished page has pixels");

    assert!(
        !pixels.is_shared(),
        "a grayscale page claimed to already be in the renderer's format"
    );
    assert_eq!(pixels.len(), width as usize * height as usize * 4);
    assert_eq!(
        &pixels[..4],
        &[0xff, 0xff, 0xff, 0xff],
        "a blank page's one channel becomes opaque white in all three"
    );
}

/// A budget of zero does not enter Pdfium at all.
///
/// The pause callback ends a slice at the first piece of work, but only after Pdfium has been
/// entered and has begun — a parse, a resource, a page's setup — which is exactly the work the
/// caller said it had no time for. This pins the stronger promise, and then the consequence that
/// matters: a job which was never entered still starts properly when it is finally given time.
#[test]
fn a_budget_of_zero_does_not_call_pdfium_and_starts_cleanly_after() {
    if !crate::pdfium::available() {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    }
    let _pdfium = crate::pdfium::exclusive();

    let document = Document::open(
        String::from("one-page.pdf"),
        MappedBytes::solid(a_one_page_pdf()),
    )
    .expect("the fixture opens");
    let mut job = RenderJob::new(&document, 0, 100, Quarters::UPRIGHT, Format::Bgra, false)
        .expect("the page loads");

    for slice in 0..5 {
        assert_eq!(
            job.advance(Duration::ZERO),
            Progress::Unfinished,
            "slice {slice} of no time did something"
        );
    }

    assert!(
        job.pixels().is_none(),
        "nothing was rendered, so nothing may be drawn"
    );

    // The first call with a real budget is `Start`, not a `Continue` on a render that does not
    // exist: the page would fail to draw at all if a zero-budget call had marked the job as begun.
    finish(&mut job);
    let (width, height, pixels) = job.pixels().expect("a finished page has pixels");
    assert_eq!((width, height), (100, 200));
    assert_eq!(pixels.len(), 100 * 200 * 4);
}

/// A failed render names the page, the size and the call — and asks for no error code.
///
/// `FPDF_GetLastError` is documented for the document loaders and for nothing else, so a render
/// failure has no code to report. What it *can* say is which page at which size failed in which of
/// the two calls, and that is what the sentence has to contain for a log to be worth reading.
#[test]
fn a_failed_render_names_the_page_the_size_and_the_call() {
    if !crate::pdfium::available() {
        eprintln!("skipping: pdfium.dll is not available");
        return;
    }
    let _pdfium = crate::pdfium::exclusive();

    let document = Document::open(
        String::from("one-page.pdf"),
        MappedBytes::solid(a_one_page_pdf()),
    )
    .expect("the fixture opens");
    let job = RenderJob::new(&document, 0, 100, Quarters::UPRIGHT, Format::Bgra, false)
        .expect("the page loads");

    // The sentence `advance` writes, asked of the function it writes it with: what a failure says
    // has to be checkable without making Pdfium fail.
    let started = job.failure(
        CALL_START,
        RENDER_FAILED,
        "Pdfium could not render the page",
    );
    assert!(
        started.contains("page 1 of one-page.pdf"),
        "the page is not named: {started}"
    );
    assert!(
        started.contains("100×200 px"),
        "the size is not named: {started}"
    );
    assert!(
        started.contains(CALL_START),
        "the call that failed is not named: {started}"
    );
    assert!(
        started.contains("status 3"),
        "the status is not named: {started}"
    );

    let continued = job.failure(
        CALL_CONTINUE,
        RENDER_FAILED,
        "Pdfium could not render the page",
    );
    assert!(
        continued.contains(CALL_CONTINUE),
        "a later slice is reported as the first: {continued}"
    );
    assert!(
        !continued.contains("Pdfium error"),
        "a render failure claimed an error code the header does not promise: {continued}"
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

    let document = Document::open(
        String::from("one-page.pdf"),
        MappedBytes::solid(a_one_page_pdf()),
    )
    .expect("the fixture opens");
    let mut job = RenderJob::new(&document, 0, 100, Quarters::UPRIGHT, Format::Bgra, false)
        .expect("the page loads");

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
