# gpui-pre-windows 0.3.12, with one line changed

This is the published `gpui-pre-windows` 0.3.12 crate, Apache-2.0, a snapshot of
`zed-industries/zed@d2a4fb6` (`[package.metadata.gpui-pre]` in `Cargo.toml` records the same).
It is vendored only because of the line below; everything else is upstream verbatim.

## The change

`src/directx_renderer.rs`, `DirectComposition::new`:

```diff
-let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, false) }?;
+let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
```

0.3.11 used `true`; 0.3.12 changed it to `false`.

## Why

Windows allows at most two DirectComposition targets for one window: one **topmost** and one
**non-topmost** ([CreateTargetForHwnd](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-idcompositiondevice-createtargetforhwnd)).
This app's interface and canvas share a window by taking one layer each:

* GPUI draws the bar, the pills and the lists — the interface — and must be **above** the canvas;
* the canvas (`apps/windows/src/platform/ink_layer/device.rs`) asks for `topmost = false`, so it is
  drawn behind GPUI's visual.

With 0.3.12, GPUI asks for the non-topmost layer too. The canvas then has no layer left, and its
`CreateTargetForHwnd` fails with `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`. The app panics during
window creation; in a release build (`panic = "abort"`, windows subsystem) that is a silent
`0xc0000409` exit with nothing on screen. See `doc/WINDOWS.md` §3.4.

Restoring `true` gives GPUI the topmost layer and leaves the non-topmost one for the canvas, which is
the arrangement 0.3.11 had and the one the renderer was designed around.

## When upgrading GPUI

Check whether upstream `gpui-pre-windows` has restored the topmost target. If it has, delete this
directory and the `[patch.crates-io]` entry in the workspace `Cargo.toml`. If it has not, re-apply
the one-line change to the new version and update this note.
