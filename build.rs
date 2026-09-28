//! Puts the vendored Pdfium next to the executable a build produces.
//!
//! ## Why a build script is doing this
//!
//! Pdfium is a shared library the app *loads* at run time rather than links against (see
//! `src/pdfium.rs`), so the linker cannot put it anywhere: a `cargo build --release` used to produce a
//! `target/release/cheap-note.exe` that could only find `pdfium.dll` if it happened to be started from
//! the project root, where `vendor/lib` is. Started the way a release executable *is* started — by its
//! own directory, or from anywhere else at all — it reported the library missing.
//!
//! Copying it next to the executable is what makes a release build a complete pair of files: start it
//! from anywhere, or carry those two files to another machine, and PDFs open.
//!
//! ## What it does when the library is not there
//!
//! Nothing, loudly. The repository does not carry Pdfium — it is downloaded into `vendor/` (see
//! `vendor/README.md`), and `/vendor` is ignored by git — so a fresh clone builds without it, and the
//! app is designed to run without it: it starts, draws, writes ink, and reports that a PDF cannot be
//! opened. What a build must not do is *look* like it has PDF support, which is why the absence is a
//! warning on every build rather than silence.

use std::path::{Path, PathBuf};

fn main() {
    let shipped = vendor_directory().join("lib");
    // The library is not a source file, so Cargo has to be told that the build depends on it: a
    // download into `vendor/lib` after the first build would otherwise not be noticed.
    println!("cargo:rerun-if-changed={}", shipped.display());

    let Some(beside) = beside_the_executable() else {
        return;
    };

    let libraries = libraries_in(&shipped);
    if libraries.is_empty() {
        println!(
            "cargo:warning=Pdfium was not found in {}: this build has no PDF support. \
             See vendor/README.md for where it comes from",
            shipped.display()
        );
        return;
    }

    for library in libraries {
        let Some(name) = library.file_name() else {
            continue;
        };
        let copy = beside.join(name);
        if let Err(error) = copy_if_newer(&library, &copy) {
            println!(
                "cargo:warning={} could not be copied to {}: {error}",
                name.to_string_lossy(),
                copy.display()
            );
        }
    }
}

/// The directory this package keeps its downloaded libraries in.
fn vendor_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor")
}

/// The shared libraries in a directory, in a name order: `pdfium.dll`, and anything built beside it.
fn libraries_in(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut libraries: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
        })
        .collect();
    libraries.sort();
    libraries
}

/// The directory a build's executable lands in.
///
/// Cargo gives a build script the *output* directory — `<target>/<profile>/build/<package>-<hash>/out`
/// — and has no variable for the directory the executable itself goes in, which is three steps above
/// it. Walking up is what that layout is for: it also answers correctly for a build with `--target`,
/// where the same three steps land in `<target>/<triple>/<profile>`.
fn beside_the_executable() -> Option<PathBuf> {
    let out = PathBuf::from(std::env::var_os("OUT_DIR")?);
    out.ancestors().nth(3).map(Path::to_path_buf)
}

/// Copies a file unless the copy already has what it says.
///
/// This runs on every build, and copying seven megabytes of library for nothing on every build is a
/// cost with no point to it. Size and time are the shortcut for "the copy is that library": a
/// different library that happens to match in both is not a case a build has to survive, and the copy
/// that is *there* is the one an executable already running was loaded from — which is why a locked
/// file is reported rather than worked around.
fn copy_if_newer(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = std::fs::metadata(from)?;

    if let Ok(existing) = std::fs::metadata(to) {
        if existing.len() == source.len() {
            if let (Ok(copied_at), Ok(written_at)) = (existing.modified(), source.modified()) {
                if copied_at >= written_at {
                    return Ok(());
                }
            }
        }
    }

    std::fs::copy(from, to).map(|_| ())
}
