//! A PDF's bytes as a mapped region rather than a heap buffer.
//!
//! ## Why this exists
//!
//! Pdfium reads a document's bytes **in place** for as long as the handle is open, so the app has to
//! keep them alive — that is the `_buffer` field on `crate::pdfium::Document`, and it is not optional.
//! What *is* optional is where those bytes live.
//!
//! A `Vec<u8>` from `std::fs::read` lives in the allocator's large-block heap, and on macOS freeing one
//! does **not** give it back: measured, a 61 MB buffer read and dropped leaves the process 61 MiB larger
//! for the rest of its life. A reader who opens three textbooks therefore pays for all three at once and
//! keeps paying, which is the whole of the complaint this module answers.
//!
//! A mapped region has no such behaviour: `munmap` returns the pages to the kernel immediately —
//! measured, 385 MiB of mapped regions came back to 1.6 MiB. So a note's document is mapped when it is
//! opened and unmapped when it is closed, and the machine gets its memory back at the moment the reader
//! leaves the note.
//!
//! ## What it is not
//!
//! **Not a lazily-loaded document.** The whole file is mapped, and Pdfium touching any byte of it faults
//! that page in. What is saved is the *allocator's* copy keeping its pages after the free, not the
//! reading of the file.
//!
//! **Not portable for free.** `mmap` is a Unix call. A platform without it falls back to copying, which
//! is what this app did before and is correct everywhere — so the Windows build keeps working and loses
//! the saving rather than losing the feature.

use std::ops::Deref;

/// A file's bytes, mapped when the platform can and copied when it cannot.
///
/// Dereferences to the bytes themselves, so a caller hands `&[u8]` to Pdfium exactly as it handed a
/// `&Vec<u8>` before. Which of the two it is has no bearing on what a caller does with it.
pub struct MappedBytes {
    storage: Storage,
}

enum Storage {
    /// A read-only mapping of the file, unmapped when this is dropped.
    #[cfg(unix)]
    Mapped(Mapping),
    /// An owned copy, for a platform with no mapping call and for a file that could not be mapped.
    Copied(Vec<u8>),
}

impl MappedBytes {
    /// Maps a file for reading, or copies it when it cannot be mapped.
    ///
    /// A copy is the fallback for an empty file, a filesystem that will not map, and a platform with no
    /// `mmap`, because a document that opens is worth more than the memory it saves. The caller cannot
    /// tell which happened — both deref to the same bytes, and that is the point of the type.
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        if let Ok(mapping) = Mapping::open(path) {
            return Ok(MappedBytes {
                storage: Storage::Mapped(mapping),
            });
        }

        Ok(MappedBytes {
            storage: Storage::Copied(std::fs::read(path)?),
        })
    }

    /// Bytes that were already in memory, kept as they are.
    ///
    /// What a caller with no file gives this type — a document a test built, or bytes that came from
    /// anywhere but a path — so that every document has one shape whether it was mapped or not.
    pub fn solid(bytes: Vec<u8>) -> Self {
        MappedBytes {
            storage: Storage::Copied(bytes),
        }
    }

    /// Whether these bytes are a mapping rather than a copy.
    ///
    /// Read by the tests that check a note takes the mapped path, and by nothing in the app: which one
    /// it is has no bearing on what a caller does with the bytes.
    pub fn is_mapped(&self) -> bool {
        #[cfg(unix)]
        let mapped = matches!(self.storage, Storage::Mapped(_));
        #[cfg(not(unix))]
        let mapped = false;

        mapped
    }
}

impl Deref for MappedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.storage {
            #[cfg(unix)]
            Storage::Mapped(mapping) => mapping.bytes(),
            Storage::Copied(bytes) => bytes,
        }
    }
}

/// Equal to any bytes with the same contents, a `Vec` included.
///
/// The contents are the whole of what this type is for, and whether they arrived by mapping or by
/// copying is not a fact a caller can see or should care about — so a test that wrote a file and read
/// it back compares the two the only way that means anything.
impl PartialEq<[u8]> for MappedBytes {
    fn eq(&self, other: &[u8]) -> bool {
        &**self == other
    }
}

impl PartialEq<&[u8]> for MappedBytes {
    fn eq(&self, other: &&[u8]) -> bool {
        &**self == *other
    }
}

/// An array literal, which is how a test spells the expected bytes: `assert_eq!(bytes, b"%PDF-1.4")`.
impl<const N: usize> PartialEq<&[u8; N]> for MappedBytes {
    fn eq(&self, other: &&[u8; N]) -> bool {
        &**self == other.as_slice()
    }
}

impl PartialEq<Vec<u8>> for MappedBytes {
    fn eq(&self, other: &Vec<u8>) -> bool {
        &**self == other.as_slice()
    }
}

impl PartialEq for MappedBytes {
    fn eq(&self, other: &Self) -> bool {
        // Named as slices first: `*self == *other` resolves to this same implementation again.
        let this: &[u8] = self;
        let that: &[u8] = other;
        this == that
    }
}

impl std::fmt::Debug for MappedBytes {
    /// The length and how it arrived, rather than the bytes themselves.
    ///
    /// A malformed document printed in a test failure is thousands of binary bytes; the two facts worth
    /// seeing are how many there are and whether they were mapped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedBytes")
            .field("len", &self.len())
            .field("mapped", &self.is_mapped())
            .finish()
    }
}

/// A read-only `mmap` of a whole file, with the length that goes with it.
///
/// One type rather than two fields of [`Storage`] so that the `munmap` runs in a `Drop` that cannot be
/// reached without the pointer and the length being together.
#[cfg(unix)]
struct Mapping {
    pointer: *mut libc::c_void,
    length: usize,
}

#[cfg(unix)]
impl Mapping {
    /// Maps `path` read-only and private, or fails with whatever the operating system said.
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file = std::fs::File::open(path)?;

        let length = usize::try_from(file.metadata()?.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the document is larger than this machine can map",
            )
        })?;

        // A zero-length file has nothing to map and `mmap` refuses it. The caller's copy fallback
        // produces an empty buffer, which `Document::open` already rejects by name.
        if length == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the document is empty",
            ));
        }

        // SAFETY: a null hint lets the kernel choose the address, and every argument is a plain value.
        // The file is open for the length of the call, `PROT_READ` with `MAP_PRIVATE` is a read-only
        // private mapping, and `length` is the file's own size. A failure comes back as `MAP_FAILED`
        // and is checked before the pointer is used.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };

        if pointer == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Mapping { pointer, length })
    }

    /// The mapped region as a slice.
    fn bytes(&self) -> &[u8] {
        // SAFETY: the pointer and the length are the pair `mmap` returned, the region is mapped for
        // reading for as long as this value lives, and the slice borrows `self` so it cannot outlive the
        // mapping. `MAP_PRIVATE` means nothing else can change the file's contents under it.
        unsafe { std::slice::from_raw_parts(self.pointer.cast::<u8>(), self.length) }
    }
}

#[cfg(unix)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the pointer and the length are exactly what `mmap` returned and have not been
        // unmapped since — `Drop` runs once. The return value is deliberately ignored: a `munmap` that
        // failed would leave the region to be reclaimed with the process, and there is nothing a caller
        // could do about it.
        unsafe {
            libc::munmap(self.pointer, self.length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file written for one test, and removed when the test ends.
    ///
    /// The two tests that use it only run where `mmap` does, so the fixture is gated with them rather
    /// than compiled into a test binary on a platform whose `MappedBytes::open` copies the file.
    #[cfg(unix)]
    struct TempFile {
        path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl TempFile {
        fn with(contents: &[u8]) -> Self {
            // Named by a counter that is global rather than per-thread, and by the process, because two
            // tests in one binary run in parallel and must not share a file: a shared name is how the
            // first test to finish deletes the file the second is still reading.
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let path =
                std::env::temp_dir().join(format!("mapped-test-{}-{id}.bin", std::process::id()));

            std::fs::write(&path, contents).expect("the fixture writes");
            TempFile { path }
        }
    }

    #[cfg(unix)]
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A mapped file reads back exactly what was written.
    ///
    /// The whole point of the type: Pdfium is handed these bytes and must see the document, not a
    /// truncated or shifted copy of it.
    #[cfg(unix)]
    #[test]
    fn a_mapped_file_is_the_file() {
        let file = TempFile::with(b"%PDF-1.4 a whole document");

        let bytes = MappedBytes::open(&file.path).expect("the fixture maps");

        assert!(bytes.is_mapped(), "a regular file is mapped, not copied");
        assert_eq!(bytes, b"%PDF-1.4 a whole document");
        assert_eq!(bytes.len(), 25);
    }

    /// A mapping that is dropped leaves its bytes readable *before* the drop, and the type is the only
    /// thing keeping the region alive.
    ///
    /// This is the safety property the whole type exists for: the `munmap` must happen exactly when the
    /// `MappedBytes` goes, because Pdfium reads the region through a raw pointer for as long as the
    /// document handle is open.
    #[cfg(unix)]
    #[test]
    fn the_bytes_live_as_long_as_the_value() {
        let file = TempFile::with(b"%PDF-1.7 still here");

        let bytes = MappedBytes::open(&file.path).expect("the fixture maps");
        let fingerprint: u8 = bytes
            .iter()
            .map(|byte| byte ^ 0x5a)
            .fold(0, u8::wrapping_add);

        // Read again through the same value, then drop it: if the mapping had been released early the
        // read below would be a use of unmapped memory, which the guard pages catch as a crash.
        assert_eq!(bytes[..5], *b"%PDF-");

        drop(bytes);

        // The file itself is untouched by the mapping being dropped — `MAP_PRIVATE` is read-only, and
        // nothing in this type writes.
        let read_back = std::fs::read(&file.path).expect("the fixture still reads");
        assert_eq!(
            read_back
                .iter()
                .map(|byte| byte ^ 0x5a)
                .fold(0, u8::wrapping_add),
            fingerprint
        );
    }

    /// Bytes that never had a file are carried as they are, and say they are not mapped.
    #[test]
    fn solid_bytes_are_copied() {
        let bytes = MappedBytes::solid(vec![1, 2, 3]);

        assert!(!bytes.is_mapped());
        assert_eq!(bytes, vec![1, 2, 3]);
        assert_eq!(bytes, &[1u8, 2, 3][..]);
    }

    /// An empty file falls back to a copy, because `mmap` refuses a zero-length region.
    ///
    /// The copy is empty, which `Document::open` rejects by name — so this pins that the fallback
    /// happens rather than that an empty document is usable.
    #[cfg(unix)]
    #[test]
    fn an_empty_file_falls_back_rather_than_failing() {
        let file = TempFile::with(b"");

        let bytes = MappedBytes::open(&file.path).expect("an empty file still opens");

        assert!(!bytes.is_mapped(), "nothing to map, so it was copied");
        assert!(bytes.is_empty());
    }

    /// A file that is not there is an error rather than an empty value.
    #[test]
    fn a_missing_file_is_an_error() {
        let missing = std::env::temp_dir().join("mapped-test-does-not-exist");

        assert!(MappedBytes::open(&missing).is_err());
    }

    /// Two mappings with the same contents are equal, and two with different contents are not.
    #[cfg(unix)]
    #[test]
    fn contents_decide_equality() {
        let one = TempFile::with(b"same");
        let other = TempFile::with(b"same");
        let different = TempFile::with(b"other");

        let one = MappedBytes::open(&one.path).expect("maps");
        let other = MappedBytes::open(&other.path).expect("maps");
        let different = MappedBytes::open(&different.path).expect("maps");

        assert_eq!(one, other);
        assert_ne!(one, different);
        assert_ne!(one, MappedBytes::solid(b"same but copied".to_vec()));
    }
}
