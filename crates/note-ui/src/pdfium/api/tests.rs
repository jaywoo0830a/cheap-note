use super::*;

/// The project root the tests pretend to have been built in. Nothing is read from it: what is
/// being pinned is which directories a *shape* of build tree leads the search to.
const ROOT: &str = "C:/projects/cheap-note";

/// A release executable finds the repository's own library — which is where a release build is
/// normally started from, and what it used to fail to find.
///
/// The regression in full: `target/release/cheap-note.exe`, double-clicked, is started *by* its own
/// directory and has the project root two directories above it, with `pdfium.dll` in `vendor/lib`
/// there. The search looked next to the executable and in the working directory — which for that
/// launch *is* the executable's directory — so the one arrangement a release build is used in was
/// the one arrangement that could not find the library.
#[test]
fn a_release_executable_finds_the_repositorys_library() {
    let root = Path::new(ROOT);
    let release = root.join("target").join("release");
    let candidates = candidates_for(Some(&release.join("cheap-note.exe")), None);

    assert_eq!(
        candidates.first(),
        Some(&release),
        "next to the executable comes first: that is where a build puts the library"
    );
    assert!(
        candidates.contains(&root.join("vendor").join("lib")),
        "and the repository's copy is found by walking up from it: {candidates:?}"
    );
}

/// The same for the two executables a development tree has: the debug one, and the one the tests
/// themselves are run from, which sits a directory deeper.
#[test]
fn a_debug_executable_and_a_test_executable_find_it_too() {
    let root = Path::new(ROOT);
    let wanted = root.join("vendor").join("lib");

    for executable in [
        root.join("target").join("debug").join("cheap-note.exe"),
        root.join("target")
            .join("debug")
            .join("deps")
            .join("cheap_note-1a2b3c4d.exe"),
    ] {
        let candidates = candidates_for(Some(&executable), None);

        assert!(
            candidates.contains(&wanted),
            "{}: {candidates:?}",
            executable.display()
        );
    }
}

/// The working directory is what a `cargo run` from the project root has — and a process that knows
/// neither its own path nor a working directory is told so rather than handed a guess.
#[test]
fn the_working_directory_is_searched_with_nothing_better_to_go_on() {
    let root = Path::new(ROOT);

    assert!(
        candidates_for(None, None).is_empty(),
        "nowhere to look, and no pretending otherwise"
    );
    assert_eq!(
        candidates_for(None, Some(root)),
        vec![
            root.join("vendor").join("lib"),
            root.join("vendor"),
            root.to_path_buf(),
        ],
        "the working directory's own vendor/lib is what a run from the project root finds"
    );
}

/// The C layouts are part of the ABI, and nothing else checks them: a field inserted in the
/// wrong place would still compile and then be read by Pdfium as a different value. These are
/// the layouts `vendor/include` declares on the x64 Windows build this app ships.
#[test]
#[cfg(target_pointer_width = "64")]
fn the_c_types_have_the_layout_the_vendored_header_declares() {
    use std::mem::{offset_of, size_of};

    assert_eq!(size_of::<SizeF>(), 8, "two f32s, no padding");
    assert_eq!(offset_of!(SizeF, width), 0);
    assert_eq!(offset_of!(SizeF, height), 4);

    assert_eq!(size_of::<Pause>(), 24, "int, callback, user pointer");
    assert_eq!(offset_of!(Pause, version), 0);
    assert_eq!(offset_of!(Pause, need_to_pause_now), 8);
    assert_eq!(offset_of!(Pause, user), 16);

    assert_eq!(
        size_of::<LibraryConfig>(),
        32,
        "the version-2 prefix: int, paths, isolate, slot"
    );
    assert_eq!(offset_of!(LibraryConfig, version), 0);
    assert_eq!(offset_of!(LibraryConfig, user_font_paths), 8);
    assert_eq!(offset_of!(LibraryConfig, isolate), 16);
    assert_eq!(offset_of!(LibraryConfig, v8_embedder_slot), 24);
}
