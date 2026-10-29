//! A page's ink as one blob, from the outside: the round trip, the checksum, and where a page is cut.
//!
//! This is the byte format itself, and the one place where "another application can read this" is a
//! claim about *bytes* rather than about tables: a page's strokes are a structure of arrays, quantised
//! to 1/64 of a pixel, compressed with a codec the chunk names, and sealed with a CRC32. What is
//! asserted here is what a second implementation must reproduce: the same points back, the same bytes
//! out of the same ink, and a refusal — never a guess — for a blob that is damaged or that says it
//! holds a different number of strokes than it does.

use note_format::chunk::{
    chunk_ranges, decode, encode, open_sealed, seal, Codec, CHUNK_MAX_STROKES,
};
use note_format::ink::{InkPoint, Stroke};

mod common;

use common::stroke;

/// A page's worth of ink, three strokes wide.
fn page() -> Vec<Stroke> {
    vec![
        stroke(40, 10.0, 20.0, 0x1B_1B_1F),
        stroke(12, 30.0, 60.0, 0xCC_22_22),
        stroke(120, 5.0, 100.0, 0x22_66_CC),
    ]
}

/// The ink that goes into a chunk is the ink that comes out of it.
///
/// Positions and widths are quantised to 1/64 of a pixel, which is below what any digitizer reports —
/// and every coordinate here is already on that lattice, so "the same ink" is exact rather than close.
#[test]
fn a_page_of_ink_round_trips_through_its_blob() {
    let written = page();
    let encoded = encode(&written).expect("a page as one blob");

    let read = decode(
        &encoded.data,
        encoded.raw_len as usize,
        encoded.codec,
        written.len(),
        encoded.crc32,
    )
    .expect("the page back");

    assert_eq!(read.len(), written.len());

    for (index, (before, after)) in written.iter().zip(&read).enumerate() {
        assert_eq!(after.color, before.color, "stroke {index}'s colour");
        assert_eq!(after.points, before.points, "stroke {index}'s points");
    }

    assert!(
        encoded.data.len() * 3 < encoded.raw_len as usize,
        "a page of handwriting compresses to a fraction of its arrays"
    );
}

/// Encoding what was just decoded gives the same bytes — the encoding is a function of the ink.
///
/// Fixed point is a lattice, so a second pass lands on the same lattice points. This is what makes the
/// quantisation safe to rely on: a note does not drift a little every time it is written.
#[test]
fn re_encoding_what_was_decoded_gives_the_same_bytes() {
    let written = page();
    let first = encode(&written).expect("a page as one blob");

    let read = decode(
        &first.data,
        first.raw_len as usize,
        first.codec,
        written.len(),
        first.crc32,
    )
    .expect("the page back");

    let again = encode(&read).expect("the page written again");

    assert_eq!(again.codec, first.codec);
    assert_eq!(again.raw_len, first.raw_len);
    assert_eq!(again.data, first.data, "the same ink, the same bytes");
}

/// A chunk of no strokes is refused: it is indistinguishable from a damaged one.
#[test]
fn an_empty_chunk_is_refused() {
    let nothing: [Stroke; 0] = [];
    assert!(encode(&nothing).is_err());
}

/// The compressor is recorded per chunk, and a codec this build does not know is an error.
///
/// The id is what the `codec` column holds, so it is part of the format: an unknown one means the blob
/// was written by a build that knows a compressor this one does not, and decoding it as something else
/// would produce a page of noise rather than a message.
#[test]
fn the_compressor_is_recorded_by_its_own_id() {
    for codec in [Codec::Raw, Codec::Lz4, Codec::Zstd] {
        assert_eq!(Codec::from_id(codec.id()).expect("its own id"), codec);
    }

    assert!(Codec::from_id(3).is_err());
    assert!(Codec::from_id(255).is_err());
}

/// A sealed blob round trips, and a damaged one is refused rather than decompressed.
///
/// The seal is a nine-byte header — the codec, the length of the arrays, and a CRC32 of the compressed
/// bytes — and it is checked *before* the decompressor sees anything. A damaged page has to be
/// reported, because the alternative is a page of noise written back over good ink.
#[test]
fn a_sealed_blob_round_trips_and_damage_is_caught() {
    let written = page();
    let encoded = encode(&written).expect("a page as one blob");
    let mut blob = seal(&encoded);

    let read = open_sealed(&blob, written.len()).expect("the page back");
    assert_eq!(read.len(), written.len());
    assert_eq!(read[0].points, written[0].points);

    // One byte of the compressed arrays changed — a disk that flipped a bit.
    let last = blob.len() - 1;
    blob[last] ^= 0xFF;

    assert!(
        open_sealed(&blob, written.len()).is_err(),
        "a damaged blob was decoded anyway"
    );

    // And a blob that is shorter than its own header.
    assert!(open_sealed(&[0, 1, 2], written.len()).is_err());
}

/// A blob that says it holds a different number of strokes than it does is refused.
///
/// The counts are *in* the blob and the row's `stroke_count` is beside it, so the two have to agree: a
/// mismatch is exactly the case where guessing would write a corrupted page back over a good one.
#[test]
fn a_blob_is_refused_when_its_counts_do_not_agree() {
    let written = page();
    let encoded = encode(&written).expect("a page as one blob");

    let with_a_missing_stroke = decode(
        &encoded.data,
        encoded.raw_len as usize,
        encoded.codec,
        written.len() + 1,
        encoded.crc32,
    );
    assert!(with_a_missing_stroke.is_err());

    let with_one_too_many = decode(
        &encoded.data,
        encoded.raw_len as usize,
        encoded.codec,
        written.len() - 1,
        encoded.crc32,
    );
    assert!(with_one_too_many.is_err());

    // A checksum that does not match the bytes.
    let wrong_crc = decode(
        &encoded.data,
        encoded.raw_len as usize,
        encoded.codec,
        written.len(),
        encoded.crc32 ^ 1,
    );
    assert!(wrong_crc.is_err());
}

/// A page is cut into chunks that cover it exactly, none larger than a chunk may be.
///
/// A page is a handful of independent blobs rather than one: it keeps a single chunk's rewrite small
/// and lets the read path decompress them in parallel. The ranges have to partition the page — no gap
/// and no overlap — or a stroke would be written twice or lost.
#[test]
fn a_page_is_cut_into_chunks_that_cover_it_exactly() {
    let written: Vec<Stroke> = (0..1200)
        .map(|index| stroke(3, (index % 60) as f32 * 12.0, 10.0, 0x1B_1B_1F))
        .collect();

    let ranges = chunk_ranges(&written);
    assert!(ranges.len() > 1, "a page this long is more than one chunk");

    let mut expected_start = 0;
    for range in &ranges {
        assert_eq!(range.start, expected_start, "the chunks are contiguous");
        assert!(range.end > range.start, "a chunk holds at least one stroke");
        assert!(
            range.end - range.start <= CHUNK_MAX_STROKES,
            "a chunk holds {} strokes",
            range.end - range.start
        );
        expected_start = range.end;
    }

    assert_eq!(expected_start, written.len(), "and they cover the page");

    // Every chunk of the page is itself a blob that round trips.
    for range in ranges {
        let slice = &written[range.clone()];
        let encoded = encode(slice).expect("a chunk");

        assert_eq!(
            decode(
                &encoded.data,
                encoded.raw_len as usize,
                encoded.codec,
                slice.len(),
                encoded.crc32,
            )
            .expect("the chunk back")
            .len(),
            slice.len()
        );
    }
}

/// A chunk holds at most 255 distinct colours, because the palette index is one byte.
#[test]
fn a_chunk_of_many_colours_ends_at_the_palette() {
    let written: Vec<Stroke> = (0..300u32)
        .map(|index| stroke(2, index as f32 * 3.0, 10.0, 0x01_00_00 + index))
        .collect();

    let ranges = chunk_ranges(&written);

    assert_eq!(ranges.len(), 2, "300 colours is two palettes");
    assert_eq!(ranges[0].end - ranges[0].start, 255);
    assert_eq!(ranges[1].end - ranges[1].start, 45);
}

/// A quantised point is a point: the blob is in 1/64 of a pixel, and that is finer than a pen reports.
#[test]
fn the_blob_is_quantised_finer_than_a_pen_reports() {
    let quantum = note_format::chunk::QUANTUM;
    assert_eq!(quantum, 64.0);

    // A point between two lattice positions comes back on the nearer one.
    let mut off_lattice = Stroke::new(InkPoint::new(10.0, 10.0, 2.0), 0x1B_1B_1F);
    off_lattice
        .points
        .push(InkPoint::new(10.007_812_5, 10.0, 2.0));
    off_lattice.close();

    let encoded = encode(&[off_lattice.clone()]).expect("a stroke as one blob");
    let read = decode(
        &encoded.data,
        encoded.raw_len as usize,
        encoded.codec,
        1,
        encoded.crc32,
    )
    .expect("the stroke back");

    let delta = (read[0].points[1].x - off_lattice.points[1].x).abs();
    assert!(delta <= 1.0 / quantum, "{delta} pixels of drift");
}
