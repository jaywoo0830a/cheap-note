//! A page's ink as one compressed blob: the encoding behind `chunks.data`.
//!
//! ## Why not JSON
//!
//! The first version of the note file wrote every point as JSON, and JSON spends more bytes on the
//! *punctuation* of a number than on the number. A point is `{"x":412.5,"y":118.25,"width":2.5}` —
//! thirty-two bytes, of which perhaps eight are the values. At a few hundred thousand points a page
//! that is megabytes of text to parse on the way in and write on the way out, and the parse is done
//! with the pen waiting.
//!
//! So a chunk is a **structure of arrays** in fixed point, with the three streams separated:
//!
//! ```text
//! [total points: varint] [strokes: varint] [palette: count + colours]
//! [per stroke: colour index, point count]
//! [x: per stroke: i32 base, then deltas]     <- the three arrays
//! [y: per stroke: i32 base, then deltas]
//! [w: per stroke: i32 base, then deltas]
//! ```
//!
//! ## Why that is small
//!
//! * **The values are deltas.** A resampled stroke is a walk of a pixel or two per step, so the
//!   difference from the previous point is the small number, and the first point of a stroke — the
//!   only large one — is stored once as an exact integer rather than as a varint that would need
//!   several bytes.
//! * **They are quantised.** Coordinates and widths are fixed point at [`QUANTUM`] units per logical
//!   pixel: 1/64 of a pixel, which is finer than any digitizer reports and finer than one pixel at
//!   400% zoom, and it turns a zigzag varint into one or two bytes where an `f32` is four.
//! * **Colours are a palette.** A page is written in one colour and a note holds a handful, so the
//!   colours are a table at the head of the chunk and each stroke stores its index.
//! * **The whole thing is compressed.** `zstd` level 3 on the arrays above; see [`encode`] for the
//!   codec fallbacks, and the `codec` column in the store for why the choice is recorded per chunk.
//!
//! ## What a chunk is *not*
//!
//! It is not self-describing beyond its own counts: the page and the sequence number are columns in
//! the `chunks` table, and the CRC32 that seals the blob is a column too. A chunk is decoded by
//! handing it those values back — see [`decode`] — and a mismatch is reported rather than guessed
//! at, because a blob that decodes into the wrong number of strokes is exactly the case where
//! guessing writes a corrupted page back over a good one.
//!
//! ## Checking the encoding against something that is not this code
//!
//! The compression is verified by size (a chunk is much smaller than its own arrays) and the format
//! by a round trip, including one that re-encodes what it decoded: fixed point is a lattice, so a
//! second pass lands on the same lattice points and produces the identical bytes. That test is what
//! makes the quantisation safe to rely on — it cannot drift on every save.

use std::ops::Range;

use crate::error::{AppError, Result};
use crate::ink::{InkPoint, Stroke};

/// Fixed-point units per logical pixel.
///
/// 1/64 of a pixel: below what a digitizer reports, below what one pixel of a 400% zoom shows, and
/// small enough that a step of up to two pixels is a single varint byte.
pub const QUANTUM: f32 = 64.0;

/// The largest raw chunk written, before compression.
///
/// A page is a handful of these rather than one blob. The number is the one the design arrives at
/// from both ends: big enough that SQLite's pages and zstd's window are used well, small enough that
/// opening a page decompresses in a few milliseconds and that rewriting one chunk is a small write.
pub const CHUNK_RAW_TARGET: usize = 64 * 1024;

/// The most strokes a chunk holds, whichever comes first.
///
/// The palette index is one byte, so a chunk cannot describe more colours than that either; and a
/// chunk that held thousands of strokes would be a blob whose *per-stroke* tables are most of it.
pub const CHUNK_MAX_STROKES: usize = 512;

/// The most distinct colours one chunk can describe.
const MAX_COLORS: usize = u8::MAX as usize;
/// The compressor a chunk was written with.
///
/// Recorded per chunk in the store's `codec` column, because the right one is a property of the ink
/// rather than of the app: a chunk of one stroke compresses to nothing, and a compressor's own
/// header would be the larger part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Not compressed: the arrays as they were built.
    Raw,
    /// `lz4_flex`, which is fast at both ends and has no header worth mentioning.
    Lz4,
    /// `zstd` at level 3, which is what a page of handwriting actually uses.
    Zstd,
}

impl Codec {
    /// The value that goes in the `codec` column.
    pub fn id(self) -> u32 {
        match self {
            Codec::Raw => 0,
            Codec::Lz4 => 1,
            Codec::Zstd => 2,
        }
    }

    /// The codec a chunk was written with, from its column value.
    ///
    /// An unknown id is an error rather than a fallback: it means the file was written by a build
    /// that knows a compressor this one does not, and decoding those bytes as something else would
    /// produce a page of noise rather than a message.
    pub fn from_id(id: u32) -> Result<Self> {
        match id {
            0 => Ok(Codec::Raw),
            1 => Ok(Codec::Lz4),
            2 => Ok(Codec::Zstd),
            other => Err(AppError::Note(format!(
                "this note uses compression {other}, which this build does not know"
            ))),
        }
    }
}

/// One encoded chunk, ready for the `chunks` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// Which compressor was used, and therefore what `codec` says.
    pub codec: Codec,
    /// The length of the arrays before compression, so decoding can size its buffers exactly.
    pub raw_len: u32,
    /// The compressed arrays.
    pub data: Vec<u8>,
    /// CRC32 of `data`, so a damaged blob is caught before it is decompressed.
    pub crc32: u32,
}
/// The arrays of one page, encoded and compressed, with the compressor that was chosen.
///
/// The choice is made by *measuring*: the arrays are built first, then each codec is given a chance
/// in order — zstd, then lz4, then nothing — and the first one that makes the blob smaller than the
/// arrays wins. That order is deliberate rather than fixed, because a small chunk is smaller than
/// any compressor's header, and a store that wrote the compressor anyway would spend bytes to slow
/// the read path down.
///
/// An empty slice is refused rather than encoded: `chunks.data` never holds a blob with nothing in
/// it, and a chunk that decodes to no strokes is indistinguishable from a damaged one.
pub fn encode(strokes: &[Stroke]) -> Result<Encoded> {
    if strokes.is_empty() {
        return Err(AppError::Note(String::from(
            "a chunk of no strokes is not written",
        )));
    }

    let raw = arrays(strokes);

    let zstd = zstd::encode_all(&raw[..], ZSTD_LEVEL).ok();
    if let Some(data) = zstd.filter(|data| data.len() < raw.len()) {
        return Ok(sealed(Codec::Zstd, raw.len(), data));
    }

    let lz4 = lz4_flex::compress(&raw);
    if lz4.len() < raw.len() {
        return Ok(sealed(Codec::Lz4, raw.len(), lz4));
    }

    Ok(sealed(Codec::Raw, raw.len(), raw))
}

/// The strokes of one chunk, from the columns the `chunks` row holds.
///
/// Every one of the four arguments is a *stored* value rather than something read from the blob, and
/// all four are checked: the CRC catches a damaged file, the decompressed length catches a
/// truncated one, and the stroke count catches the case both of those miss — a blob that is intact
/// and is not the chunk it is filed under. That last one is why `stroke_count` is a column and not a
/// nicety: on a mismatch this returns an error instead of a page that would be written back over the
/// good one.
pub fn decode(
    data: &[u8],
    raw_len: usize,
    codec: Codec,
    stroke_count: usize,
    crc32: u32,
) -> Result<Vec<Stroke>> {
    if crc32fast::hash(data) != crc32 {
        return Err(AppError::Note(String::from(
            "a chunk of this note is damaged (its checksum does not match)",
        )));
    }

    let raw = match codec {
        Codec::Raw => data.to_vec(),
        Codec::Lz4 => lz4_flex::decompress(data, raw_len).map_err(|error| {
            AppError::Note(format!("a chunk of this note could not be unpacked: {error}"))
        })?,
        Codec::Zstd => zstd::bulk::decompress(data, raw_len).map_err(|error| {
            AppError::Note(format!("a chunk of this note could not be unpacked: {error}"))
        })?,
    };

    if raw.len() != raw_len {
        return Err(AppError::Note(format!(
            "a chunk of this note unpacks to {} bytes, not the {raw_len} it was written as",
            raw.len()
        )));
    }

    let strokes = read_arrays(&raw)?;
    if strokes.len() != stroke_count {
        return Err(AppError::Note(format!(
            "a chunk of this note holds {} strokes, not the {stroke_count} it is filed under",
            strokes.len()
        )));
    }

    Ok(strokes)
}
/// Which slices of `strokes` make one chunk each.
///
/// Chunking happens on the *writer's* side and is measured rather than counted, because the arrays
/// are what has a size: a stroke of three points and a stroke of a thousand are the same row in the
/// database and a very different number of bytes. A chunk therefore ends at whichever comes first —
/// [`CHUNK_RAW_TARGET`] of estimated arrays, [`CHUNK_MAX_STROKES`] strokes, or the palette filling
/// up, which is what keeps the colour index one byte.
///
/// The ranges tile the slice exactly and in order: every stroke of the page is in exactly one chunk,
/// and the chunks are written in the order they are returned, which is where `seq` and `stroke_start`
/// come from.
pub fn chunk_ranges(strokes: &[Stroke]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut raw = 0;
    let mut colors: Vec<u32> = Vec::new();

    for (index, stroke) in strokes.iter().enumerate() {
        let size = estimated_raw(stroke);
        let full = raw + size > CHUNK_RAW_TARGET
            || index - start >= CHUNK_MAX_STROKES
            || (colors.len() >= MAX_COLORS && !colors.contains(&stroke.color));

        if full && index > start {
            ranges.push(start..index);
            start = index;
            raw = 0;
            colors.clear();
        }

        raw += size;
        if !colors.contains(&stroke.color) {
            colors.push(stroke.color);
        }
    }

    if start < strokes.len() {
        ranges.push(start..strokes.len());
    }

    ranges
}

/// What one stroke is expected to cost as arrays, for the chunking decision.
///
/// Counted rather than measured: the alternative is encoding the whole page to find out where the
/// chunks are and then encoding it again in pieces, which doubles the work to sharpen a boundary
/// that only has to be *about* right.
fn estimated_raw(stroke: &Stroke) -> usize {
    // Three bases, the colour index and the count, then two bytes of values per point: an estimated
    // byte per coordinate and width, which is what a delta of a resampled stroke costs.
    let points = stroke.points.len();
    let tables = 3 * std::mem::size_of::<i32>() + 2;
    tables + points * 3 * 2
}
/// The lines of one page, split into the three arrays and the tables that describe them.
fn arrays(strokes: &[Stroke]) -> Vec<u8> {
    let points: usize = strokes.iter().map(|stroke| stroke.points.len()).sum();
    let mut colors: Vec<u32> = Vec::new();
    for stroke in strokes {
        if !colors.contains(&stroke.color) {
            colors.push(stroke.color);
        }
    }

    let mut out = Vec::with_capacity(estimated_raw_of(strokes));
    put_varint(&mut out, points as u64);
    put_varint(&mut out, strokes.len() as u64);

    put_varint(&mut out, colors.len() as u64);
    for color in &colors {
        put_varint(&mut out, u64::from(*color));
    }

    for stroke in strokes {
        // The palette cannot overflow: `chunk_ranges` ends a chunk before it would, and this writer
        // is the only thing that writes chunks.
        let index = colors
            .iter()
            .position(|color| *color == stroke.color)
            .unwrap_or(0);
        out.push(index as u8);
        put_varint(&mut out, stroke.points.len() as u64);
    }

    for axis in 0..3 {
        for stroke in strokes {
            put_stream(&mut out, stroke, axis);
        }
    }

    out
}

/// One stroke's values of one axis: the first as an exact integer, the rest as deltas from it.
fn put_stream(out: &mut Vec<u8>, stroke: &Stroke, axis: usize) {
    let mut previous = 0i32;

    for (index, point) in stroke.points.iter().enumerate() {
        let value = quantised(point, axis);
        if index == 0 {
            out.extend_from_slice(&value.to_le_bytes());
            previous = value;
        } else {
            put_varint(out, zigzag(value.wrapping_sub(previous)));
            previous = value;
        }
    }
}

/// The strokes the arrays in `raw` describe.
fn read_arrays(raw: &[u8]) -> Result<Vec<Stroke>> {
    let mut cursor = Cursor::new(raw);

    let points = cursor.varint()? as usize;
    let count = cursor.varint()? as usize;

    let colors = cursor.varint()? as usize;
    if colors > MAX_COLORS {
        return Err(AppError::Note(String::from(
            "a chunk of this note names more colours than a chunk can hold",
        )));
    }
    let mut palette = Vec::with_capacity(colors);
    for _ in 0..colors {
        palette.push(cursor.varint()? as u32);
    }

    // Both counts come from the blob, so neither is trusted with an allocation before the bytes that
    // would have to hold them are known to be there: a damaged header must not ask for a gigabyte.
    if count > raw.len() || points > raw.len() {
        return Err(AppError::Note(String::from(
            "a chunk of this note claims more ink than its own bytes could hold",
        )));
    }

    let mut headers: Vec<(u32, usize)> = Vec::with_capacity(count);
    for _ in 0..count {
        let index = usize::from(cursor.byte()?);
        let color = *palette.get(index).ok_or_else(|| {
            AppError::Note(String::from(
                "a chunk of this note refers to a colour it does not hold",
            ))
        })?;
        headers.push((color, cursor.varint()? as usize));
    }

    let mut ink: Vec<Vec<InkPoint>> = Vec::with_capacity(headers.len());
    for (_, count) in &headers {
        ink.push(vec![InkPoint::new(0.0, 0.0, 0.0); *count]);
    }

    for axis in 0..3 {
        for (stroke, (_, count)) in ink.iter_mut().zip(headers.iter()) {
            if *count == 0 {
                continue;
            }

            let mut previous = cursor.i32_le()?;
            set(stroke, 0, axis, previous);

            for index in 1..*count {
                previous = previous.wrapping_add(unzigzag(cursor.varint()?));
                set(stroke, index, axis, previous);
            }
        }
    }

    if cursor.remaining() != 0 {
        return Err(AppError::Note(format!(
            "a chunk of this note has {} bytes left over",
            cursor.remaining()
        )));
    }

    Ok(headers
        .into_iter()
        .zip(ink)
        .map(|((color, _), points)| {
            let mut stroke = Stroke {
                points,
                color,
                outline: Vec::new(),
                bounds: [0.0; 4],
            };
            // The bounds and the outline are not stored — they are derivable from the points — but
            // the eraser hit-tests against them and a frame culls by them, so a stroke without them
            // would draw and refuse to be erased.
            stroke.close();
            stroke
        })
        .collect())
}
/// Puts one axis's value into a point of a stroke being rebuilt.
fn set(points: &mut [InkPoint], index: usize, axis: usize, value: i32) {
    let value = unquantised(value);
    let point = &mut points[index];
    match axis {
        0 => point.x = value,
        1 => point.y = value,
        _ => point.width = value,
    }
}

/// What every stroke of a page is expected to cost, for the initial capacity.
fn estimated_raw_of(strokes: &[Stroke]) -> usize {
    strokes
        .iter()
        .map(estimated_raw)
        .sum::<usize>()
        .saturating_add(64)
}

/// One value of a point, in fixed point.
fn quantised(point: &InkPoint, axis: usize) -> i32 {
    let value = match axis {
        0 => point.x,
        1 => point.y,
        _ => point.width,
    };

    (value * QUANTUM)
        .round()
        .clamp(i32::MIN as f32 + 1.0, i32::MAX as f32) as i32
}

/// A fixed-point value back in logical pixels.
fn unquantised(value: i32) -> f32 {
    value as f32 / QUANTUM
}

/// The compressed blob plus the three columns that describe it.
fn sealed(codec: Codec, raw_len: usize, data: Vec<u8>) -> Encoded {
    Encoded {
        codec,
        raw_len: raw_len as u32,
        crc32: crc32fast::hash(&data),
        data,
    }
}

/// `zstd` level: the one the design settles on, where a page of handwriting is most of the way to
/// its compressed size and the compressor's own time is still a rounding error against a frame.
const ZSTD_LEVEL: i32 = 3;

/// A little-endian varint: seven bits a byte, the top bit marking "there is more".
fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A signed value as a varint: small magnitudes stay small, whether they are negative or positive.
fn zigzag(value: i32) -> u64 {
    ((value << 1) ^ (value >> 31)) as u32 as u64
}

/// A zigzag varint back as the signed value it was.
fn unzigzag(value: u64) -> i32 {
    ((value >> 1) as i32) ^ -((value & 1) as i32)
}

/// A reader over the raw arrays, which reports a short chunk instead of panicking on one.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = *self.bytes.get(self.at).ok_or_else(short)?;
        self.at += 1;
        Ok(byte)
    }

    fn i32_le(&mut self) -> Result<i32> {
        let mut bytes = [0u8; 4];
        for byte in bytes.iter_mut() {
            *byte = self.byte()?;
        }
        Ok(i32::from_le_bytes(bytes))
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;

        for shift in (0..64u32).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }

        Err(AppError::Note(String::from(
            "a chunk of this note holds a number that is not a number",
        )))
    }
}

/// The error a chunk raises when it ends in the middle of a value.
fn short() -> AppError {
    AppError::Note(String::from(
        "a chunk of this note ends in the middle of a value",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stroke that walks across the page, one point every 1.5 px, as a resampled pen leaves.
    fn stroke(points: usize, from: (f32, f32), color: u32) -> Stroke {
        let mut stroke = Stroke::new(InkPoint::new(from.0, from.1, 1.25), color);

        for step in 1..points {
            stroke.points.push(InkPoint::new(
                from.0 + step as f32 * 1.5,
                from.1 + (step % 7) as f32 * 0.75,
                1.25 + (step % 5) as f32 * 0.1,
            ));
        }

        stroke.close();
        stroke
    }

    /// A page of `strokes` strokes, each with a realistic number of points.
    fn page(strokes: usize) -> Vec<Stroke> {
        (0..strokes)
            .map(|index| {
                stroke(
                    48 + index % 17,
                    (12.0 + index as f32 * 3.0, 20.0 + index as f32 * 2.0),
                    Stroke::DEFAULT_COLOR,
                )
            })
            .collect()
    }

    /// What was written comes back: same strokes, same colours, same points to the quantum.
    #[test]
    fn a_chunk_round_trips() {
        let strokes = vec![
            stroke(40, (10.5, 20.25), 0x00_11_22),
            stroke(9, (300.0, 4.0), 0xAA_BB_CC),
        ];
        let encoded = encode(&strokes).expect("the page encodes");

        let decoded = decode(
            &encoded.data,
            encoded.raw_len as usize,
            encoded.codec,
            strokes.len(),
            encoded.crc32,
        )
        .expect("the page decodes");

        assert_eq!(decoded.len(), strokes.len());
        for (before, after) in strokes.iter().zip(&decoded) {
            assert_eq!(before.color, after.color);
            assert_eq!(before.points.len(), after.points.len());

            for (before, after) in before.points.iter().zip(&after.points) {
                for (before, after) in [
                    (before.x, after.x),
                    (before.y, after.y),
                    (before.width, after.width),
                ] {
                    assert!(
                        (before - after).abs() <= 0.5 / QUANTUM,
                        "{before} and {after} are the same value to the quantum"
                    );
                }
            }
        }
    }

    /// A single-stroke blob is usually stored *uncompressed*: a compressor's header is bigger than
    /// the stroke.
    ///
    /// This is why the codec is measured rather than chosen once for the whole build. A dirty row is
    /// one stroke — usually 20-80 points, 100-300 bytes — and at that size zstd earns nothing
    /// (measured: raw wins at 20, 45 and 70 points, and zstd only takes over somewhere past a
    /// hundred). It is the `chunks` rows, which are a page at a time, that compress.
    #[test]
    fn a_single_stroke_blob_usually_skips_the_compressor() {
        let short = encode(&[hand_stroke(11, 45)]).expect("the stroke encodes");
        assert_eq!(
            short.codec,
            Codec::Raw,
            "{} bytes of one stroke were left as they were",
            short.raw_len
        );

        let long = encode(&[hand_stroke(11, 120)]).expect("the stroke encodes");
        assert_eq!(long.codec, Codec::Zstd, "a long stroke is worth compressing");
    }

    /// A hand-written page is many times smaller than the JSON it used to be.
    ///
    /// The fixture is deliberately *irregular* — 1-2 px steps, a slowly turning heading, a different
    /// shape for every stroke — because a page of identical strokes compresses to almost nothing and
    /// would flatter the format. These are the numbers `doc/STORE.md` quotes, and this test is what
    /// keeps them true:
    ///
    /// ```text
    ///  10 strokes   445 points    1.9 KB of arrays   1.7 KB of chunk    21 KB of JSON
    /// 100 strokes  5.4 KB points 23.2 KB of arrays  18.9 KB of chunk   258 KB of JSON
    /// 1000 strokes 54.9 KB points 235 KB of arrays  184 KB of chunk   2.6 MB of JSON
    /// ```
    #[test]
    fn a_hand_written_page_is_much_smaller_than_json() {
        let strokes: Vec<Stroke> = (0..200)
            .map(|index| hand_stroke(index as u64 + 7, 40 + index % 31))
            .collect();

        let raw = arrays(&strokes);
        let encoded = encode(&strokes).expect("the page encodes");
        let json = serde_json::to_vec(&strokes).expect("the strokes serialise");

        assert!(
            encoded.data.len() * 8 < json.len(),
            "{} bytes of chunk against {} bytes of JSON",
            encoded.data.len(),
            json.len()
        );
        assert!(
            encoded.data.len() < raw.len(),
            "and the compressor still earns its header"
        );
    }

    /// A stroke that walks like a hand: small steps, a heading that turns slowly, and a shape of its
    /// own.
    fn hand_stroke(seed: u64, points: usize) -> Stroke {
        let mut state = seed.wrapping_mul(2_654_435_761) | 1;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32
        };

        let mut heading = random() * std::f32::consts::TAU;
        let mut x = random() * 640.0;
        let mut y = random() * 860.0;
        let mut stroke = Stroke::new(
            InkPoint::new(x, y, 1.25 + random() * 1.5),
            Stroke::DEFAULT_COLOR,
        );

        for _ in 1..points {
            heading += (random() - 0.5) * 0.35;
            let step = 1.0 + random();
            x += heading.cos() * step;
            y += heading.sin() * step;
            stroke.points.push(InkPoint::new(x, y, 1.25 + random() * 1.5));
        }

        stroke.close();
        stroke
    }

    /// A dot survives: one point is a stroke, and it is the case a delta-only format gets wrong.
    #[test]
    fn a_single_point_stroke_round_trips() {
        let strokes = vec![stroke(1, (77.5, 91.0), 0x00_00_00)];
        let encoded = encode(&strokes).expect("the dot encodes");

        let decoded = decode(
            &encoded.data,
            encoded.raw_len as usize,
            encoded.codec,
            strokes.len(),
            encoded.crc32,
        )
        .expect("the dot decodes");

        assert_eq!(decoded[0].points.len(), 1);
        assert!((decoded[0].points[0].x - 77.5).abs() < 0.02);
        assert!(decoded[0].bounds[2] > 77.0, "and it knows where it is");
    }

    /// The point of the format: a page of handwriting costs a fraction of its own arrays.
    #[test]
    fn the_codec_shrinks_what_it_is_given() {
        let strokes = page(200);
        let encoded = encode(&strokes).expect("the page encodes");

        assert_eq!(encoded.codec, Codec::Zstd, "a page is worth compressing");
        assert!(
            encoded.data.len() * 4 < encoded.raw_len as usize,
            "{} bytes of arrays became {} bytes of chunk",
            encoded.raw_len,
            encoded.data.len()
        );
    }

    /// A damaged blob is caught by its checksum rather than decompressed into nonsense.
    #[test]
    fn a_damaged_chunk_is_refused() {
        let strokes = page(3);
        let mut encoded = encode(&strokes).expect("the page encodes");
        encoded.data[2] ^= 0x40;

        let error = decode(
            &encoded.data,
            encoded.raw_len as usize,
            encoded.codec,
            strokes.len(),
            encoded.crc32,
        )
        .expect_err("a damaged chunk is refused");

        assert!(error.to_string().contains("damaged"), "{error}");
    }

    /// A chunk that is intact but is not the one it is filed under is refused too: the stroke count
    /// is a column exactly so that this case can be told apart from a good read.
    #[test]
    fn a_chunk_filed_under_the_wrong_count_is_refused() {
        let strokes = page(4);
        let encoded = encode(&strokes).expect("the page encodes");

        let error = decode(
            &encoded.data,
            encoded.raw_len as usize,
            encoded.codec,
            strokes.len() + 1,
            encoded.crc32,
        )
        .expect_err("the wrong count is refused");

        assert!(error.to_string().contains("not the 5"), "{error}");
    }

    /// Nothing is written for nothing, and a compressor this build does not know is named rather
    /// than guessed at.
    #[test]
    fn what_cannot_be_read_is_refused_by_name() {
        assert!(encode(&[]).is_err());

        let error = Codec::from_id(9).expect_err("an unknown codec is refused");
        assert!(error.to_string().contains('9'), "{error}");
    }

    /// The chunks of a page tile it: every stroke in exactly one chunk, in order, each chunk within
    /// the size and stroke bounds.
    #[test]
    fn a_page_is_split_into_chunks_that_tile_it() {
        let strokes = page(1200);
        let ranges = chunk_ranges(&strokes);

        assert!(ranges.len() > 1, "a page of this size is several chunks");
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[ranges.len() - 1].end, strokes.len());

        let mut next = 0;
        for range in &ranges {
            assert_eq!(range.start, next, "the chunks follow one another");
            next = range.end;

            assert!(
                range.len() <= CHUNK_MAX_STROKES,
                "a chunk holds at most {CHUNK_MAX_STROKES} strokes"
            );

            let encoded = encode(&strokes[range.clone()]).expect("the chunk encodes");
            let decoded = decode(
                &encoded.data,
                encoded.raw_len as usize,
                encoded.codec,
                range.len(),
                encoded.crc32,
            )
            .expect("the chunk decodes");
            assert_eq!(decoded.len(), range.len());
        }
    }

    /// A page written in hundreds of colours is split so that each chunk's palette fits in one byte.
    #[test]
    fn the_palette_ends_a_chunk_before_it_overflows() {
        let strokes: Vec<Stroke> = (0..300)
            .map(|index| stroke(4, (index as f32, 0.0), 0x10_00_00 + index))
            .collect();

        let ranges = chunk_ranges(&strokes);
        assert!(ranges.len() > 1, "one byte of colour index cannot hold 300");

        for range in &ranges {
            let encoded = encode(&strokes[range.clone()]).expect("the chunk encodes");
            let decoded = decode(
                &encoded.data,
                encoded.raw_len as usize,
                encoded.codec,
                range.len(),
                encoded.crc32,
            )
            .expect("the chunk decodes");
            assert_eq!(decoded[0].color, 0x10_00_00 + range.start as u32);
        }
    }

    /// Fixed point is a lattice: decoding puts every value on it, so a note that is read and written
    /// again does not drift, and the second write is the same bytes as the first.
    #[test]
    fn re_encoding_what_was_decoded_gives_the_same_bytes() {
        let strokes = page(50);
        let first = encode(&strokes).expect("the page encodes");

        let decoded = decode(
            &first.data,
            first.raw_len as usize,
            first.codec,
            strokes.len(),
            first.crc32,
        )
        .expect("the page decodes");

        let again = encode(&decoded).expect("the page encodes again");
        assert_eq!(again.codec, first.codec);
        assert_eq!(again.data, first.data, "the second write is the first");
    }

    /// The arrays are smaller than the JSON they replace, by an order of magnitude, which is the
    /// whole reason the format changed.
    #[test]
    fn the_arrays_are_much_smaller_than_json() {
        let strokes = page(20);
        let raw = arrays(&strokes);

        let json = serde_json::to_vec(&strokes).expect("the strokes serialise");
        assert!(
            raw.len() * 5 < json.len(),
            "{} bytes of arrays against {} bytes of JSON",
            raw.len(),
            json.len()
        );
    }
}
