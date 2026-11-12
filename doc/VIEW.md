# Why the view is the way it is

This document is the reasoning behind `crates/note-ui/src/app.rs` — the arguments that used to be written out in the
file itself. The code keeps one-line reasons where a reader needs them at the line they are reading;
this keeps the arguments, in the order a person meets them.

It is meant to be read after [ARCHITECTURE.md](ARCHITECTURE.md) (what runs where) and beside
[STORE.md](STORE.md) (where the ink goes).

If you read only one paragraph, read this one:

> **The canvas fills the window and everything else floats over it**, because a pen reading then needs
> no offset arithmetic to become a stroke. The pen's own wake draws the ink and presents it
> immediately — the interface's frame is too expensive to wait for — and everything that is *not* the
> ink (a page rasterised, the counters, the writer's reports) happens on a one-millisecond housekeeping
> wake of its own. The status line's counters rebuild once a second and are written to a log file, the
> bar can be turned off, and the pen is refused by a line or a state, never by a rectangle.

## 1. The screen: an overlay, and what it costs

The ink canvas fills the whole content area, and the toolbar is drawn *over* it. That is not only a
visual choice: `pen-windows` reports the pen in **client-window** coordinates and GPUI paints in
**window** coordinates, so an ink point needs no layout arithmetic at all. A toolbar that took part in
the layout would sit at the top of the content area, every stroke would have to be offset by its
height, and that offset would have to be captured from a layout callback — exactly the coupling this
avoids.

The overlay costs the pen **one number**: where the bar ends (`NoteApp::bar_edge`). A reading that
lands on the bar is a press on a control rather than ink, so that line has to be the bar's *real* bottom
edge — observed from the frame, since only the toolkit knows how the bar wrapped — while no ink point
is ever *moved* by the bar existing, which is the offset this design avoids. That one line is where
everything about the pen changes its mind: ink refuses a `Down` above it, the ghost cursor is not drawn
above it, and the system pointer is the cursor there. `BAR_HEIGHT` (148 px) is only the estimate the
line falls back to on the frame before the bar has been measured; every frame after that uses the
measured edge, so a bar that wrapped taller than the estimate hides the ghost across all of itself and
a page under a short one is page. A bar that is hidden has no edge at all (`0`), even though the switch
that hid it was inside the strip it gives back — and the toggle publishes the new line *before* the
frame that redraws the bar, so the hand that hid it is not left with the arrow.

Two margins go with it: `PAGE_MARGIN` (24 px) between the sheet and the window's edges — the bar covers
that strip, which is the price of the overlay — and `BAR_MARGIN` (12 px) between the floating bar and
the floating page pill and the window's edges, so the desk shows in the gap. That gap is what makes the
sheet read as paper on a desk rather than as a rectangle filling a window.

## 2. The loop

Nothing here polls for pen input. The pen thread pushes readings into a queue, and an async task
(`start_pen_pump`) *waits on that queue* — not on a timer — takes every batch the instant it lands,
feeds the ink model, and calls `cx.notify()`. A frame is therefore scheduled exactly when there is
something new to show, at the rate the pen reports it: 133 Hz, 240 Hz, whatever the digitizer sends,
with no ceiling taken from any display. The app is idle, and spends nothing, otherwise.

"Something new" is **one** thing: ink that changed. The pen's ghost cursor used to be the other, and it
has left the frame altogether — a pen held in range without touching lays no ink, and its cursor is
drawn in a window of its own, fed from the pen thread (see `cursor_overlay`). So a batch that laid no
ink has nothing to show and no frame is drawn for it — which is what stopped the top of the window
flickering while writing.

**The wake draws the canvas itself**, rather than waiting for a frame to. The ink is the one part of a
canvas that changes between frames, and a frame on this stack is redrawn on every vblank *and* carries
the whole interface with it: measured at 0.89 ms of this app's own work inside a 9.6 ms interval, the
rest of the interval belonging to the window's own drawing. A present of the canvas alone is about a
millisecond. So the pump hands the canvas to its layer and the compositor shows the newest present it
is given:

* **immediately, never after waiting for the compositor.** A waitable frame-latency object was tried
  and made the ink *worse* — every present waited for the compositor to take the last one, which
  measured `present 21.16 ms (47/s)` where the frames it replaced reached the screen a hundred times a
  second. A dropped frame costs its own draw (0.3–1 ms); a wait costs more, every time.
* **and a frame still draws the canvas too**, because a frame is what changed the *sheet* — the zoom,
  the pan, the page, the paper — and because the sheet's geometry is decided while an element tree is
  being built. Both call `draw_canvas`.
* **a failure keeps the layer.** There is no second renderer to fall back to — a frame paints none of
  the canvas — so a dropped layer is a canvas that is gone, while a kept layer holds its last presented
  frame and can say what is wrong. It is reported once, because a line rebuilt every frame is work the
  frame does not have to carry.

The second task (`start_display_pump`) does the work that is not the ink — rasterising the page,
rebuilding the counters, draining the writer — on `HOUSEKEEPING_INTERVAL`, **one millisecond**: the
shortest wait that still parks the task rather than spinning it, and a fixed floor rather than a rate
taken from the display, because nothing in the loop reads the monitor any more. The work it does is
skipped while the pen is laying ink, so waking this often costs a timer and a comparison when there is
nothing to do, and a rasterisation can never delay a stroke.

## 3. The top bar

**Two rows and one surface.** The first row is what the reader is *doing* — the document, the tool in
hand, and the commands that act on the note — and the second is what is being written *on*: the sheet's
size, its ruling, the two colours, and the pen's weight. The second row wraps and the first does not.
The bar floats for the reason §1 gives, and it is rounded and lifted off the desk because that is what
makes the sheet underneath read as paper.

Three details in it are load-bearing:

* **It reports where it ended.** The one number the overlay costs the pen is the bar's bottom edge, and
  the bar publishes it with a zero-height marker pinned to its own bottom edge: its bounds are right
  whatever the bar's height turned out to be, and a bar that wraps into three rows reports the third
  row's bottom rather than a guess. It paints nothing, takes no pointer, and is out of the layout — it
  exists to be measured.
* **Its width comes from a full-width box with a margin's worth of padding**, not from setting both
  insets on the bar itself: an absolutely positioned element with a left *and* a right inset is laid out
  at its content's size here rather than stretched between the two, and a bar at its content's size
  never wraps its second row — it runs off the edge of the window.
* **Its elements are collected into owned vectors before they are chained onto the row**, because each
  `cx.listener` takes a mutable borrow of the context and one long builder chain would hold them all at
  once.

**Why the second row is boxes and swatches rather than buttons.** Six sizes, four rulings and five pens
as buttons was more than a narrow window holds, and the one that was current had to be found among its
neighbours rather than read off the control — so size, ruling and pen are *choosers*, each a single box
showing what is in use with the alternatives in a list under it, while the twelve colours stay a row of
swatches, which is what a palette is. Each group is captioned, because without the words it would be a
guess which of the two runs of squares is the paper and which the ink. The pen's weight sits beside the
ink's colour because the two are one answer — the colour is *which* pen, the weight is *how heavy* it
is — and its caption is a property (`Gray`) rather than a thing (`Pen`), since `Ink` already captions
the colours. **The weight is spelled out in millimetres, and in nothing else**: each box in the list reads
`0.58 mm`, because a name is a label and a thickness is what a reader is actually choosing between — five
boxes reading Fine, Light, Normal, Bold, Heavy say nothing about what they will draw. The number is measured on the
paper (`settings::PenWeight::millimetres`), on the app's own millimetre — the scale every canvas size is drawn at — and
it is
the line a pen lays *without* pressure, which is the nib's own thickness: a pen that reports pressure draws thinner and
thicker either side of it. The note stores the pen as the *multiplier* rather than as that number
(`settings::PenWeight::scale`, which is what `pen_weight` holds), because a thickness belongs to a nib and a
multiplier does not: the same "0.29 mm" is the fine pen on one note and the light one on a note whose plain
nib is wider, and a row that held a measurement would mean a different pen on each.

A chooser's width is set on the **row**, not on the select: a `Select` fills its parent by design (it is
a form field, and a form field is as wide as the field it is on), so in a row it would take the whole
bar. It is wide enough for the longest label (`Square`, `Letter`) so the box does not resize as the
choice changes, which would move the controls beside it.

`sync_choosers` keeps each box showing what the note is actually written with, and it is where the zoom
field is filled too — the same rule, one row down: the *view* changes the zoom under the field the way a note
opening changes the boxes. What the style *is* and
what a box *draws* are two things, and they are kept in step the way `home` keeps its list's highlight
on the row Enter would open: three reads a frame, a write only on frames where they disagree. This is
not cosmetic — these boxes are the only control that changes the paper, the ruling and the pen, and a
box showing A4 while the note is A5 is a press that asks for A4 and reports A4. It moved from cosmetic
to necessary when the style became the *note's*: opening a note now changes all three without any box
being pressed.

**The title chip** holds the note's *name* — the one a person gave it, or the one derived from the
folder while it has none ("chapter-3", "Blank sheet"). The file it was written on is still in the status
line, because a note that was renamed still came from somewhere. Double-clicking it is the whole of the
way in: there is no button beside it, because there is no second gesture to offer — every notebook turns
its own title into a field when it is double-clicked — and while a name is being typed the field is
*here*, in place, because a name is edited where it is read.

### The status line

The counters sit in a translucent pill **on the desk**, out of the bar, because it is a readout rather
than a control — and because the line changes four times a second: text in the bar would re-lay-out the
bar, while text in a pill of its own re-lays-out the pill.

It is rebuilt when the reader changes something, and once a second besides
(`STATUS_REFRESH_INTERVAL`): the counters move on their own, and a person watching a stroke should see
them move. The pill's copy button puts the whole line on the clipboard, and the same line is appended
to `status.log` beside the notes — `~/Library/Application Support/cheap-note/status.log` on macOS,
`%LOCALAPPDATA%\cheap-note\status.log` on Windows — with a UTC timestamp and a session header. A file
past 8 MB is rotated to `status.log.1` on the next launch; a log that cannot be opened says `log: off`
in the line it was meant to record.

It wraps, and is allowed 60% of the window's width (`STATUS_PILL_WIDTH`), because of what the line
carries: the document and the page, the ink's counts, the page cache's ratio, the zoom and the frame's
own timings. Set on one nowrap line of 460 pixels — what this pill used to do — the timings were the
part that fell off the end, and the timings are the whole reason to read the line while something is
being measured. And it carries only what **both platforms can say**: a capture's own report (how it is
attached, how coarsely it batches readings, which axes the digitizer has) and the pen's lean are
diagnostic, and they are read under `CHEAP_NOTE_TRACE` instead, which is what keeps the line the same
sentence on Windows and on the Mac. It floats `STATUS_LIFT` (40 px) above the desk's bottom row — the
page pill's height plus a margin — because a readout drawn over the control a person is reaching for is
worse than one a row higher, and it grows *upward* from the corner it is anchored to rather than down
off the window.

**It is on a one-second clock for the counters, and on the change for the rest.** The line is rebuilt
by the change that made it stale — a key, a tool, a page, a session started or stopped — and once a
second while a note is in front, because the counters move on their own. A line that moves on every
frame is a line nobody can read, and re-shaping and re-laying-out text on every frame is exactly what
made the top of the window look like it was flickering while writing. What needs a clock to be
readable is a **session**, which a person starts and stops by hand: `Ctrl+M` for both, because a
session is a stopwatch and starting it and stopping it are the same gesture a moment apart. While it
runs it says only *that* it is running; what it measures — the wait between frames and the pen's own
wait — is read once, afterwards. An error is the one thing allowed to force the line to be rebuilt at
once, rather than waiting for the next second.

### Turning the bar off

The bar can be switched off entirely, and the line with it, because all of it floats over the canvas
rather than taking part in the layout. **A bar that can be hidden must never be hidden permanently**: the
switch that brings it back lives *inside* the bar, so a handle is drawn over the desk whenever the bar is
away — the same pill the page sits in, with one button in it, at the top right of the window.

Zoom does *not* hide with the bar, and it is the one control that must not: the way back lives in the
bar, so a sheet zoomed into a corner of itself with the bar away would have no way out. It is *reading*
rather than writing — it changes nothing about the note — which is what the page pill and the counters
have in common with it, and why all three stay.

## 4. The desk's own row

Floating pills rather than one bar, because they are read at different distances: the page number is
reached for constantly and sits in the middle of the desk where the hand already is, the zoom is reached
for when the page's own shape is in the way and sits at the edge that hand falls to, and the counters are
glanced at and stay out of the way at the other edge. The page is centred by *this* row and the rest are
taken out of the flow, so a counter growing by a digit cannot shove the page off centre — a page number
that moved whenever a number changed would be worse than no page number.

The zoom pill holds two steps, the zoom itself, the two fits, and a lock. The zoom sits between the steps and is a
**field rather than a readout**: a person's zoom is often a number they already have in mind — the size they
read at, the number the page was laid out at — and stepping to 60% is a dozen presses, so a number typed
into it is taken on Enter and understood as a percentage ("60" and "60%" are the same answer). It is given a
fixed width so that stepping from 99% to 100% to 101% does not shuffle the buttons either side of it, and it
shows the *view's* zoom at every frame (`sync_choosers`), whether the view changed by a press, a pinch, a
pinch, a fit or a note being opened — a field that showed the number typed last would be showing a zoom the
sheet is not drawn at. A click away puts the view's own number back rather than leaving a half-typed one, and
a number the view will not take (nonsense, zero, or something outside its range) is refused out loud rather
than snapped to the nearest limit. Zoom lives here rather than on the bar because it is reading, not writing,
and the pill it sits in is the one the page commands left behind.

**The lock holds the gestures off, and only the gestures** (`settings::Settings::zoom_locked`). A reader who
has set a page to 60% and is writing on it wants it to stay at 60%: a palm settling onto a trackpad and a
settling onto a trackpad and a stray pinch all move the zoom without being meant, and one of them landing
mid-sentence takes the line being written away from the nib. Locked, a pinch, the right
stick and Fit Width/Height do nothing — the fit says so on the status line rather than failing
silently — while the two ways a zoom is
*asked for*, the steps and the typed number, still work: a lock that stood in their way would be a lock with
no key. It sits at the end of the pill, after a hairline, because it is not another way of moving the zoom but
a statement about the gestures that do; it is the note's, like every other setting, and it says `(locked)` on
the status line so that a Fit which did nothing reads as the lock rather than as a bug.

The page commands — insert a page before or after this one, delete this one, and mark it — lead the
bar's second row and stand bare on it, rather than in a pill: in the bar, every group does. They lead
because a page *is* the sheet, and they are the only controls in the app that change how much of it
there is. The bookmark sits with them because it is a fact about *this page* rather than about the view:
the page pill and the zoom pill are how the sheet is read, and a mark is something the page keeps. Its
button says which way round the page is — filled for a marked page — and the list of what is marked is
the button beside it, because the two are the same subject. The document's own contents stand in their
own group: a different subject again — the marks belong to the reader, the contents to the document they
annotate — and it is disabled for a document that has none, which is most of them.

## 5. The pen's rules

The pen is captured by the **window**, not by anything on it — `pen-windows` delivers a raw
`WM_POINTER` stream — so nothing in the drawing layer can stand between the pen and the page, and the
app refuses the pen itself. There are exactly three refusals, and all of them are app *state*:

* **A line**: above `bar_edge` a reading is a press on a control rather than ink (§1).
* **A screen**: while the home list, the marked pages or the contents are in front, and while a name is
  being typed, every reading is dropped where it would otherwise become a stroke. A name being typed
  must not leave a line across the page behind the field, and a screen is drawn over a sheet nobody can
  see — a stroke laid there would go into a note the user is not looking at.
* **The palette at the pen** (§6): while it is up the nib lays nothing, and a reading that lands near one
  of its colours is a *choice* rather than a stroke. This is the one control a pen is aimed at instead of
  written on, and both halves of that are the same state: the `NoteApp::palette` that refuses the ink is
  what reads the tap (`palette_from_pen`), so there is no press for a stroke to arrive on — and, as with
  a screen, no rectangle that could be wrong by a frame.

That second rule is *why* the two lists are screens rather than panels floating on the sheet: a panel
would have had to add a rectangle to the ink rule, and a rectangle that is wrong by a frame is ink drawn
under a control.

### The ghost cursor

The pen's tilt is drawn as a ghost — a nib mark with the pen's body leaning away from it — because a
Windows cursor is a fixed bitmap: the system can choose *which* cursor to show, never which angle, and
there is no arrow that means "a pen held at 40 degrees". The system pointer is taken out of the way by
`system_cursor` exactly while the ghost is on screen, so the ghost *is* the cursor rather than a marker
beside one — and there is no switch for it. Turning the ghost off would leave the pen with no cursor of
its own at all, which is a state nobody asked for; what the bar's switches turn off is the bar.

Five facts about the ghost are decided here, and each of them is something a pen reading cannot say:

| Fact | Where it comes from |
|---|---|
| whether the pen has a cursor of its own at all | the **ink model**, not a batch, so the cursor survives the batches that lay nothing — most of them, while the pen is merely held over the window — and disappears only when the pen does |
| where it is and how it leans | the pen's newest reading, in window coordinates, with no sign flip: lean right and the ghost extends right of the nib |
| where it is *not* drawn | **only below the bar**: over the bar it would be drawn behind an opaque background, where it cannot be seen — and **never on the home screen**, which has no sheet to point at and is meant to be tapped, **nor under a control that hangs over the page** (a chooser's list, the palette at the pen) |
| the scale, the colours on this paper, and where the bar ends | published to the overlay (`publish_screen`) on the frames where they change, so a new pen colour or paper takes hold at once rather than at the next reading |
| how **thick** the mark is | the same publication (`Screen::nib_radius`): half the width the tool in hand lays, which only the app knows — the pen's weight is a setting, and the marker's band is the tool's |

**The nib's dot is the colour in hand, and it is as wide as the line in hand.** The mark is drawn in the ink the next
stroke will be written in — the pen's colour, or the marker's — because that is what a reader looks at the dot to know,
and the palette in the bar changes with the tool, so the two always say the same thing. For the same reason it is drawn
as wide as the line that stroke will be (`cursor::nib_radius_for_width`): half the width the tool in hand lays, so
`Fine` and `Heavy` are two different dots, and a highlighter's mark is as wide as the band it is about to lay. The
width is the pen's *widest* press and not the pressure of the moment — the weight is a setting and a pressure is a
measurement, and a dot that grew and shrank as the hand pressed and eased would never mean the same thing twice — which
makes this the one place the pen's weight can be *seen* without putting ink on the page. The soft edge under the mark
and the nib's bloom are drawn in the paper's **contrast colour** instead, which is what keeps a mark findable when it is
close to the colour of the paper: white ink on white paper reads as a white dot with a dark rim rather than as nothing
at all. The three facts reach the overlay as a set (`cursor_overlay::Screen::colour`, `Screen::halo` and
`Screen::nib_radius`), and the eraser and the lasso — which write nothing — keep the contrast colour and the pen's own
width, because it is the pen whose weight the reader set.

**The body casts a shadow.** A leaning pen draws a rod, and a rod of a faint colour needs more than an edge when the
paper is nearly the colour of the rod: the body is painted *three* times, as the sheet's own shadow is — the same rod
**dropped** a few pixels down and to the right in the contrast colour, then the soft edge around it, then the body
itself, widest and faintest first. The drop is what makes the rod read as lying *above* the page rather than being
painted on it, and it is the layer an eye finds first on a page of writing; the halo alone thickens the body's own
edge, which is no help at all when that edge and the paper are the same colour.

The system pointer is driven **from the ghost** and not from the pen's range, because the two states
have to be impossible to separate: a hidden pointer with nothing drawn in its place is a window with no
cursor at all — and on a list, that is a list no pen can click.

**A control that hangs over the page takes the ghost away.** The bar is one line (`bar_edge`): above it is
interface, below it is page. A control that opens *below* itself breaks that line — a chooser's list, and the
palette at the pen — because the place the reader is aiming at reads as page while the thing they are aiming
at is a control. Both of them therefore suppress the ghost while they are up, and the system pointer is the
cursor over them. The list is the sharper case of the two: it is a list of *options*, meant to be pressed one
by one, and a list with no pointer on it cannot be pressed at all.

## 6. The keyboard

Undo and redo are **actions** — GPUI's unit of a keyboard command — rather than key listeners, so a
*binding* decides which keys mean them (`Ctrl+Z`, `Ctrl+Y`, `Ctrl+Shift+Z`: Windows applications reach
redo with the first and everything else with the second, and there is no text field anywhere in this app
for either to collide with). The bar's buttons and the keyboard both end in the same two methods, so
there is one implementation of each and no way for the two paths to disagree. It matters more here than
it would in a text editor: the bar can be hidden, and without a keyboard command an app with the bar off
would have no way to take a stroke back at all.

Everything else a key does is registered by **the screen that is showing**, as a listener on the element
it paints, so a key reaches a window with nothing focused — and so a *character* reaches a list's filter
without a text field to type it into.

The table below writes its chords the way Windows writes them. On a Mac the command key replaces `Ctrl`
and the app says so the Mac way — `⌘Z`, `⇧⌘B`, `⌥⇧⌘R` — because the bindings are on GPUI's *secondary*
modifier (`Ctrl` on Windows, `⌘` on macOS) and because the interface's own labels are computed from the
chord instead of typed into a string (`crates/note-ui/src/keys.rs`). Nothing in the view knows which
platform it is running on: `gpui` answers both questions.

| Keys | What they do |
|---|---|
| `Escape` | leaves the note for the home list — and while a name is being typed, closes the field instead: an open field that `Escape` did not close would leave a person typing into a box they cannot leave |
| `←` `→` | the previous and next page — the same two commands the page pill's buttons are. Not while a name is being typed: there the arrows belong to the field, and a caret that turned the page would be a trap |
| `Ctrl+B` / `Ctrl+Shift+B` | mark the page in front / open the list of what is marked: the two halves of one idea, which is why they are the two halves of one key |
| `Ctrl+↑` / `Ctrl+↓` | the previous and next *mark*, without opening the list — which is the point of them: one keystroke per mark, for a reader hopping between the pages they use |
| `Ctrl+O` / `Ctrl+Shift+O` | open a PDF / the document's own contents, on the letter that already means "open a file", because a table of contents is what a document is opened *by* |
| `Ctrl+N` | a new blank sheet |
| `Ctrl+S` | `Save`, which writes the file a person carries rather than one more write into a note that is already written (§10) |
| `Ctrl+M` | the measurement session, on the letter with nothing else to mean |
| `Ctrl+Shift+C` | the palette at the pen: it opens under the nib, and the same chord puts it away — `C` for colour, on the chord every drawing app spells it with (§6, below) |
| `Ctrl+\` | show or hide the bar: not a letter, because every letter worth having is taken (`B` marks, `R` turns, `O` opens, `S` saves, `M` measures) and `T` is `Ctrl+Shift+T` in every browser. `Ctrl+\` is already the panel toggle in Xcode, VS Code, Photoshop and Illustrator, so it is the one key that needs no teaching — and it is the same key on the Mac, `⌘\` |

**The palette at the pen** is an overlay that takes the keyboard the way a *box* does rather than the way
a screen does: while it is up the arrows walk its ring, `Enter` takes the colour the ring marks, `Escape`
puts it away, and nothing else a key means reaches the sheet or the bar under it (`NoteApp::note_key_down`,
`NoteApp::palette_take`). Why it is shaped the way it is comes down to five decisions:

* **It opens where the *pen* is**, which the app knows to the last reading (`last_cursor`), so the palette
  arrives under the nib — where the hand already is — rather than at the pointer, which is the second
  answer, or the middle of the window, the third. It is then *clamped onto the screen* (`palette_centre`),
  because the pen is at the window's edge as often as it is in the middle, and a ring half off the window
  is a ring with half its colours.
* **It does not follow the pen.** A ring that moved with the hand is a ring the hand cannot aim at, and the
  pen keeps reporting while the palette is up: the position is fixed where it opened.
* **The ring is the palette of the tool in hand** (`palette_colours`): the marker's four colours replace
  the pen's twelve without the palette being rebuilt, which is the same rule the bar's Ink row follows —
  and it is why taking a colour from behind the eraser puts the pen back in hand, since a colour is what a
  pen writes with.
* **A tap is answered by distance**, not by the element under the pointer (`palette_at`), because the pen is
  captured by the window and never reaches the element tree at all (§5) — the same reason the ghost cursor
  has a window of its own. One function answers the pen's tap, a mouse's click and the keyboard's own place
  on the ring, so the three cannot disagree. The ring is a clock face — the first colour of a table at
  twelve o'clock, and the table's order going clockwise (`palette_point`) — so a palette is *learnt* rather
  than hunted for, and the gaps between discs are wider than two reaches, which is what makes a ring of
  twelve pressable with a pen (there is a test for both).
* **It is two zones, not thirteen discs on one field.** The colours are on the ring, because a ring is
  what makes them a list; the tools — and the colour in hand — are inside, in a well of the theme's quiet
  grey, so that what the palette *offers* and what it is *set to* are two things to look at rather than
  one field to read. A tool is a chip with its icon in it rather than a bare glyph, which is what the same
  tool is on the bar and for the same reason: an icon alone on a white field is a smudge. The hub is the
  colour the next stroke will be written in, and it is *not* a control, so it is set in a ring of white —
  the one filled circle in the middle that cannot be pressed does not look like the three that can. Its
  measurements are in §12 with the rest of them — and they are the design's, in 96-DPI logical pixels, so
  what is drawn is those numbers times `Platform::unit_scale`: a macOS point is four thirds of one, and
  the same numbers there would be a third too large (`PaletteGeometry`).

The two list screens keep the app's own chords and nothing else: the arrows, Enter and Escape belong to
the list that has the focus, and the same chords work there as on the note, because a person looking at
a list of places is still in the app. The *steps between marks* are deliberately not bound there: the
arrows are the list's own, and two meanings on one key would be one meaning too many.

**One rule hands the focus over** (`claim_sheet_keyboard`): it focuses the sheet on the *single* frame
where the screen in front changes to the note screen. Not every frame — a list that is up needs the
focus for its own arrow keys, and taking it away once per frame would break exactly the screens this
fixes the fallback from. The rule exists because the note screen is the *fallback* screen: nothing hands
it the keyboard, and a window whose focus is left on a screen that has just gone away resolves that focus
to *no node at all*, which leaves the note's keys **swallowed** — no arrows, no `Ctrl+S`, no `Escape`,
from opening a note until something else gave the sheet the keyboard back. Doing it in the one place
every transition passes through (opening a note, closing a list, coming back, closing a name field) is
what stopped every transition having to remember it.

## 7. The sheet, and the page in hand

What the user writes on is described by `canvas.rs`: its size, its colour, and what is printed on it. A
PDF page overrides all three, because a PDF page is its own paper. All of it — the size, the colours, the
ruling, the ink's colour, the pen's weight and the zoom — belongs to the **note** and is kept in it, so
opening a note opens the sheet it was written on, written with the pen it was written with. A blank sheet
that is closed on loses the choice made on it: with no note open there is nowhere to write an answer, and
the alternative — the sheet somebody last chose, handed to a note written on another — is the global
state this design exists to have none of.

A paper size is a *physical* size, so choosing one sets the drawing scale too: a person picking A5
expects half a sheet of A4, not the same sheet under another name.

**The ruling discards itself.** It is keyed on the sheet's rectangle, its style and its paper colour, so
it rebuilds on the next frame without being told. What is left to rebuild is the status line: it is
rebuilt by the change the user just made — they are looking for it — and once a second for the counters
that move on their own (`STATUS_REFRESH_INTERVAL`).

Changing the pen's weight leaves what is already on the page untouched: a stroke keeps the width it was
drawn at, exactly as it keeps its colour, because the width is stamped into every point as the nib moves
(`ink.rs`). So this is what the *next* line is laid with, and choosing a marker on a page of fine writing
changes nothing about the page.

### Turning a page

**A page keeps the place it was being read at**, for as long as the note is open: turning to a page added at the
end of a note opens that page at its own **top**, rather than at whatever part of the page before it the reader had
scrolled to, and turning back to a page one was in the middle of comes back to the middle. The `zoom` is the
*reader's* and follows them from page to page; the pan is the *page's*, and `NoteApp::turn_to` is where one is put
away and the next taken up (a page nobody has looked at has none, and opens at its top). The places are in memory
only — a note opened tomorrow starts its pages at their tops — and they are dropped when the pages are renumbered,
because a place kept for page 4 belongs to page 3 once one in front of it is deleted.

**The tool in hand is the reader's too**, and travels with them for a sharper reason: turning a page is not reaching
for a different pen. A reader who highlights a page and then turns to the next one means to keep the marker, so the
tool is carried *across* the turn rather than handed back by the page being arrived at — the page's own document is
told which tool is in hand as it comes to the front (`ink::Notes::show`, the one path every page turn takes,
including the page read out of the file and the page that follows a deleted one). The split is the same one the pan
and the zoom make: the ink, the selection a lasso has, and the history of a page's edits belong to the **page** and
come back with it; the tool and the zoom belong to the **reader**. What the tool is *not* is a setting — it is not
written into the note and not restored by opening one, so every note starts with the pen in hand (`Tool::Pen`), and
only a page that was written on while the marker was up is a page the marker came from.

The **order** is the whole of it: the page being left is closed first — its outstanding ink is handed to
the writer and folded into chunks, which is the design's "compaction at page close" — and only then is
the page being turned to read, so a page's ink is never in two places at once. The ink moves with the
page it was written on, which is what keeps a note on the sheet it was written on rather than on
whichever sheet is shown next. Nothing waits for any of it: the batch is handed to the writer's thread
and the compaction is queued *behind* it, in that order, so the ink is in the note before the chunks that
follow it.

An **inserted** page is a blank page rather than a copy of its neighbour — it is paper to write on, and
duplicating a page is a different command, which this app does not have — and the app turns to it,
because a page that was just made is the page that is about to be written on: leaving the reader on the
old one would make the button look like it did nothing.

A **deleted** page takes its ink with it: there is nowhere to show it afterwards, and keeping it would
mean keeping an identity for "the page that used to be here" that no later page could be confused with.
On a note about a document the page leaves the *note*, not the file.

### The commands that cannot be taken back

Clearing a page's ink and deleting the page are the two things a note has **no undo for** — the first drops the
history with the strokes, and the second drops the page that history belongs to — so neither acts on the click that
reaches for it. Both put up a box instead: what goes, and two answers, `Clear the page` / `Delete the page` and
`Keep it`. The question names the page in front and says what is lost, because a reader who is asked "are you sure"
learns nothing and clicks yes, while a reader who is asked about *this* page can tell whether that is the page they
meant.

There is no way out of the box that is not one of the two answers: a click outside it and Escape both *refuse*, the
word on the refusing button is "Keep it" rather than "Cancel" — it says what happens to the page — and while a box is
up the pen draws no ghost and lays no ink and the keyboard belongs to the question
(`NoteApp::ask`/`answer`, and `confirmed` is the one place an answer becomes an action).

### The page's rotation

A page's rotation belongs to the **page**, not to the view. It is stored with the page — one field of the
note's own page list, see `meta('layout')` in [STORE.md](STORE.md) — so a page turned and left comes back
turned, and a document's own `/Rotate` is the same fact about the same page. There are four commands
because a page can only be turned in quarter turns: this page clockwise or counter-clockwise, and every
page of the note the same two ways. "Turn the note round" is that second pair and not a rotation of the
window: each page keeps its own turn, so turning the note and then turning one page back leaves that page
where the reader put it.

**The ink does not move.** A stroke is stored in the page's own coordinates, and a turn changes only
*where those coordinates are drawn*: the pen's reading is turned back through the same mapping the layer
draws the ink forward through — `ink::paper_of_drawn` and `ink::drawn_of_paper`, which a test holds
together — so writing on a page the reader turned is stored the way it will be read, and turning the page
back turns the writing with it. A turn is therefore free: no stroke is rewritten, no chunk is re-encoded,
and the only row that changes is the page list's.

Two rectangles are in play, and for a quarter turn they are each other's transpose: the **paper**'s own
rectangle, which is what the ink is measured in and what a reading is checked against, and the rectangle
the sheet is **drawn** in. The ruling is built on the first and placed in the second, so a page on its
side has its lines running the other way — paper that was turned, rather than a grid painted over it. A
reading is likewise checked against the paper and not the drawn rectangle: a page on its side has a strip
of desk beside it that must not write.

The interface offers the four as two pairs of buttons, and the page in front's two also answer to
`Ctrl+R` and `Ctrl+Shift+R` (with `Alt` for every page). The status line says which way up the page is
while it is not the right way up, and says nothing when it is.

### The lasso

The third tool neither adds ink nor removes it: it takes ink **in hand**. A loop is swept around the writing
the reader wants, and what it encloses becomes the page's **selection** — which can then be dragged somewhere
by pressing on the ink itself, or removed with Delete (and a button on the bar, because the hand holding the
pen has no Delete key). Escape puts the selection down, and only then does Escape leave the note.

The gestures are the ones a notebook has, and they are decided at the moment the nib goes down: a press *on*
the selected ink takes hold of it; a press anywhere else starts a new loop, which is also how a selection is
let go of — a reader pressing beside the ink is drawing a region, not asking for the old one. What the loop
encloses is decided by the even-odd rule on the loop's own points, and the ink is taken a **whole stroke** at
a time: a stroke the loop merely crosses is taken if any of its points is inside, which is the trade-off the
eraser makes too, and never a fragment that nothing could pick up again.

**A selection belongs to its page**, like the ink does: turn away and back and the same strokes are still in
hand. It is the page's own idea of what is in hand — the note has never heard of it, and nothing about it is
written to the file.

**The ink does not move while it is being dragged.** A drag is one offset in the canvas description — the same
idea as a page's rotation one level down — so dragging a page of handwriting costs the same as dragging one
stroke, and a drag that is put down where it started costs nothing at all. The points move once, when the
reader lifts, and the derived geometry with them, because a stroke's bounds and outline are what the eraser
hit-tests and what a frame culls by.

The one thing this costs the file is a second number. A note writes a page out again when an append cannot
express what happened to it, and it tells that by *counting* strokes: an undo, an erase, or a deleted
selection leaves fewer than the note holds. A **move** leaves the count exactly as it was and every stroke
changed, so the page counts its shifts as well (see [STORE.md](STORE.md), §6). Moving and deleting are edits
like any other, so both can be taken back — and a move leaves the selection in hand as well, so it can be
dragged back by hand.

Showing it: the ink in hand is drawn once more in the theme's accent, translucent, and the loop being swept is
drawn as the ring it is — thin, over the ink, so the writing can be read through it while it is being chosen.

### The highlighter

The marker is the pen's *sibling* rather than a fourth kind of gesture: it lays a **band** where the pen lays a
line. Two numbers make it that — one fixed width, and a colour with an alpha in it — and both are stamped into the
stroke when the nib goes down, exactly as the pen's colour and weight are. Nothing else in the app knows what a
highlighter is: a highlight *is* an ordinary stroke, so it can be erased, taken in a lasso's loop, dragged, undone,
and stored with the page, and no part of the file format had to change for it.

Its colours are a palette of their own, and the bar's ink row follows the tool: four pale ones, because a band is
read *through* — a pen's black at the same alpha would be a grey smear over the page. A band does not respond to
pressure at all (a marker has one tip, and a translucent band that thinned under a light touch would show the
writing through in stripes), and the eraser takes it like any other ink.

**The band is freehand, always.** It follows the nib exactly as a line does, over a document's text as much as over
a blank sheet: what the hand draws is what is laid down, with no snapping to characters and no line-by-line
banding. A PDF's text is the document's, not the note's, and a marker that rearranged itself around it would be
describing the file rather than making a mark on the page.

**And it is laid once.** The band is one polygon — the stroke's own outline, filled by the nonzero rule — not a
chain of overlapping pieces. That distinction is the difference between a 35% band and a solid one: alpha
composes, so ink drawn as hundreds of overlapping segments and discs came out 91-100% opaque, and unevenly enough
that a *short* mark looked broken where a long one merely looked dark. Both renderers now fill the outline, which
is what makes a highlight over one word the same colour as a highlight across a paragraph
([ARCHITECTURE.md](ARCHITECTURE.md), invariant 17).

**The ink is the union of the nib's discs.** One sentence, and the whole of the model: a stroke is the set of
places the nib covered, so a place is ink exactly when it is within the nib's radius of any reading. Everything
that used to be a special case follows from it without being mentioned. A pen held down and turned lays a dot,
not a ring, because every disc is at the same place. A stroke that doubles back is still one region, because a
union has no order. An end is round, because the disc at the end is round. A corner is filled, because the disc at
the corner reaches past it (`ink::Model`, and `a_pen_turned_on_the_spot_fills_a_disc` in
`apps/macos/src/platform/metal.rs`).

**And its boundary is drawn from the discs, not from a normal.** The outline the renderer fills is the edge of
that union: between two readings it is the common *outer* tangent, and round each reading it is the arc no other
disc covers — a half circle at an end, the outside of the turn at a corner, and nearly a whole circle where the
pen turned round and came back. That is one rule for every shape a hand makes
(`ink::ribbon_outline`, and the `fill_tests` scenarios in `apps/macos/src/platform/metal.rs`).

**What this replaced, and why it kept breaking.** The outline used to be two *offset edges*, each point pushed
along the normal through its neighbours. That construction has a normal to keep in step with its neighbours at
every reading, and a turn is exactly what flips it. Each of the four bugs fixed before this model was a symptom
of it: an annulus for a turned pen, a slit through a stroke that met itself, a half-missing end where a stroke
rolled in, and one unpainted reading at every sharp corner — the last of which survived every attempt to patch the
normal. A union of discs has no edges to keep in step, so none of those cases exists to be got wrong.

### A mark

A **mark** is one page of the note and nothing else — no name, no colour, no text — so it is a page
rather than a moment: inserting a page before a marked one renames the mark along with the page it is on,
and deleting a page takes its mark with it, in the same transaction. It is written where it is made
rather than on the writer's thread, because a mark is the one thing the app stores that is neither ink
nor a setting: a single `INSERT` or `DELETE` in a file that is already open. Its list is always the
note's reading order, so a list cannot disagree with the pages it lists. And the command that takes a
mark off is not a toggle: the menu says "take the bookmark off", and a command that says what it does has
to do that rather than the opposite of what it finds there.

**Undo and redo** are offered by the bar only when the page's history says they would do something — a
button that is always there and sometimes does nothing is the one thing a user cannot tell apart from a
broken command — and the bar asks the same question the command will, so an edit is only ever taken back
by a command that said it could. What can be taken back is **everything a command does**: a stroke written,
the eraser's sweep, a selection deleted, a page cleared, a lasso's drag. There are no exceptions left to
remember, because an edit *is* the record of what was done (`history::Edit`): undo is not "give me the last
stroke back", it is "put back what I did".

**A page's history outlives the session.** The edits are written down beside the ink, in the same
transaction, so a note opened on Friday still has a way back through what was written on Monday — up to the
note's own depth of them (4096, cut back by the idle housekeeping, see [STORE.md](STORE.md), §3). The
session keeps a few hundred in memory, and an undo that runs out of *those* reads one more out of the note
rather than stopping: the status line says how far back the page can go (`history 12 back, 3 forward`), and
the reader never meets the seam between the two. An undo is written to the note at once rather than on the
batch clock: it is a deliberate act, and leaving it in memory for half a second is how it is lost if the app
is closed in that half-second.

## 8. The document

A page's bitmap width is **quantised onto a ladder** (`PDF_PIXEL_WIDTHS`: 1024, 1536, 2304, 3456 px),
because a page is rasterised whenever the requested width *changes*, and rasterising is the one part of a
frame that costs milliseconds — measured on a blank A4 page: 1.4 ms at 720 px wide, 4.7 at 1440, 16.9 at
2880, 66.5 at 5760, with the bitmap itself reaching 179 MB at the last of those. A zoom gesture asks for a
new width on *every event*, so an exact width means a pinch across a PDF re-rasterises the page hundreds
of times and never finishes a frame. Quantising turns that into a handful of rasterisations per gesture,
and nothing needs to be exact: GPUI scales the bitmap to the bounds it is painted into, so a page rendered
at a width the reader is not quite at is simply slightly softer.

The rungs grow **geometrically** — half again as wide each time — because the cost is quadratic in the
width: memory and time each grow by about two and a quarter per rung. Four rungs cover 5% to 1600% of
zoom, every zoom the app allows, with the worst rung at 67 MB and about 27 ms. The top rung is where it
stops growing: past it a page is magnified rather than resolved, which is the trade-off every viewer makes
when it stops rendering at the image's own resolution. A request lands on the *smallest* rung that covers
it — a page rendered a little too large is sharp, and one rendered a little too small is soft, and only
one of those is visible.

Rendering happens at `PDF_RENDER_SCALE` (twice the logical width), which keeps page text crisp when the
window is scaled and on a high-DPI panel, and is paid once per page rather than once per frame.

**The cache key is what the bitmap depends on**: the page, the rung, and whether it is grayscale. The
grayscale switch therefore invalidates nothing by hand — the bitmaps it changed are simply not the bitmaps
the cache holds, and the next frame asks for the ones it now wants.

**The render happens in the housekeeping pump, not in the frame.** A rasterisation blocks this thread
either way, but the pump is *between* frames rather than inside one, so the cost lands on how soon the
next reading is consumed instead of on whether a frame is drawn at all. The slice budget keeps even that
small: Pdfium stops when the budget is spent (`pdf::SLICE_BUDGET`), and the next wake carries on where it
left off. It waits for the pen to stop by `PDF_QUIET_INTERVAL` (120 ms), which is the trade a person
feels: while a stroke is being laid, the rung already on screen is the right one, and an eighth of a
second is short enough that a person pausing to think sees the sharp one land, and long enough that the
pause between two letters of a word is not mistaken for one.

The **first frame of a document** does not use the ladder's answer: the cheapest rung is rendered there at
once, because it costs about a millisecond and showing nothing at all is worse. What the zoom actually
asked for, and could not have yet, becomes a *pending* request that the pump pays for when the pen is
quiet.

**Cancellation has two halves**, and neither needs a flag. A request the view has moved on from *before*
its render started is replaced when the next one is planned — the replacement is what the counter counts.
A render *already in flight* is abandoned by the pump, and dropping the job is what releases the page and
the unfinished bitmap.

## 9. Gestures

The trackpad is the only pointer this app takes a scroll from. A two-finger drag pans the sheet, and a pinch
zooms about the point between the fingers, with the sheet moving the way the *view* was pushed. A mouse
wheel is not read at all — not the scroll, not a `⌘`-held roll — because a wheel reports lines and a trackpad
reports pixels, and the conversion between them was a guess about a system setting this app has no API for.
Pans and pinches both report pixels, which are the units the view already pans in, so nothing is converted.

Ignoring the wheel is a deliberate loss of support rather than an oversight: a line-based delta is dropped at
`NoteApp::on_scroll`, so a plugged-in mouse pans nothing and zooms nothing. The advantage is that there is one
zoom gesture and one pan gesture, with no chord that turns one into the other.

That trade was made on a Mac, where a trackpad is part of the machine, and it reads differently on Windows,
where a desktop usually has a wheel and no precision trackpad — so the same rule means "no scrolling with the
mouse at all" there. It is left as it is knowingly rather than overlooked; [WINDOWS.md §3.2](WINDOWS.md#32-a-mouse-wheel-pans-nothing)
sets out the three ways it could be revisited and what each costs.


**A swipe that began at either side turns the page.** Rightwards from the left edge is the previous page,
leftwards from the right edge is the next, which is the direction a page of a book moves when it is turned. The
gesture is followed across readings rather than decided on one (`EdgeSwipe`), because a trackpad sends a swipe as
dozens of small deltas and none of them alone says where the hand is going. Three rules keep it from firing by
accident:

- **The band is where the gesture began** (`EDGE_SWIPE_BAND`, 120 logical px). A trackpad reports the position a
  gesture *started* at with every reading of it, so the band is answered once per gesture. A scroll begun over the
  page begins outside it, and the paper fills the window. (That parenthetical is macOS' behaviour, not every
  platform's: Windows reports the *live cursor* per reading, which is why this gesture is not reliable there —
  see [WINDOWS.md §3.1](WINDOWS.md#31-an-edge-swipe-never-turns-a-page).)
- **The travel is signed and accumulated** (`EDGE_SWIPE_TRAVEL`, 80 logical px), and it has to be *away from* the
  edge that began it. A gesture that drifts out and comes back adds up to nothing, and a swipe pushing into the
  page from the rim is a pan, because the sign says so.
- **It must be more sideways than vertical** (`EDGE_SWIPE_SLOPE`, two to one). A reader scrolling down the page
  with a thumb near the edge is mostly vertical with a little sideways drift; a diagonal flick still counts.

While a swipe is in flight it does **not** pan — a page turn that dragged the sheet sideways first would be two
motions for one intention — and it turns **one** page. `turned` is never cleared while the gesture lasts, so a fast
flick that keeps sending readings after the page turns does not run through four pages. The gesture ends on the
same quiet gap a pinch ends on (`GESTURE_SETTLE_INTERVAL`, read by the pump), and a swipe that ended without
turning anything is simply forgotten.

**The pen's own two gestures take the nib off the paper.** With **space held**, a nib that is down moves the
sheet instead of writing on it: the paper follows the hand exactly, and either the pen or the key may end the
gesture. With **option held** (`⌥` on macOS, `Alt` elsewhere), the nib's vertical travel zooms — up is closer,
down is further away — measured from where the nib went down, so a hand that wanders and comes back returns the
sheet to the zoom it started at. Space wins when both are held, because a hand that is panning may rest on
option without meaning to zoom.

**Both gestures are absolute, not cumulative.** Each reading places the sheet where the hand is, rather than
moving it by the distance since the reading before: a pan is set from the pan the gesture began at, and a zoom
from the zoom it began at. The view accumulates (`Viewport::pan_by`, `Viewport::zoom_by`), so adding a whole
travel on every reading would add it again and again — a page that accelerates under a steady hand and drifts
while the nib is still. Set, the page follows the hand exactly and comes back when the hand does.

**A nib that moves the sheet moves it twice as far** (`PEN_PAN_GAIN`, 2.0). A hand has only one screen's
travel in it, and the sheet is often longer than that; a 1:1 pan would make a long page unreachable without
lifting the pen and placing it again. The direction is unchanged — the paper follows the hand, so a nib going
right still shows what is further left.

**The pan is the hand's number, and the clamp is the drawing's.** A gesture stores where the hand put the
sheet, out of range included; `Viewport::origin` is what refuses to draw past the layout's edge. Holding the
stored pan inside its bounds on every reading instead — which the app did — looks harmless and is not: a clamp
throws away where the hand is, and no later reading can bring it back, because each one is measured from the
gesture's origin rather than from the stored value. A hand that took the sheet past its edge and came back would
watch the paper sit still through the whole return and then jump; measured at **500 px of travel with nothing
moving**, on a sheet drawn twice the window's height. The gesture settles its pan inside the bounds **once, at
the end** (`NoteApp::settle_pan`), so the scroll's rule still holds — the end of a sheet is an end — without the
paper detaching from the hand while it is moving.

That is not the same rule as the scroll's, and the two must not be confused. A scroll is a *sequence of steps*, so
each step starts from where the last left the sheet, and a scroll past the end would otherwise build an invisible
debt (`Viewport::hold_pan`). A nib is an *absolute position*, so there is no step to unwind: the fix for the scroll
is exactly the bug for the pen.

**Both gestures are taken from the hand's settled position, not from the raw reading** (`PenJitter`). A pen's
digitizer never reports a still hand as still: the nib wanders by a fraction of a pixel between readings, and a
machine with a spinning fan shakes the whole tablet, which the pen reports as movement. Both gestures make that
worse — a pan multiplies it by `PEN_PAN_GAIN`, and a zoom turns it into a change of the sheet's size — so a page
held still visibly shivers and a zoom breathes. The correction is a threshold on the reading
(`PEN_JITTER_SPAN`, 0.5 px): a reading within that distance of where the hand has settled is the tremor and
answers the settled point, and one beyond it is the hand and is answered *itself*, at once.

It is deliberately **not a low-pass filter**. Averaging the last readings would smooth the tremor and lag the nib
by however many were averaged, and a nib that trails the hand is the one thing a pen cannot be; the threshold
costs no latency on the readings that mean something. The settle point moves with the hand rather than staying at
the gesture's origin, so a slow drift measured in sub-span steps is still followed within one span of it — a
filter that kept the origin would hold a creeping hand forever. It touches **gestures only**: writing is not
filtered at all, because a stroke is the reader's own line and the tremor is the hardware's. `Settings::pen_jitter`
is **on by default** — the noise is not something a reader put there — and is read when the nib goes down, so
throwing the switch mid-gesture cannot jump the sheet.

**The zoom's rate does not depend on the zoom.** A doubling takes `PEN_ZOOM_TRAVEL` — 200 logical pixels of
vertical travel — whether the sheet is at 25% or at 800%. That is the rule a reader has to learn, and it is one
sentence: this much travel is this much zoom. A pinch is not that rule and does not need to be: it reports the
change its own fingers made, as a factor, so there is no travel to measure and no zoom to scale by. A nib is not
a factor either, because the hand knows exactly how far it moved.

**A nib in the air is not a nib on the paper.** A digitizer reports a hovering pen, and hovering is where the
ghost cursor is drawn and nothing else: with a key held, a hand resting over the display does not move the
sheet. Only `Down` and `Move` are contact — `Hover`, `Enter`, `Leave`, `Idle`, `Up` and `Cancel` are all air
(see `touches_paper`).

Either key ends its gesture when it is let go, and the nib goes back to writing from wherever it is: neither
the pen nor the key has to be lifted and pressed again. A gesture never opens a stroke, so the ink already on
the paper is never disturbed, and a gesture that begins over the bar is not a gesture at all — the bar is where
presses belong to controls.

The keys are read from the operating system rather than from the window's keyboard (`device_query`), because
the rule is *"this key is down while the nib is on the paper"*, and the nib is on the paper for as long as the
hand is writing — which is not the same as this window having focus. It is a query, not a listener: no thread,
no event loop, no system-wide tap.

The numbers behind that are all *derived* rather than picked:

| Number | Value | Where it comes from |
|---|---|---|
| `EDGE_SWIPE_BAND` | 120 logical px | how far in from a side a gesture may have begun and still count as an edge swipe. Wide enough to catch a swipe from the rim without aiming, narrow enough that a scroll begun over the page never lands in it |
| `EDGE_SWIPE_TRAVEL` | 80 logical px | the sideways travel a swipe needs, about two centimetres: more than a resting hand drifts and less than a deliberate swipe. Signed and accumulated, so out-and-back is not one |
| `EDGE_SWIPE_SLOPE` | 2.0 | how much more sideways than vertical a gesture must be. A thumb scrolling near the edge is mostly vertical; a diagonal flick still counts |
| `PEN_ZOOM_TRAVEL` | 200 px | the nib's travel for a doubling, at any zoom: a little over half a finger's movement, and shorter than a pair of fingers needs to pinch — a hand on a pen has finer control than two fingers |
| `PEN_PAN_GAIN` | 2.0 | how far the sheet moves per pixel the nib travels: a hand has one screen's travel in it and a page is often longer, so 1:1 would make a long page unreachable without lifting the pen |
| `PEN_JITTER_SPAN` | 0.5 px | what a gesture treats as the hand moving: readings under it are the digitizer's own tremor (a resting hand, a spinning fan) and are ignored, so a page held still does not shiver. Chosen against the hardware rather than estimated: two tenths still shivered under a spinning fan, and half a pixel is below what a hand can aim at, so nothing meant is lost. A threshold rather than an average, so there is no lag |
| pan settle | once, at gesture end | when a pen pan is clamped inside the layout's bounds. Never mid-gesture: a clamp discards where the hand is and no later reading can recover it, because each is measured from the gesture's origin — measured at 500 px of hand travel with the paper motionless (§9). The scroll's rule is the opposite one, and rightly: a scroll accumulates steps, a nib names a place |

A pinch's delta is already a fraction of the current size, so its factor is one more than the delta — clamped as a
*factor* rather than as a fraction, so a nonsense delta cannot invert the sheet — and a pinch *replaces* the pan
offset with its anchor rather than adding to it, so a zoom never drifts away from the point under the fingers.

## 10. The note in hand

The app starts on a blank sheet, which is not a note yet — a note is a folder with a database in it, and
there is no reason to make one for a session that never writes anything. **The first stroke is what makes
one**, and from then on the ink is written as it is laid, so nothing is ever held only in memory and `Save`
writes out a file that has been the note all along. A note made this way has no document, which is what a
note written on a blank sheet is, and its folder is named after the moment it was made, so two blank-sheet
notes in one session are two notes rather than one overwriting the other.

The note that is made takes over what is in hand: the page list, the sheet the ink is written on, every
setting, and which page is open — plus the marks that were made on the sheet *before* there was a note to
keep them in, since a blank sheet exists only once something is written on it and a mark is one of the
things that makes it exist. Reading a note's settings fills in only what the note actually says, so a note
that has never been told anything keeps the sheet and the pen already up — which is how a blank page, or a
PDF just placed, continues what is in hand instead of snapping back to the shipped set. The whole set is
then written straight back, so from that moment the answers travel with *that note* and not with whoever
was written in last. A choice made on a blank sheet and then thrown away is gone: there is nowhere to have
written it.

**Opening** prefers the *folder* to the file: the note that already exists is opened, and a file is only
placed when there is no note for it. Placing a file again is how a note is lost — what was written in the
old note is not in the file — so a note whose folder has gone but whose file is still there is placed
afresh, and the row says so out loud.

**Naming** belongs to the note, not to the list: the rename is written into the note first, and the list is
told what the note answered — never the other way round, because the index is a cache of what the notes
say, and a cache that runs ahead of what it describes is how a name goes missing from a note that has it.
What the list is told is what the *note* says, not what the list would show for it: an unnamed note is
stored as unnamed, and the list derives the words at the moment it draws them. Storing the derived words
would freeze them — a row that says "Blank sheet" because that is what it was called the day it was made,
rather than because that is what it is. The name goes with the note everywhere: the bar's title, the
window's title and what `Save` names its file all come from one string, and a blank sheet still saying the
name of the note that was open would be a sheet named after a document it does not have — and, on its first
stroke, a new note filed under that name.

**Forgetting** takes an entry out of the list and touches no note: the message says where the note still
is, because "forgot" and "deleted" are one keystroke apart in meaning and the app means the first.

**`Save` is an export.** The note is already saved — it is a folder the app writes into as the pen moves —
so the button writes the single file a person carries, which is why it asks where *every* time and why the
suggestion is the note's own name turned into a file name: what Windows refuses becomes a dash, a run of
whitespace becomes one space, and what a file system would quietly drop is dropped *visibly*.

## 11. Painting the canvas

A frame's canvas is *described* — `describe_canvas` — and the layer draws it, because the shadow's steps,
the paper's colour and the sheet's geometry are this app's design while the layer knows how to put
rectangles on a surface and nothing about what a page looks like. Both renderers are handed the same
description, which is what keeps them from drifting apart.

In the order painted: the desk, the page's shadow, the sheet, what is printed on the paper, the document's
page, and the ink. All of it in **logical window pixels**, with the display's scale travelling alongside
rather than multiplied into every rectangle: the layer applies it once, as the surface's transform, which
is also where the ink's own coordinates are scaled — one multiplication per draw instead of one per
rectangle, and one place to get it right.

**The shadow is three translucent rectangles**, each a little wider than the last and a little fainter,
painted in that order, so they accumulate into one soft edge. The renderer's own shadows are for
*elements*: they cost a layer and are painted behind an element's own background, which a rectangle on the
desk does not have — and a page is drawn by the canvas layer, so its shadow is three rectangles in the
canvas it is handed. Three steps read as one blurred edge at the sizes a page is drawn at. The sheet is
both lifted *and* offset downward, the way a sheet of paper lies on a desk: a shadow centred on the paper
reads as a glow, and one that is only offset reads as a hard edge.

The ink in the description is the finished strokes as one `Arc` — a pointer copy per draw rather than a copy
of every stroke — plus the *open* stroke, closed live at the current zoom, and a `revision` number that
counts the rebuilds a zoom's change of detail forced (see `ink_layer/render.rs`).

### What a frame actually costs, and what it does not

Measured on a 25-page, 8,182-stroke note at 2560×1600 on a 144 Hz display, with `CHEAP_NOTE_TRACE=1`: **143
to 146 draws a second under continuous load**, with the ink at 350,000–400,000 vertices and one rebuild a
second. That is the number to hold onto, because two things about it were not obvious and each cost a
detour.

**A still frame reports almost no draws, and that is not a drop.** The trace line shows two to five draws a
second when nothing moves and nothing is rebuilding. The canvas draws only when there is something to draw —
a drawable that is not ready is a frame to skip — so a page that has not changed asks for nothing, and macOS
does not hand out frames nobody asked for. Reading that number as a frame rate is reading an idle counter
as a slow one; the frame rate is what it says *while the hand is moving*, and a measurement with no
stimulus in it measures nothing.

**The vertices were never the cost.** A page at 356,000 vertices is under a fifth of a millisecond of vertex
shading. The fragments are what a frame spends: 1280×800 logical at two device pixels a side is 4.1 M
pixels, four samples a pixel of multisampled colour is 16.4 M samples, the page texture covers the whole
window, and counting the page and the ink's own pass that is over 30 M samples against under a million
vertices — tens of times the work, not a few. So raising `FACET` from 1 to 2 — a real halving of the
geometry, and worth having — moved the frame's total cost by about a percent. It was the wrong lever for a
frame-rate problem, and it was pulled before the measurement that would have said so. The geometry is still
the right thing to economise, because it is free to do and the buffer is smaller; it is simply not where a
dropped frame comes from.

What that leaves, if a frame ever does drop under load, is in the fragments: the sample count, the resolve,
and how much of the window the page texture has to cover. None of them has been touched, because none of
them had to be.

### The one place a zoom did hitch, and why it was not the easing

A pinch on a long page stepped rather than glided, and the step was not in the zoom — that is a continuous
`f32` and `pen_zoom_factor` is a smooth exponential. It was in the **detail**: a stroke's outline is cut for
the rung the zoom falls in (`detail_zoom`, a factor of 1.5), and crossing a rung re-derived every stroke on
the page. Measured on the page of 8,182 strokes, one rung was **23.7 ms** — three and a half frames at
144 Hz — and it was done inside `render`, so the frame that crossed the rung paid for all of it. A nib
travelling 117 pixels crosses a rung, so the hitch came round every 117 pixels of a pinch.

The tempting answer was to ease the zoom, and it is the wrong one twice over. The zoom is already
continuous, so there is nothing to smooth; and easing would make the hand *dwell* near a boundary — the
hitch would come round more often and be easier to feel. The step is information, not a rough edge: a
different rung is a different number of pieces per curve, and no amount of easing changes how many pieces
there are.

What was done instead splits the work across the seam it always belonged on, and then puts every core on it.
`InkDocument::request_detail` records the rung a frame wants and `build_detail` does the work; the frame
calls the first, the housekeeping pump calls the second once the gesture has been quiet for
`DETAIL_SETTLE_INTERVAL`. The page keeps drawing the rung it has until then — between rungs that is under a
quarter of a pixel of chord error, which is [`FACET`]'s own bound and invisible under four samples a pixel.
Measured after the split, the frame pays **0.0 ns** for a rung and the pump pays the rebuild between frames.

Even after the split, the pump's 24 ms was time the frame loop could not use: a rung was three and a half
frames of drawing at 144 Hz, paid on the thread that also draws. The second decision was to stop spending one
core on work that is one stroke wide. Every stroke's outline is derived from that stroke alone, so the
strokes are independent and the loop over them was the one place in the view where the machine's other cores
were doing nothing. `build_detail` now runs the rebuild over a `rayon` pool:

| 8,182 strokes, one rung | time |
| --- | --- |
| a `for` loop, one core | 23.6 ms |
| the pool, twelve-core M6 | **5.0 ms** |

It is not twelve times faster because a stroke takes about three microseconds: at that size the pool's own
scheduling and the `Arc::make_mut` copy a shared stroke costs are a real share of the work. What the 4.7×
buys is that a rung is now a third of a frame at 144 Hz instead of three and a half — under the budget
rather than over it, which matters because the pump shares the main thread with the frame loop. The same
pool `note-format` seals chunks with, and it is the only parallelism in the view: everything else that runs
per frame is already cheap enough that spreading it would cost more in scheduling than it saved.

Live, on that page: **144 draws/s with the ink at 2.25 M vertices** while rungs were being rebuilt three
times a second. The number to remember is that a rung is not a frame's work, and this is the seam that says
so.

### Why a pan needed nothing

A pan does not get the same treatment, and the reason is arithmetic rather than taste. The two gestures look
alike and cost nothing alike:

| | 8,182 strokes |
|---|---|
| a zoom rung, re-deriving every outline | **24.3 ms** |
| a pan's cull, asking every stroke if it is on screen | **6.3 µs** |

Three thousand eight hundred times apart. A pan was already close to free before this change, for two
reasons that compose: the triangles are built in the **paper's** units, so a scroll is a `Transform` the
shader applies and not a rebuild at all; and when a scroll does leave the region the surface built for, what
it costs is `visible_in` per stroke — four comparisons against a precomputed bounding box — plus the fan over
the survivors' outlines, which already exist. There is no second `ribbon_outline` in a pan to move out of a
frame.

`CULL_SPAN` is what buys the room: the built region is the visible rectangle grown by half itself, so a pan
has a quarter-screen of slack on every side before it rebuilds anything. Measured with the pen on a 25-page
note: **143 to 145 draws/s through a full-screen pan**, `rebuilds/s` at 0 or 1.

So a pan would be given the pump's treatment only if its cull ever grew to a frame, and the measurement in
`measures_the_rung_costs` is what would say so: it asserts the cull stays under 2 ms, two orders of magnitude
above where it is.

### The frame cap, and what it is not

The bar's `Frames` chooser sets `frame_cap`: the most frames a second **the canvas** will draw. It is not a
display mode — nothing about the monitor changes — and it is not a request to the compositor either. It is
one comparison in `draw_canvas`, against the time since the last frame was handed over:

```rust
if held_by_cap(self.settings.frame_cap, self.last_present.map(|last| last.elapsed())) {
    self.canvas_dirty = true;   // the pump tries again; the newest description draws next time
    return;
}
```

Two details decide whether it paces anything. The comparison is `since < interval` rather than `<=`, so a
display at 60 Hz and a cap of 60 Hz agree instead of each waiting for the other. And a frame that arrives
early is **dropped rather than deferred**: the description is rebuilt from the model each time, so the next
frame draws the newest state anyway, and holding a stale one back would only add latency to a drawing that
is already on its way.

`last_present` is the clock, and it is only ever set where a frame was actually handed over — a failed
`nextDrawable` leaves it alone, so a run of missed drawables does not accumulate credit. Setting a new cap
clears it, so a cap moved down is not held back by a frame drawn a moment ago and one moved up does not wait
for the old interval to expire.

Measured on the 25-page note: **57 to 58 draws/s at `60 Hz`** and **124 to 127 at `144 Hz`**. The second is
under the 144 it asks for, and that is what a cap of this shape does — each frame is scheduled from the
*previous* one, so a little of every frame's own cost is added to the interval, and the rate settles slightly
below the ceiling. A cap is a ceiling, and settling under it is the honest outcome rather than a defect to
tune away.

### Vertical sync, and what it actually does

`V-Sync` sits beside the cap: whether a frame waits for the display's own clock before it is shown. On is the
default. `CAMetalLayer.displaySyncEnabled` is the whole of the mechanism on macOS, and `set_vsync` moves it
the moment the switch does — the property is consulted at present time, not at layer creation, so there is
no restart and no new window.

**It paces the frame rate, and the earlier reading that said otherwise was measuring the cap instead.**
With `frame_cap` at `360 Hz`, the trace read 144 draws/s either way, and that was taken to mean the switch
did nothing. It meant the cap was doing the pacing in both cases: a `360 Hz` ceiling is a 2.78 ms interval,
which binds long before a 144 Hz display's scan-out does, so the flag had nothing left to decide. The two
controls interact, and measuring one underneath the other measures the one on top:

| `frame_cap` | V-Sync | draws/s |
|---|---|---|
| `Unlimited` | off | **173 to 178** |
| `Unlimited` | on | **122 to 128** |
| `60 Hz` | on | 57 to 58 |
| `144 Hz` | on | 124 to 127 |

The row that settles it is the first pair: with no cap of the app's own, the switch alone moves the rate by
roughly forty percent. And the last two agree with the second — `144 Hz` capped and `Unlimited` with the
switch on both land at 124, because with V-Sync on it is the display's 144 Hz clock that decides, and
`displaySyncEnabled` is what makes the layer wait for it.

What that means for a reader: V-Sync **on** is the tear-free answer and costs frames above the display's
rate; V-Sync **off** lets the canvas present as fast as it draws (177/s on this page), which is lower latency
per frame at the price of tearing and of work nobody sees. The cap is a separate, lower ceiling; setting one
is not a substitute for the other, and a cap above the display's rate does not make V-Sync irrelevant — it
just hides it.

Windows has no choice to make at all: it presents at a sync interval of zero always, for reasons `device.rs`
records, so its `set_vsync` is empty and the setting is carried with the note regardless. The switch is still
on the bar there, which is a small lie in the interface rather than in the mechanism —
[WINDOWS.md §3.3](WINDOWS.md#33-the-v-sync-switch-does-nothing) has the two ways to stop telling it.

## 12. The numbers, in one table

| Constant | Value | What it decides |
|---|---|---|
| `PAGE_MARGIN` | 24 logical px | the margin between the sheet and the window's edges — the strip the floating bar covers |
| `BAR_MARGIN` | 12 logical px | how far the floating bar and the page pill sit from the window's edges: the desk showing in that gap is what makes the sheet read as paper on a desk |
| `BAR_HEIGHT` | 148 logical px | the estimate `bar_edge` falls back to only on the frame before the bar has been measured; the *measured* edge is the one line ink, the ghost cursor and the system pointer all use |
| `STATUS_PILL_WIDTH` | 0.6 | the fraction of the window the status pill may use; it wraps, and the fraction is what decides into how many lines |
| `STATUS_LIFT` | 40 logical px | how far above the desk's bottom row the status pill floats (the page pill's height plus `BAR_MARGIN`) |
| `STATUS_REFRESH_INTERVAL` | 1 s | how often the status line's counters are rebuilt while a note is in front: a readout's pace, not a frame's |
| `MEASURE_KEY` | `Ctrl+M` | the session's one key, for starting and for stopping: named once, and the line that reports on a session says it out loud |
| `HOUSEKEEPING_INTERVAL` | 1 ms | the housekeeping pump's own clock: a fixed floor, not a rate taken from the display, and the shortest wait that still parks the task rather than spinning it |
| `PDF_QUIET_INTERVAL` | 120 ms | how long the pen must be still before a page render is paid for: an eighth of a second, so a person pausing to think sees the sharp page land and the pause between two letters of a word does not count |
| `PDF_PIXEL_WIDTHS` | 1024, 1536, 2304, 3456 | the ladder a request is quantised onto; the rungs are half again as wide each time because the cost is quadratic |
| `PDF_RENDER_SCALE` | 2.0 | the bitmap-width multiplier: twice the logical width keeps page text crisp when scaled, and is paid once per page |
| `PALETTE_KEY` | `Ctrl+Shift+C` | the palette at the pen's one chord, for opening it and for putting it away; the status line says so while it is up |
| `PALETTE_RING_RADIUS` / `PALETTE_TOOL_RADIUS` | 104 / 54 logical px | how far the colours and the three tools sit from the palette's centre: the tools are in the well inside and the colours are on the ring outside, and both rings are far enough out that no point on the palette is within reach of two controls |
| `PALETTE_DISC` / `PALETTE_DISC_BOX` | 30 / 38 logical px | a colour disc, and the box it draws in — the box carries the ring that marks the keyboard's place, so a placement is measured in the box rather than in the disc |
| `PALETTE_TOOL_DISC` / `PALETTE_TOOL_ICON` | 38 / 20 logical px | a tool chip, and the icon inside it: a little over half, which is the proportion the bar's own tool buttons are drawn at |
| `PALETTE_HUB` / `PALETTE_HUB_RING` | 44 / 4 logical px | the colour the next stroke is written in, and the ring of white it is set in: the readout at the centre is the one filled circle there that is not a control, and it is drawn so as not to look like one |
| `PALETTE_WELL` / `PALETTE_WELL_PAD` | 158 / 6 logical px | the well the hub and the tools sit in, and the gap between its rim and the outermost chip: the fill that makes the middle of the palette a zone of its own |
| `PALETTE_REACH` | 24 logical px | how near a tap has to come to a colour's centre to be that colour: a disc's radius plus the slack a hand needs, and under half the gap between two colours on the densest ring |
| `PALETTE_MARGIN` / `PALETTE_PAD` / `PALETTE_RADIUS` | 8 / 14 / 133 logical px | how far the palette is kept from the window's edge, the white the ring of colours is given inside it, and the square it is drawn in: the ring radius, half a disc, and that white |
| `BATCH_INTERVAL` / `BATCH_STROKES` | 500 ms / 200 | how long ink may wait in memory, and how much of it may |
| `CHECKPOINT_QUIET_INTERVAL` / `CHECKPOINT_INTERVAL` | 5 s / 5 min | when the write-ahead log may be folded back into the note file, and how rarely |
| `PAGE_SHADOW` | three steps | the spread, the offset beyond the spread, and the colour of each of the three rectangles a sheet's shadow is made of, widest and faintest first |
| `FACET` | 2.0 window px | how long a straight piece of a stroke may be: the chord error at a pen's tightest turn is 0.17 px, under the quarter-of-a-pixel grid four samples a pixel resolve, so the curve cannot be seen through it. Raising it from 1 to 2 halved the geometry (6.98 M vertices to 3.49 M on a traced page) and the buffer with it — worth having, but it is not where a dropped frame comes from (§11) |
| `SAMPLES` | 4 | what a pixel's colour is sampled at, and the one rendering constant that would matter if a frame ever did drop: multisampling is the frame's real cost, and it is 4 because Metal has no analytic antialiasing to give a polygon (Windows fills with coverage instead) |
| `DETAIL_SETTLE_INTERVAL` | 32 ms | how long a zoom must be quiet before the detail rung it asked for is built: two frames at any pointer rate, and the pause that keeps a 23.7 ms rebuild out of the frame that crossed the rung (§11) |
| `RUNG` (`detail_zoom`) | 1.5 | the factor between detail rungs. Crossing one re-derives every stroke on the page, so the step size is a trade between how often that happens and how far the drawn detail can lag the zoom — and at 1.5 the lag is under a quarter of a pixel |
| `FrameCap` (`frame_cap`) | `Unlimited` | the most frames a second the canvas draws, whatever the display can show: `60`, `144`, `180`, `240`, `360` or none. A ceiling on the canvas's own work, checked against the time since the last frame was handed over, and not a display mode — a display slower than the chosen rate simply draws at its own |
| `vsync` | on | whether a frame waits for the display's clock before it is shown: `CAMetalLayer.displaySyncEnabled` on macOS, nothing on Windows, which presents at a sync interval of zero always. With no `frame_cap` of its own, the switch moves the rate by about forty percent — 177 draws/s off against 124 on, on a 144 Hz display — so it is a real pacer and not a cosmetic flag. A cap tighter than the display's rate binds first and hides it (§11) |

Every `PALETTE_*` length above is a *design* number, in the 96-DPI logical pixels a Windows display
measures in. The palette is drawn at those numbers times `Platform::unit_scale` — `1.0` on Windows and
`0.75` on macOS, whose logical pixel is a point — so it is the same size in the hand on both
(`PaletteGeometry`). The rings, the discs, the well, the margin and the reach of a tap all move by that
one factor, which is what keeps the gaps the palette's tests pin proportional at either size.

## Where this document and the code part company

`crates/note-ui/src/app.rs` keeps the reasoning where a reader needs it *at the line they are reading* — one line, and
usually just the surprising half of it. This file keeps the argument: the measurements, the alternatives
that were tried, and the failures that are the reason a rule exists.

So if a change makes a sentence here false, **this is the file to change** — a stale reason is worse than
no reason, because the next person will keep a rule whose justification has gone.



Panning is a distance in logical pixels, in the direction of the scroll: up is earlier in the page, so a
scroll taken away from the user moves the sheet *down* — the same rule a document viewer follows, applied
to both axes. The left stick is the same distance asked for by a position rather than by a stream: it pans
the *view*, so right is later in the page and the sheet moves left, and up is earlier in the page and the
sheet moves down. The right stick's Y axis is the same kind of rate for the zoom, anchored on the window's
centre; its X axis is not read for the zoom. While another application is in front, the same sticks are
that application's pointer and scroll, and the shoulders are its buttons — the platform posts those
events, and the view only decides how far. Zooming is *anchored* to a point, and the pan is stored rather than recomputed from a centre
each frame for exactly that reason (`view.rs`).







