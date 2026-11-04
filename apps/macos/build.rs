//! Puts the vendored Pdfium next to the executable a build produces.
//!
//! ## Why a build script is doing this
//!
//! Pdfium is a shared library the app *loads* at run time rather than links against (see
//! `crates/note-ui/src/pdfium/`), so the linker cannot put it anywhere: a `cargo build --release` would
//! produce a `target/release/cheap-note` that finds `libpdfium.dylib` only by walking up from its own
//! directory to the repository's `vendor/lib` — that is, only while it sits in the build tree. Copied to
//! its own folder, or carried to another machine, it would report the library missing.
//!
//! Copying it next to the executable is what makes a release build a complete pair of files: start it from
//! anywhere, or carry those two files together, and PDFs open.
//!
//! ## What it does when the library is not there
//!
//! Nothing, loudly. The repository does not carry Pdfium — it is downloaded into `vendor/` (see
//! `vendor/README.md`), and `/vendor` is ignored by git — so a fresh clone builds without it, and the app
//! is designed to run without it: it starts, draws, writes ink, and reports that a PDF cannot be opened.
//! What a build must not do is *look* like it has PDF support, which is why the absence is a warning on
//! every build rather than silence.
//!
//! ## What this does not do
//!
//! Make an `.app`. A bundle is a directory with an `Info.plist`, an icon and the executable inside
//! `Contents/MacOS` — what Finder, the Dock and Gatekeeper want — and it is a packaging step rather than a
//! build one. The pair this produces runs from a terminal, which is what a release build has to do.

use std::path::{Path, PathBuf};

fn main() {
    let vendor = vendor_directory();
    let shipped = vendor.join("lib");

    // The library is not a source file, so Cargo has to be told the build depends on it: a download into
    // `vendor/lib` after the first build would otherwise not be noticed. `VERSION` is watched as well, so
    // an update that replaces the library re-runs this even when its timestamp is preserved.
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("VERSION").display()
    );
    println!("cargo:rerun-if-changed={}", shipped.display());

    // The version a load failure should name. Pdfium's public API has no version function, so this is the
    // build's provenance: it tells a person looking at a missing symbol which headers and which library
    // were meant to go together.
    println!(
        "cargo:rustc-env=CHEAP_NOTE_PDFIUM_VERSION={}",
        pdfium_version(&vendor)
    );

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
                library.display(),
                copy.display()
            );
        }
    }

    /// The directory this package keeps its downloaded libraries in.
    ///
    /// The repository's `vendor/`, which is *above* this crate: the app lives in `apps/macos`, and the library
    /// is downloaded once for the repository rather than per crate. So this walks up from the crate's own
    /// directory until it finds one, and falls back to the first candidate — which is what a build with no
    /// vendored library reports, and the sentence is the same either way.
    fn vendor_directory() -> PathBuf {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

        for ancestor in manifest.ancestors() {
            let candidate = ancestor.join("vendor");

            if candidate.is_dir() {
                return candidate;
            }
        }

        manifest.join("vendor")
    }

    /// The version of the Pdfium build in `vendor/`, from its `VERSION` file.
    ///
    /// The file is four `KEY=value` lines written by `pdfium-binaries`. A clone without a vendored library has
    /// no such file, and `unknown` is the honest answer rather than a guess.
    fn pdfium_version(vendor: &Path) -> String {
        let text = std::fs::read_to_string(vendor.join("VERSION")).unwrap_or_default();
        let mut parts = ["", "", "", ""];

        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            let slot = match key.trim() {
                "MAJOR" => 0,
                "MINOR" => 1,
                "BUILD" => 2,
                "PATCH" => 3,
                _ => continue,
            };

            parts[slot] = value.trim();
        }

        if parts.iter().any(|part| part.is_empty()) {
            String::from("unknown")
        } else {
            parts.join(".")
        }
    }

    /// The shared libraries in a directory, in a name order: `libpdfium.dylib`, and anything built beside it.
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
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("dylib"))
            })
            .collect();

        libraries.sort();
        libraries
    }

    /// The directory a build's executable lands in.
    ///
    /// Cargo gives a build script the *output* directory — `<target>/<profile>/build/<package>-<hash>/out` —
    /// and has no variable for the directory the executable itself goes in, which is three steps above it.
    /// Walking up is what that layout is for: it also answers correctly for a build with `--target`, where the
    /// same three steps land in `<target>/<triple>/<profile>`.
    fn beside_the_executable() -> Option<PathBuf> {
        let out = PathBuf::from(std::env::var_os("OUT_DIR")?);

        out.ancestors().nth(3).map(Path::to_path_buf)
    }

    /// Copies a file unless the copy already has what it says.
    ///
    /// This runs on every build, and copying seven megabytes of library for nothing on every build is a cost
    /// with no point to it. Size and time are the shortcut for "the copy is that library": a different library
    /// that happens to match in both is not a case a build has to survive.
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
}
