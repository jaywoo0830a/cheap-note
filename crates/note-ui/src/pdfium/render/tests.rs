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

    let document =
        Document::open(String::from("one-page.pdf"), MappedBytes::solid(a_one_page_pdf())).expect("the fixture opens");

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

    let document =
        Document::open(String::from("one-page.pdf"), MappedBytes::solid(a_one_page_pdf())).expect("the fixture opens");
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
