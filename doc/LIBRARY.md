# Backing up every note, and getting them back

This document follows the **library**: every note the app holds, written as one zip on the desktop, and
read back on another machine — which button does what, what is in the file, what happens to a note that is
already there, and what the app refuses. It is the *operation* written down; what a note *is* on disk is
[STORE.md](STORE.md), and one stroke's journey to the disk and back is its §6 and §7.

If you read only one paragraph, read this one:

> **One button writes every note to one zip on the desktop, and a machine that has never seen a note takes
> that file in on its own.** The file is `cheap-note-library.zip`; it is a plain zip of *notes* — each one
> vacuumed exactly as *Save* vacuums one — so it can be opened by anything, and read back by starting this
> app with it on the desktop. A machine that already has notes is never touched without being asked.

---

## 1. Why a file, and not a server

A backend — accounts, a bucket, a sync loop — would solve three problems: *get all my notes out of this
machine*, *carry them*, and *get them into another one*. All three are already solved by what a note is.
A note is a folder with a database in it, and a database has one honest way out: `VACUUM INTO`, which the
*Save* button has used from the beginning ([STORE.md](STORE.md) §8). A library is that same operation,
done for every note at once, plus a place to put it and a way to take it back.

What a server would have bought, and what this buys instead:

| A server gives | This gives |
|---|---|
| nothing to press | *Back up everything* — one press, no dialog |
| a URL to install | a file on the desktop |
| an account, and a bill | nothing |
| notes in somebody's bucket | notes in a zip you can open with `unzip` |

What it costs: **carrying the file is the person's job.** A USB stick, a mail client, a cloud folder
someone already syncs — any of those moves it, and none of them is this app's business. The one thing
this app will not do is put a live database in a folder a synchroniser copies under it — that is the rule
[STORE.md](STORE.md) §1 starts from, and a zip is how a note gets out of the app into such a folder
without being a live database.

---

## 2. What a library is

```text
cheap-note-library.zip                       (on the desktop)
    library.json                             what this is, when, and which notes
    notes/chapter-3-9f2a1c44/note.db         a VACUUM INTO copy: complete, standalone
    notes/chapter-3-9f2a1c44/source.pdf      the document, byte for byte (if the note has one)
    notes/chapter-3-9f2a1c44/attachments/…   anything the note carries
    notes/blank-1712345678901/note.db
```

`library.json` is the manifest, and it is deliberately readable JSON:

```json
{
  "version": 1,
  "created_at_ms": 1731900000000,
  "notes": ["blank-1712345678901", "chapter-3-9f2a1c44"]
}
```

* `version` — the library's own format, `1`. A library from a *newer* build is refused as newer, not as
  broken (§6): the file is fine, this build is old.
* `created_at_ms` — when it was written, on the same clock a note's rows use. The status line shows it as
  an age ("made 2 days ago"), because an age needs no timezone.
* `notes` — the folder name of every note in it, in the order they were written. Folders are listed in
  name order by `note::notes_in`, so this is also the order the next backup would use.

### Why the notes are flat, and not one zip of zips

*Save* writes **one note** as one zip. A library written as a zip of those would be a zip inside a zip:
the outer file could not be opened by anything that does not also open the inner one, each note would be
deflated a second time for no gain (a SQLite file and a PDF are compressed already), and the day somebody
lost the app would be the day they could not read their own writing without unzipping twice.

Laid out flat, the library is a **file manager's zip**. `unzip cheap-note-library.zip` gives
`notes/<folder>/note.db` and the document beside it, and `sqlite3 notes/chapter-3/note.db "select 1"` says
it is a database. That is the whole reason the layout is this one.

### Why the folder names are kept

A folder's name is a note's identity ([STORE.md](STORE.md) §1): the document's stem plus a digest of its
full path, which is what makes importing the same PDF twice find the same note. A library that renamed
folders on the way in would turn one note into two on every restore. So a note arrives on the second
machine under the name it had on the first, and opening the same PDF there still lands in that folder.

### A name that means the same thing on every machine

Keeping the name is only half of it: the name has to be one *every* filesystem will take, and the same
one on all of them, or the note that arrives is not the note that left. `note::folder_name` therefore
hands out a name from a rule that is the same on Windows, macOS and Linux:

* ASCII letters, digits, `_` and `-` are kept; every other run — a space, a Korean vowel, an emoji, a
  Windows-reserved character — becomes **one dash**, and nothing is left at either end. A stem with no
  ASCII left at all (`한글 보고서.pdf`) becomes `note-<digest>`.
* The digest is over the path's **UTF-8**, so the same path digests to the same number on a platform
  that spells paths in UTF-16 and one that spells them in bytes.

The two bugs this closes, both of them a Mac-to-Windows crossing:

| Before | After |
|---|---|
| `is_alphanumeric` kept `한글` on macOS and threw it away on Windows, so one note was `한글-9f2a1c44` on one machine and `note-9f2a1c44` on the next | one ASCII name, the same string on every platform |
| a trailing `.` or `-` was kept, and Windows *strips* it — the folder on disk stopped being the folder the manifest named | no name ends in `.`, ` ` or `-` |

The digest is what keeps two such documents apart: `보고서.pdf` and `계획.pdf` share a stem of `note`,
and only their paths do.

### How a name is written into the zip

A zip can carry its entry names as ASCII, as code page 437, or as UTF-8 with the entry's language flag
(bit 11) set — and which one a tool *reads* is the tool's choice. The two tools in the way of a note
that crosses from a Mac to a Windows machine disagree: a Mac's `zip` reads 437 unless the flag is set,
while Windows Explorer and older WinZip builds read 437 unless the flag is set **and** the entry carries
the Info-ZIP Unicode Path field (`0x7075`, APPNOTE 4.6.8) — the name again, in UTF-8, with the CRC-32 of
the stored name in front of it. `note::write_zip` writes both: the flag (the `zip` crate sets it for any
name that is not ASCII) and the field. `tests/library.rs` reads them back with a plain zip reader, the
way a Windows tool would. So a library with `notes/한글-보고서-…/` in it unpacks to that folder name
wherever it is opened.

A note's **document** keeps its own name too, in the row a note writes about itself (`meta`'s
`document`). A library carries the folder, not that row's source of truth — so `place_library` fills the
row in from `source.pdf` when it is missing, and a restored PDF note is called `한글 보고서`, not
`source`.

The same rule applies to the note's own *source* row (the file it was made from, kept for "open the
original again"): it is written with forward slashes, so `C:\notes\a.pdf` is stored as
`C:/notes/a.pdf`, which every platform can read and which Windows accepts as well. And the recent list
never takes the other machine's spelling of it: a merge of two windows' lists keeps the source this
machine has when the two name different files (`recent::merge`).



---

## 3. Backing up: what *Back up everything* does

`home.rs`'s button calls `NoteApp::back_up_everything` (`crates/note-ui/src/app.rs`), which is the
application's half; `note::write_library` (`crates/note-format/src/note.rs`) is the format's. In order:

```text
press
  │
  ├─ <30 s since the last press?        → "a backup is already being written", stop
  ├─ persist(now)                       → the page in hand is on the disk before the copy is taken
  ├─ no note folder in the notes folder → "there are no notes to back up yet", stop
  ├─ pick the folder                    → desktop → home → the notes folder's parent  (§4)
  │
  └─ background executor: note::write_library
        for each note folder:
            VACUUM INTO <note>/library.tmp.db
            plus source.pdf and attachments/   (Stored / Deflated)
        write library.json beside the staging zip
        write .tmp-<pid>.library.zip  ← every entry
        rename it onto cheap-note-library.zip
        delete every library.tmp.db and the staging files, success or not
```

Four decisions in that picture are worth their own sentence:

* **The page in hand is written first.** `persist` hands the open page's ink to the note's writer before
  anything is copied. A backup is the copy a person trusts; a stroke that was still in memory would be a
  stroke it does not have.
* **The copy comes from `VACUUM INTO`, not from the files.** The note may be open, so `note.db` may have
  its newest pages in `note.db-wal`. `VACUUM INTO` reads the database as one consistent snapshot and
  writes a standalone file — no log, no free pages ([STORE.md](STORE.md) §8). The vacuum is made in the
  note's *own* folder (`library.tmp.db`), so it is a copy on one volume rather than two, and it is
  deleted whether the library was written or not.
* **The zip is built beside the target and renamed onto it.** `cheap-note-library.zip` is written as
  `.tmp-<pid>.library.zip` in the same folder and renamed at the end. A backup pressed over last week's
  library either replaces it with a complete one or leaves last week's file exactly as it was — never a
  half-written file carrying every note's name, and never a failed backup that lost the good one. Every
  temporary is removed on every path out, by a `Drop` that lists them.
* **It runs on the background executor, not on the note-writer thread.** The writer's queue belongs to
  the one note that is open; a library is about all of them, including the ones nothing has opened. The
  UI thread is not blocked, and the status line says `writing every note as one file…` while it runs.

One name, not one per day: `cheap-note-library.zip` is always the same file, because the button means
"put my whole library where I can find it". Yesterday's copy is worth less than today's, and replacing it
is what makes pressing the button twice make sense.

When it is done the status line says `backed up 12 notes to cheap-note-library.zip`, or, on a failure,
`nothing was backed up: <the reason>` — the operating system's own sentence, because that is the
information a person needs.

---

## 4. Where it goes, and what happens when there is no desktop

`library::backup_target` picks the folder, in this order:

| # | Folder | When |
|---|---|---|
| 1 | `$XDG_DESKTOP_DIR`, else `~/Desktop` (macOS, Linux); `%USERPROFILE%\Desktop`, else `%OneDrive%\Desktop` (Windows) | the folder **exists** |
| 2 | the home folder (`$HOME`, `%USERPROFILE%`) | no desktop, or the desktop is configured but missing |
| 3 | the notes folder's parent (`%LOCALAPPDATA%\cheap-note`) | neither exists — a folder the app already owns |

GPUI — the one thing here that knows about windows — has no question for "where is the desktop", so it is
answered from the environment (`library::desktop_folder`, §8). A folder that is *configured* but not
*there* is skipped rather than written to: a backup in the second-best folder is better than an error. The
status line names whichever folder was used, so nobody has to guess where the file went.


---

## 5. Restoring: the two ways in

### 5.1 Automatically, when the machine is empty

`NoteApp::restore_on_start` runs once, at the end of `NoteApp::new`, after anything on the command line
has been opened. It restores only if **all four** hold:

1. nothing was opened on the command line;
2. the app has nothing of its own — `library::is_empty`: the list has no entry still on disk, **and** the
   notes folder has no note folder in it;
3. a desktop folder exists;
4. `cheap-note-library.zip` is in it.

That is exactly a machine that has never seen a note — a fresh install, a new laptop — where there is
nothing to lose. It is the *only* thing this app does without being asked, so the guard is what makes it
safe: a machine with notes is never touched.

```text
second machine, app started
  │
  ├─ anything opened on the command line?          → nothing happens
  ├─ a note in the list, or a note in the folder?  → nothing happens
  ├─ no desktop, or no library on it?              → nothing happens
  │
  └─ place_library(desktop/cheap-note-library.zip, notes folder)
        for each note in the library:
            unpack into .tmp-<pid>.<name>/            (nothing outside the note is written)
            move the existing <name>/ aside, if any
            rename .tmp-<pid>.<name> → <name>/         ← the note takes its place
            drop the old copy (or put it back, if the rename failed)
        reload the list; scan it: the notes are adopted by their folders
        status line: "restored 12 notes, with a library made 2 days ago from
                      cheap-note-library.zip — this machine had no notes of its own"
```

The file is **left on the desktop**. It may be somebody's only copy, and one that has just come through a
mail client is not a file to delete. It is not taken in twice either: after the first restore the machine
has notes, so the guard above fails on the next start, and the person is asked rather than acted for.

### 5.2 By hand, on a machine that has notes

*Restore a library…* on the home screen asks for a file (`Choose a cheap-note library to restore`) and
places what it holds with the same `note::place_library`. This path **does replace** the notes the library
names: a person who chose the file is saying that its copy is the one they want, and it is the path that
can say so out loud. A note that was open is let go of — the writer's checkpoint on the way out is
harmless — because the folder behind it is no longer the folder it was opened from.

### What "replace" means, exactly

A note is unpacked into a temporary folder *beside* its destination and renamed onto it, so a restore is
one atomic step per note:

* the old note is moved aside → the new one is renamed into place → the old one is deleted;
* if the new note cannot be renamed into place, **the old one is put back**;
* if unpacking fails part-way, the temporary folder is removed and the existing note is untouched.

So a restore that is interrupted, refused, or out of disk leaves either the old note or the new one — never
the two mixed, and never nothing.


---

## 6. What the container refuses

A library is a file somebody else can make, hand to a person, and have taken in. These are the refusals —
all `Variant::NotANote` unless said otherwise — and all of them are decided **before** anything is written
to the notes folder:

| The file | What happens |
|---|---|
| not a zip this build can read | `… is not a library: it is not a zip this app can read` |
| a zip with no `library.json` (a folder of photographs, say) | `… is not a library: it holds no library.json` — nothing is placed |
| a `library.json` that is not the manifest | refused, with the JSON parser's own reason |
| `version` newer than `LIBRARY_VERSION` | refused as `NewerBuild` — "written by a newer build of the app"; the file is fine, this build is old |
| an entry whose path climbs out of its note (`../…`, `/…`, `C:…`) | `… holds an entry that points outside its note: <name>` — **at the listing**, before anything is written |
| a note under a folder name that is not one (`..`, a separator, empty) | refused for the same reason |
| `notes/<folder>/` with no `note.db` in it | not a note: left out of the listing rather than offered as one |

The path rule is one function shared by both halves — `is_note_path` / `is_folder_name` in
`crates/note-format/src/note.rs` — because it is the one way a file from elsewhere reaches into a disk it
was not given. It is enforced twice: when the library is *listed* (so a tampered library is refused whole,
rather than placed around the bad entry) and again when each note is *written* (so the half that writes
does not trust its caller). `tests/library.rs` pins both, including that a refused restore leaves the note
that was already in that folder exactly as it was.

---

## 7. Known limits

Said out loud, because each one is a decision rather than an oversight:

* **Nothing backs up on its own.** There is no timer and no on-quit backup today; the same
  `note::write_library` call is what an automatic one would use (§9).
* **The automatic restore only fires on an empty machine.** A machine with notes shows nothing when a
  library is put on its desktop — that is the guard, and the answer there is *Restore a library…*.
* **The backup is made in one pass**, so on a large library the press takes a few seconds. The UI is not
  blocked and the status line says what is happening, but there is no progress bar.
* **No versions are kept.** One name means today's library replaces yesterday's; keeping both is a copy in
  a folder that already keeps versions (a sync service, a Time Machine disk).
* **Carrying the file is manual.** That is the trade against a server, and the one thing the app cannot do
  for you.
* **Uploading the file somewhere** is the obvious next step, and it is a *syncing* problem rather than a
  backup one: it would need an account, and therefore the bill this feature exists to avoid.

---

## 8. Where the code is

| Where | What |
|---|---|
| `crates/note-format/src/note.rs` | `write_library`, `read_library`, `place_library`; `LibraryManifest`; `LIBRARY_MANIFEST` / `LIBRARY_NOTES` / `LIBRARY_VERSION`; the shared `write_zip` (with the Info-ZIP Unicode Path field), `folder_name`, `is_note_path`, `is_folder_name`, `record_document_name` |
| `crates/note-format/src/store.rs` | `set_source` (written with forward slashes, so a path carried to another platform is one it can read) |
| `crates/note-ui/src/library.rs` | the application's half: `LIBRARY_FILE`, `backup_target`, `desktop_folder`, `home_folder`, `is_empty`, `describe` — pure functions of paths, so they are tested without a window |
| `crates/note-ui/src/app.rs` | `back_up_everything` (the button), `restore_library` (the chooser), `place_library_file`, `restore_on_start`, `BACKUP_MIN_INTERVAL` |
| `crates/note-ui/src/home.rs` | the two buttons in `actions`, and `Home::reload` after a restore |
| `crates/note-ui/src/recent.rs` | `Recent::same_file_as` and `merge`: a list never takes the other machine's spelling of a note's source |
| `crates/note-format/tests/library.rs` | the container from outside: the layout and manifest read with a zip reader and a JSON parser, a restore's ink and document coming back, attachments, **the UTF-8 flag and Unicode Path field a Windows tool reads**, a Korean folder and document name surviving the restore, the refusals, and a refused restore leaving the note that was there |
| `crates/note-ui/src/library/tests.rs` | where a backup goes and the fallback order, the guard on the automatic restore, and the words the status line shows |
| `crates/note-ui/src/recent/tests.rs` | the merge, including that a note carried to another machine keeps *this* machine's source |

The two rules a change has to keep:

* **The format's tests** open a library with a zip reader and a JSON parser only, the discipline every
  other format test keeps: a library written here has to be readable by something that has never seen this
  code.
* **The view's tests** never make a window: where a backup goes is a rule about paths, and a rule about
  paths can be asked with paths.

---

## 9. If anything is added

* **Automatic backup** (on quit, or once a day) is the same function called from `NoteApp`'s close path —
  nothing in the format changes.
* **Keeping versions** is a change to `backup_target` alone: a name with a date in it, and the folder a
  person already keeps versions in.
* **A `.cheapnote` file association** for a library is packaging rather than code: the command-line path
  already opens a file's notes, and `cheap-note <library.zip>` is the line it would take.
* **Asking before an older library replaces newer notes** is `LibraryManifest::created_at_ms` against each
  note's `Stamp` — the figure is in the file for exactly that reason.
* **A different naming scheme** is a change to `folder_name` and nothing else — but it changes a note's
  *identity*, so the old names have to keep being found: an existing note is opened by the folder it is
  already in, and only a *new* import asks `folder_name` (`app::open_or_place`). The rules §2 lays out
  (ASCII-safe, the same on every platform, the digest over the path) are what a second machine relies on;
  a name that breaks one of them arrives as a different note, or does not arrive at all.
