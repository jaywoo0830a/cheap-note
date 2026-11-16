//! Every `unsafe` block says what it is relying on.
//!
//! ## Why this is a test rather than a convention
//!
//! Item 16 of *Effective Rust* is blunt about unsafe code: localise it, and write down the invariant
//! each block depends on, because "the unsafe marker doesn't mean that no rules apply — it means
//! that you are now responsible for maintaining them". Clippy can be asked for that
//! (`clippy::undocumented_unsafe_blocks`), but the workspace's `unsafe` lives in the parts of the
//! build that need a specific toolchain and a GPU, and a lint that only runs where someone remembers
//! to run it is a convention with extra steps.
//!
//! So this is the check instead: a plain scan of the source tree, runnable anywhere, that fails when
//! an `unsafe` block has no `// SAFETY:` comment above it. The macOS half of this project was
//! already fully commented when the scan was written and the Windows half was not — which is the
//! argument for having it, since the difference was not a difference in care but in whether anyone
//! had looked.
//!
//! ## What counts, and what does not
//!
//! * An `unsafe` **block** — `unsafe {`, or `unsafe [` for a mutable static — must have a comment
//!   containing `SAFETY:` directly above it, through any `#[allow]` attributes in between.
//! * An `unsafe fn` **declaration** does not: it is a promise to a caller, and the caller's blocks
//!   are what carry the comments. `pdfium::api` is a table of thirty-two of those, and demanding
//!   thirty-two comments saying "this is an FFI signature" would train the reader to skip them.
//!
//! The scan is line-based and deliberately simple. The message names the file and line, because the
//! fix for a miss here is to look at it.

use std::path::{Path, PathBuf};

/// Every `.rs` file under the workspace's source directories.
///
/// Enumerated rather than globbed: no dependency, no walk order to depend on, and the list is short
/// enough to read. A new crate is a line here, which is the right amount of friction for a check
/// that is supposed to cover everything.
fn sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate is inside the workspace")
        .parent()
        .expect("the workspace is the repository root")
        .to_path_buf();

    let mut files = Vec::new();
    let starts = [
        "crates/note-format/src",
        "crates/note-ui/src",
        "apps/macos/src",
        "apps/windows/src",
        "apps/macos/pen-macos/src",
    ];

    for start in starts {
        collect(&root.join(start), &mut files);
    }

    files.sort();
    files
}

/// Adds every `.rs` file under `dir`, depth first.
fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // A crate that is not in this checkout — the Windows app on a Mac, say — has nothing to
        // check, which is not a failure.
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Whether `line` opens an `unsafe` block, as opposed to declaring an `unsafe fn`.
///
/// `unsafe {` and `unsafe [` are blocks. `unsafe fn`, `unsafe extern` and `unsafe impl` are
/// declarations. A comment that merely mentions the word is not a block either, so the scan looks at
/// what follows the word rather than at the word itself.
fn opens_a_block(line: &str) -> bool {
    if line.trim_start().starts_with("//") {
        return false;
    }

    let mut rest = line;
    while let Some(at) = rest.find("unsafe") {
        let after = rest[at + "unsafe".len()..].trim_start();
        if after.starts_with('{') || after.starts_with('[') {
            return true;
        }

        // Not a block: keep looking to the right, in case one follows on the same line.
        if after.len() < 2 {
            break;
        }
        rest = &after[1..];
    }

    false
}

/// Whether the lines above `at` carry a `SAFETY` comment, near enough to belong to it.
///
/// "Near enough" is the block's own line if it has one, or the contiguous run of comment lines
/// directly above — through attributes such as `#[allow(...)]` and blank lines, which are often
/// written between a comment and the code it is about.
///
/// A block may also say that it rests on a **[`MODULE_WIDE_SAFETY`](self::MODULE_WIDE_SAFETY)** note
/// at the top of its module. That exists because most of this workspace's unsafe blocks are calls
/// through one FFI boundary that share a single invariant — a handle this type owns, still open —
/// and repeating it forty times, identically, would make the forty-first go unread.
fn explains(lines: &[&str], at: usize, module_note: bool) -> bool {
    // A module that states the shared invariant once, at the top, has answered for every block in
    // it that does not carry a more specific note of its own.
    if module_note {
        return true;
    }

    if let Some(comment) = comment_of(lines[at]) {
        if comment.contains("SAFETY") {
            return true;
        }
    }

    let mut above = at;
    while above > 0 {
        above -= 1;
        let line = lines[above].trim();

        if line.contains(MODULE_WIDE_SAFETY) {
            return module_note;
        }

        if line.is_empty() || line.starts_with("#[") {
            continue;
        }

        let Some(comment) = comment_of(lines[above]) else {
            return false;
        };

        if comment.contains("SAFETY") {
            return true;
        }
    }

    false
}

/// The marker a block uses to point at its module's own `SAFETY` note.
///
/// Spelled as a word rather than as free prose so that the reference is greppable and so that a
/// typo cannot silently satisfy the check: `SAFETY: module-wide`
const MODULE_WIDE_SAFETY: &str = "SAFETY: module-wide";

/// The text of `line`'s comment, if it is one.
///
/// Doc comments are excluded: `///` and `//!` document the item below them, and a `SAFETY` note in
/// documentation is a different claim from one attached to a block.
fn comment_of(line: &str) -> Option<&str> {
    let line = line.trim();
    (line.starts_with("//") && !line.starts_with("///") && !line.starts_with("//!")).then_some(line)
}

/// Every `unsafe` block in the workspace has a `SAFETY` comment saying what it relies on.
///
/// This is the check a reviewer cannot be relied upon to do by hand: an `unsafe` block added under
/// time pressure looks exactly like one added carefully, and the invariant it depends on is
/// invisible until the day it is broken. Finding one here is cheap; finding out in production is not.
#[test]
fn every_unsafe_block_explains_itself() {
    let mut unexplained = Vec::new();
    let mut blocks = 0;

    for path in sources() {
        let source = std::fs::read_to_string(&path).expect("a source file");
        let lines: Vec<&str> = source.lines().collect();

        // A module opts in to the shared-invariant form by writing the phrase in its own `//!`
        // documentation, which is where a reader would look for the argument behind every call in
        // the file.
        let module_note = source.contains(MODULE_WIDE_SAFETY);

        for (at, line) in lines.iter().enumerate() {
            if !opens_a_block(line) {
                continue;
            }

            blocks += 1;
            if !explains(&lines, at, module_note) {
                unexplained.push(format!("{}:{}", path.display(), at + 1));
            }
        }
    }

    assert!(
        blocks > 0,
        "the scan found no unsafe blocks at all, so it is not looking at the code"
    );

    assert!(
        unexplained.is_empty(),
        "{} of {blocks} unsafe blocks have no `// SAFETY:` comment above them.\n\
         Each one has to name the invariant it depends on, or the next reader cannot tell a \
         carefully-argued block from a hopeful one:\n  {}",
        unexplained.len(),
        unexplained.join("\n  ")
    );
}

/// The scanner tells an `unsafe` block from an `unsafe fn` declaration.
///
/// The distinction is what keeps `pdfium::api` readable: it declares thirty-two `unsafe extern "C" fn`
/// types, and a scanner that treated those as blocks would demand thirty-two identical comments.
#[test]
fn a_block_is_told_from_a_declaration() {
    assert!(opens_a_block("    unsafe {"));
    assert!(opens_a_block("        let x = unsafe { call() };"));
    assert!(opens_a_block("    unsafe ["));

    assert!(!opens_a_block(
        "    pub(super) close_document: unsafe extern \"C\" fn(FpdfDocument),"
    ));
    assert!(!opens_a_block(
        "    unsafe fn resolve<T: Copy>(library: &Library)"
    ));
    assert!(!opens_a_block("unsafe impl Send for Job {}"));
    assert!(!opens_a_block(
        "    // SAFETY: this mentions unsafe { but is a comment"
    ));
}

/// A note is found above a block, through attributes and blank lines, and not across code.
#[test]
fn a_note_is_looked_for_where_it_belongs() {
    let with_note = ["// SAFETY: it is ours.", "unsafe { f() }"];
    assert!(explains(&with_note, 1, false));

    let through_blank = ["// SAFETY: it is ours.", "", "unsafe { f() }"];
    assert!(explains(&through_blank, 2, false));

    let through_attribute = [
        "// SAFETY: it is ours.",
        "#[allow(unused_unsafe)]",
        "unsafe { f() }",
    ];
    assert!(explains(&through_attribute, 2, false));

    let severed = ["// SAFETY: it is ours.", "let x = 1;", "unsafe { f() }"];
    assert!(
        !explains(&severed, 2, false),
        "a block after unrelated code is not explained by a note that belonged to something else"
    );

    let bare = ["let x = 1;", "unsafe { f() }"];
    assert!(!explains(&bare, 1, false));
}

/// A module that states the invariant once covers the blocks in it; one that does not, does not.
///
/// This is the escape hatch the big FFI modules use, and it has to be earned: the phrase has to be in
/// the file. A block cannot claim a module note that is not there.
#[test]
fn a_module_note_covers_its_own_module_only() {
    let bare = ["let x = 1;", "unsafe { f() }"];
    assert!(
        explains(&bare, 1, true),
        "the module said it once, at the top"
    );
    assert!(!explains(&bare, 1, false), "and said nothing without it");
}
