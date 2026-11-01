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
#     apps/macos/bundle.sh            → dist/Cheap Note.app
#
# The signature is ad-hoc (`-`): enough for this machine to run what it built, and not a claim about
# distribution. A build that will be handed to someone else needs a Developer ID and a notarisation step,
# which is a decision about an Apple account rather than about this repository.

set -eu

cd "$(dirname "$0")/../.."

APP="dist/Cheap Note.app"
NAME="cheap-note"

echo "— building (release)"
cargo build --release -p "$NAME-macos"

echo "— assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/cheap-note "$APP/Contents/MacOS/cheap-note"

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

echo "— done: $APP"
du -sh "$APP" | cut -f1 | xargs -I{} echo "  {} on disk"
echo "  launch it with: open \"$APP\""
