//! Case A at the model boundary. No window is needed. Timings stop at model commit.
//! cargo test --release -p note-ui --lib long_note_cpu_bench -- --ignored --nocapture
//! Set CHEAP_NOTE_HIDDEN_STROKES to 0, 10000, 100000 or 1000000 per process.

use crate::ink::{InkDocument, InkTransform, Tool};
use crate::input::{Phase, Point, Reading};
use note_format::ink::{InkPoint, Stroke};
use note_format::settings::Settings;
use std::time::Instant;

fn fixture(hidden: usize) -> InkDocument {
    let strokes = (0..1000 + hidden)
        .map(|index| {
            let origin = if index < 1000 { 0.0 } else { 8000.0 };
            let x = origin + 10.0 + (index % 50) as f32 * 10.0;
            let y = 30.0 + (index / 50 % 100) as f32 * 5.0;
            let mut stroke = Stroke::new(InkPoint::new(x, y, 2.0), Stroke::DEFAULT_COLOR);
            for step in 1..24 {
                stroke.points.push(InkPoint::new(x + step as f32, y, 2.0));
            }
            stroke
        })
        .collect();
    InkDocument::from_strokes(strokes)
}

fn sample(phase: Phase, x: f32) -> Reading {
    Reading {
        phase,
        id: 1,
        pixel: Point::new(x, 700.0),
        pressure: Some(0.5),
        ..Reading::default()
    }
}

fn edit_and_restore(doc: &mut InkDocument, count: usize, config: &Settings) -> (u128, u128) {
    let view = InkTransform::identity();
    doc.set_mode(Tool::Pen);
    doc.consume(
        &[sample(Phase::Down, 40.0), sample(Phase::Move, 50.0)],
        &view,
        config,
    );
    let up = sample(Phase::Up, 60.0);
    let start = Instant::now();
    doc.consume(&[up], &view, config);
    let append_ns = start.elapsed().as_nanos();
    assert_eq!(doc.finished().len(), count + 1);
    let appended = doc.finished()[count].clone();
    assert!(doc.undo());
    assert_eq!(doc.finished().len(), count);
    assert!(doc.redo());
    assert_eq!(*doc.finished()[count], *appended);
    assert!(doc.undo());

    doc.set_mode(Tool::Eraser);
    let erase = sample(Phase::Down, 300.0);
    let start = Instant::now();
    doc.consume(&[erase], &view, config);
    let erase_ns = start.elapsed().as_nanos();
    doc.consume(&[sample(Phase::Up, 300.0)], &view, config);
    assert_eq!(doc.finished().len(), count);
    (append_ns, erase_ns)
}

#[test]
fn long_note_edit_round_trip_preserves_existing_strokes() {
    let mut doc = fixture(100);
    let before = doc.finished().as_ref().clone();
    edit_and_restore(&mut doc, before.len(), &Settings::default());
    assert_eq!(doc.finished().as_slice(), before.as_slice());
}

#[test]
#[ignore = "release benchmark; one fixture per process"]
fn long_note_cpu_bench() {
    let hidden = std::env::var("CHEAP_NOTE_HIDDEN_STROKES")
        .unwrap_or_else(|_| "0".into())
        .parse::<usize>()
        .expect("integer fixture size");
    assert!([0, 10_000, 100_000, 1_000_000].contains(&hidden));
    let mut doc = fixture(hidden);
    let config = Settings::default();
    let count = doc.finished().len();
    for _ in 0..100 {
        edit_and_restore(&mut doc, count, &config);
    }
    let rows: Vec<_> = (0..1000)
        .map(|_| edit_and_restore(&mut doc, count, &config))
        .collect();
    println!("hidden_strokes,sample,model_pen_up_ns,model_blank_erase_ns");
    for (i, (append, erase)) in rows.into_iter().enumerate() {
        println!("{hidden},{i},{append},{erase}");
    }
}
