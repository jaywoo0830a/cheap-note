#!/bin/sh
#
# The macOS release, from source to the desktop, in one command: build, test, bundle, install.
#
# This script replaces `apps/macos/bundle.sh`. That file did the packaging half — the `.app`, its icon, an
# ad-hoc signature and a zip to carry; this one does the same steps and the build and the tests in front of
# them, so there is one place to read for the whole release.
#
#     scripts/macos/install-desktop.sh              # release build → tests → dist/ → ~/Desktop
#     scripts/macos/install-desktop.sh --fast       # the mac crates' tests only
#     scripts/macos/install-desktop.sh --no-build   # package the executable already built
#     scripts/macos/install-desktop.sh --app-only   # build and install, skip the tests
#     scripts/macos/install-desktop.sh --no-install # build, test, package into dist/; no desktop copy
#     scripts/macos/install-desktop.sh --binary target/aarch64-apple-darwin/release/cheap-note
#                                                   # pack a prebuilt executable — the PGO build (COMPILE.md
#                                                   # §2/§4), packed as …-pgo.zip — and skip the build
#     scripts/macos/install-desktop.sh --dry-run    # print the commands, run none of them
#
# The default is the *plain* release profile (`[profile.release]` in the root Cargo.toml), never PGO:
# `cargo pgo` leaves its binary in `target/aarch64-apple-darwin/release/` and a plain build writes
# `target/release/`, so the two do not overwrite each other, and `--binary` is how the PGO one is packed.
#
# Two things come out of a run: the `.app` to open on this machine (`dist/Cheap Note.app`) and the zip to
# carry to another one (`dist/CheapNote-<version>-macos-<arch>[-pgo].zip`).
#
# The signature is ad-hoc (`-`): enough for this machine to run what it built, and not a claim about
# distribution. A build that is handed to somebody else needs a Developer ID and a notarisation step, which
# is a decision about an Apple account rather than about this repository.

set -eu

cd "$(dirname "$0")/../.."

# The package is `cheap-note-macos` and the binary is `cheap-note` (apps/macos/Cargo.toml).
PACKAGE="cheap-note-macos"
BINARY="target/release/cheap-note"

# What comes out: the bundle, and a copy of it where it can be opened without going through `dist/`.
APP="dist/Cheap Note.app"
DESKTOP="$HOME/Desktop/Cheap Note.app"

RUN_BUILD="yes"
RUN_TESTS="yes"
TESTS_WIDE="yes"
INSTALL_DESKTOP="yes"
DRY_RUN=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --fast)
            TESTS_WIDE=""
            shift
            ;;
        --no-build)
            RUN_BUILD=""
            shift
            ;;
        --app-only)
            RUN_TESTS=""
            shift
            ;;
        --no-install)
            INSTALL_DESKTOP=""
            shift
            ;;
        --binary)
            # A prebuilt executable: no compile, and no tests of it either (the tests would test a different
            # artifact). `bundle.sh` took this flag for the PGO build, and COMPILE.md names it in the PGO
            # recipe, so it is kept.
            BINARY="${2:?--binary needs a path}"
            if [ ! -x "$BINARY" ]; then
                echo "! $BINARY is not an executable"
                exit 1
            fi
            RUN_BUILD=""
            RUN_TESTS=""
            shift 2
            ;;
        --dry-run)
            DRY_RUN="yes"
            shift
            ;;
        -h|--help)
            sed -n '3,28p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "! unknown argument: $1"
            echo "  usage: scripts/macos/install-desktop.sh [--fast] [--no-build] [--app-only] [--no-install]"
            echo "                                      [--binary <path>] [--dry-run]"
            exit 1
            ;;
    esac
done

# One place that runs a command, so `--dry-run` cannot miss one.
run() {
    echo "\$ $*"
    if [ -z "$DRY_RUN" ]; then
        "$@"
    fi
}

# 1. The build. The plain release profile — `[profile.release]` in the root Cargo.toml, which is the
#    profile COMPILE.md §1 measured as the optimum for this workspace (no PGO, no RUSTFLAGS).
if [ -n "$RUN_BUILD" ]; then
    echo "— building (release, no PGO)"
    run cargo build --release -p "$PACKAGE"
elif [ -x "$BINARY" ]; then
    echo "— --no-build: using $BINARY"
else
    echo "! --no-build was given but $BINARY does not exist"
    echo "  run without --no-build first"
    exit 1
fi

# A dry run has not built anything, so there is nothing to package or install yet.
if [ -n "$DRY_RUN" ]; then
    echo "— (dry run) done: nothing was built, packed or installed"
    exit 0
fi

# 2. The tests. The wide run is the one doc/MACOS.md §2 gives (`cargo test --workspace`: the Windows app
#    compiles to a stub off Windows, so the same command is green on both platforms). `--fast` is the
#    mac crates alone, for a loop that has already seen the rest pass.
#
#    The bench is run on the release binary as well: it draws through the same Metal device the window
#    uses, so it is the one test of the canvas that does not need a person and a pen. The pdf tests that
#    need a real Pdfium skip themselves when none is on the machine (doc/MACOS.md §2's caution), so a
#    green run here is not a claim that a page renders.
if [ -n "$RUN_TESTS" ]; then
    echo "— testing"
    if [ -n "$TESTS_WIDE" ]; then
        run cargo test --workspace
    else
        run cargo test -p "$PACKAGE" -p pen-macos
    fi

    echo "— bench (release binary, Metal device)"
    run "$BINARY" --bench --strokes 0,29,316,1000,8182 --frames 30
fi

# 3. The bundle: the `.app` Finder, the Dock and Gatekeeper want.
#
#    A bundle is not a build product — `apps/macos/build.rs` already puts `libpdfium.dylib` beside the
#    executable, which is what a terminal launch needs. This is the packaging step on top of it: the
#    `Contents/MacOS` directory, an `Info.plist`, an icon, and a signature.
echo "— assembling $APP"
mkdir -p dist
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BINARY" "$APP/Contents/MacOS/cheap-note"

# Pdfium is loaded at run time and looked for beside the executable, so it has to be copied from beside the
# one this build made — `target/release/` for a plain build and `target/<target>/release/` for a
# `--binary` one. A build without it is still a valid app: it starts, draws, writes ink, and says that a PDF
# cannot be opened.
PDFIUM="$(dirname "$BINARY")/libpdfium.dylib"
if [ -f "$PDFIUM" ]; then
    cp "$PDFIUM" "$APP/Contents/MacOS/libpdfium.dylib"
else
    echo "! no libpdfium.dylib beside $BINARY — this app will not open PDFs (see vendor/README.md)"
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

# The file that is handed to somebody else: a zip, because that is what goes on a download page or into a
# chat, and made by `ditto` rather than by `zip` because an app bundle is not a folder of files — the
# extended attributes and the executable's permission bits are part of whether it runs. `-c -k
# --sequesterRsrc` keeps what Apple's Archive Utility would, in the layout it expects. The name is the one
# the releases have always used: `<name>-<version>-macos-<arch>[-pgo].zip`, and `-pgo` is decided by the
# executable's own path (`cargo pgo` builds into `target/<target>/release/`).
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

# 4. The copy to open from the desktop.
#
#    A copy and not a link, because `dist/` is rebuilt from scratch on every run (`rm -rf` above), so a link
#    into it would point at a directory that stops existing. `ditto` and not `cp -R`, for the reason the zip
#    uses it. The old copy is removed first, so a second run replaces the app instead of merging the new
#    bundle into the old one (which would leave a stale `libpdfium.dylib` behind). It goes after the zip so
#    that a failed pack does not leave a half-installed app on the desktop. The signature travels with the
#    copy — `ditto` keeps the bundle's `_CodeSignature` — so the copy is still valid on this machine.
if [ -n "$INSTALL_DESKTOP" ]; then
    echo "— installing $DESKTOP"
    mkdir -p "$HOME/Desktop"
    rm -rf "$DESKTOP"
    ditto "$APP" "$DESKTOP"
fi

# 5. The checks. `codesign` above is allowed to refuse without failing the script, so the bundle that is
#    about to be opened is verified here: it exists, and it is still what was signed.
TARGET="$APP"
if [ -n "$INSTALL_DESKTOP" ]; then
    TARGET="$DESKTOP"
fi

echo "— verifying $TARGET"
if [ ! -d "$TARGET" ]; then
    echo "! $TARGET was not installed"
    exit 1
fi

if ! codesign --verify --deep --strict "$TARGET" 2>/dev/null; then
    echo "! $TARGET failed signature verification"
    echo "  open it anyway with Right-click → Open, or re-run this script"
    exit 1
fi

# The build.rs pair travels with the bundle (doc/MACOS.md §2): an app without it starts but cannot open a
# PDF. Worth saying out loud rather than discovering when the first textbook fails to open.
if [ ! -f "$TARGET/Contents/MacOS/libpdfium.dylib" ]; then
    echo "! no libpdfium.dylib inside the app — PDFs will not open (see vendor/README.md)"
fi

echo
echo "— done"
if [ -n "$INSTALL_DESKTOP" ]; then
    echo "  installed: $DESKTOP"
    du -sh "$DESKTOP" | cut -f1 | xargs -I{} echo "  {} on disk"
    echo "  launch it with: open \"$DESKTOP\""
else
    echo "  packaged: $APP"
    echo "  launch it with: open \"$APP\""
fi
