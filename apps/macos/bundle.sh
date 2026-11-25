#!/bin/sh
#
# Wraps a release build in the `.app` Finder, the Dock and Gatekeeper want.
#
# A bundle is not a build product: `apps/macos/build.rs` already puts `libpdfium.dylib` beside the
# executable, which is what a terminal launch needs. This is the packaging step on top of it — the
# `Contents/MacOS` directory the executable goes in, an `Info.plist`, an icon, and a signature.
#
# Run from anywhere; it works from the repository root it finds itself in:
#
#     apps/macos/bundle.sh            → dist/Cheap Note.app  and  dist/CheapNote-0.1.0-macos-arm64.zip
#     apps/macos/bundle.sh --binary target/aarch64-apple-darwin/release/cheap-note
#                                     → the same, with a PGO build instead of a plain one (COMPILE.md),
#                                       packed as dist/CheapNote-0.1.0-macos-arm64-pgo.zip
#     apps/macos/bundle.sh --install-desktop
#                                     → the same, and then a copy in ~/Desktop so it opens from the Dock
#                                       without going through dist/ first
#
# Two things come out of it: the `.app` to open on this machine, and the zip to carry to another one.
#
# The signature is ad-hoc (`-`): enough for this machine to run what it built, and not a claim about
# distribution. A build that will be handed to someone else needs a Developer ID and a notarisation step,
# which is a decision about an Apple account rather than about this repository.

set -eu

cd "$(dirname "$0")/../.."

APP="dist/Cheap Note.app"
NAME="cheap-note"

# Which executable goes in the bundle.
#
#   (nothing)        build the plain release profile
#   --binary <path>  use an executable already built — the PGO one, which `cargo pgo` leaves in
#                    `target/aarch64-apple-darwin/release/` and which a plain `cargo build --release`
#                    here would overwrite. `COMPILE.md` has the three commands that make it.
BINARY=""

# Whether to also put a copy where it can be opened without going through `dist/`.
#
# A copy and not a link: `dist/` is rebuilt from scratch on every run (`rm -rf` below), so a link into it
# would be a link to a directory that stops existing. `ditto` and not `cp -R`, for the reason the zip uses
# it: an app bundle is not a folder of files — the extended attributes and the executable's permission bits
# are part of whether it runs. The old copy is removed first, so a second run replaces the app instead of
# merging the new bundle into the old one (which would leave a stale `libpdfium.dylib` behind).
INSTALL_DESKTOP=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary)
            BINARY="${2:?--binary needs a path}"
            if [ ! -x "$BINARY" ]; then
                echo "! $BINARY is not an executable"
                exit 1
            fi
            shift 2
            ;;
        --install-desktop)
            INSTALL_DESKTOP="yes"
            shift
            ;;
        *)
            echo "! unknown argument: $1"
            echo "  usage: apps/macos/bundle.sh [--binary <path>] [--install-desktop]"
            exit 1
            ;;
    esac
done

if [ -n "$BINARY" ]; then
    echo "— using $BINARY (no build)"
else
    echo "— building (release)"
    cargo build --release -p "$NAME-macos"
    BINARY="target/release/cheap-note"
fi

echo "— assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BINARY" "$APP/Contents/MacOS/cheap-note"

# Pdfium is loaded at run time and looked for beside the executable. A build without it is still a valid
# app: it starts, draws, writes ink, and says that a PDF cannot be opened.
if [ -f target/release/libpdfium.dylib ]; then
    cp target/release/libpdfium.dylib "$APP/Contents/MacOS/libpdfium.dylib"
else
    echo "! no libpdfium.dylib — this app will not open PDFs (see vendor/README.md)"
fi

cp apps/macos/Info.plist "$APP/Contents/Info.plist"

# An icon, from the Windows build's own artwork: the `.ico` already carries a 256-pixel image, and an
# `.icns` is that image at the sizes the Finder asks for.
if [ -f apps/windows/assets/app.ico ]; then
    ICONSET=$(mktemp -d)/AppIcon.iconset
    mkdir -p "$ICONSET"
    sips -s format png apps/windows/assets/app.ico --out "$ICONSET/base.png" >/dev/null 2>&1 || true

    if [ -f "$ICONSET/base.png" ]; then
        for size in 16 32 64 128 256 512; do
            sips -z "$size" "$size" "$ICONSET/base.png" \
                --out "$ICONSET/icon_${size}x${size}.png" >/dev/null 2>&1 || true
        done
        # The @2x variants are the next size up, which is how Apple's naming works.
        cp "$ICONSET/icon_32x32.png" "$ICONSET/icon_16x16@2x.png" 2>/dev/null || true
        cp "$ICONSET/icon_64x64.png" "$ICONSET/icon_32x32@2x.png" 2>/dev/null || true
        cp "$ICONSET/icon_256x256.png" "$ICONSET/icon_128x128@2x.png" 2>/dev/null || true
        cp "$ICONSET/icon_512x512.png" "$ICONSET/icon_256x256@2x.png" 2>/dev/null || true

        iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns" 2>/dev/null || \
            echo "! could not make an icon; the app will use the generic one"
    fi
fi

echo "— signing (ad-hoc)"
codesign --force --sign - "$APP" >/dev/null 2>&1 || echo "! codesign refused; the app may need it to run"

# The file that is handed to somebody else.
#
# A zip, because that is what goes on a download page or into a chat, and made by `ditto` rather than by
# `zip` for the reason an app bundle is not a folder of files: Finder-launched apps carry extended
# attributes and resource forks, and the executable's permission bits are part of whether the app runs
# at all. `ditto -c -k --sequesterRsrc` keeps what Apple's Archive Utility would, in the layout it
# expects, and is the archiver Apple recommends for a bundle. The name is the one the releases have
# always used: `<name>-<version>-macos-<arch>[-pgo].zip`.
#
# Nothing is deleted: the `.app` stays in `dist/` to open, and the zip is the copy to carry. The zip is
# *ad-hoc signed app* too — it will run on the machine that built it, and on another Mac it needs
# Right-click → Open (or `xattr -dr com.apple.quarantine`) because it is not notarised (see the top of
# this file).
VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist" 2>/dev/null || echo 0.0.0)
ARCH=$(uname -m)
PROFILE=""
case "$BINARY" in
    *aarch64-apple-darwin*|*x86_64-apple-darwin*) PROFILE="-pgo" ;;
esac
ZIP="dist/CheapNote-$VERSION-macos-$ARCH$PROFILE.zip"

echo "— packing $ZIP"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

echo "— done: $APP"
du -sh "$APP" | cut -f1 | xargs -I{} echo "  {} on disk"
echo "  launch it with: open \"$APP\""
echo "— done: $ZIP"
du -sh "$ZIP" | cut -f1 | xargs -I{} echo "  {} to carry"
echo "  check it with: ditto -x -k \"$ZIP\" /tmp/cheap-note-check && open \"/tmp/cheap-note-check/Cheap Note.app\""

# A copy to open from the Dock, asked for with `--install-desktop`.
#
# It goes after the zip so that a failed pack does not leave a half-installed app on the desktop: the
# desktop copy is a convenience, and dishing out a copy of a bundle that did not finish being built would
# be the one thing worse than not having one. The signature travels with the copy — `ditto` keeps the
# bundle's `_CodeSignature` — so the copy is still valid on this machine.
if [ -n "$INSTALL_DESKTOP" ]; then
    DESKTOP="$HOME/Desktop/Cheap Note.app"

    echo "— installing $DESKTOP"
    mkdir -p "$HOME/Desktop"
    rm -rf "$DESKTOP"
    ditto "$APP" "$DESKTOP"

    echo "— done: $DESKTOP"
    echo "  launch it with: open \"$DESKTOP\""
fi
