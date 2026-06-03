#!/usr/bin/env bash
# Build node-agent for Android ARM64 and place it in the Android jniLibs directory.
#
# Prerequisites (run once):
#   rustup target add aarch64-linux-android
#
# NDK: set ANDROID_NDK_HOME to your NDK path, or this script downloads r27c.
#   export ANDROID_NDK_HOME=/opt/android-ndk-r27c   # or wherever yours is

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JNILIBS="$REPO_ROOT/android/app/src/main/jniLibs/arm64-v8a"

# ── NDK setup ──────────────────────────────────────────────────────────────

NDK_HOME="${ANDROID_NDK_HOME:-}"
if [ -z "$NDK_HOME" ]; then
    # Try common locations
    for candidate in \
        "$HOME/Android/Sdk/ndk/27.2.12479018" \
        "$HOME/android-ndk-r27c" \
        "/opt/android-ndk-r27c" \
        "/usr/lib/android-ndk"
    do
        if [ -d "$candidate" ]; then
            NDK_HOME="$candidate"
            break
        fi
    done
fi

if [ -z "$NDK_HOME" ] || [ ! -d "$NDK_HOME" ]; then
    echo "Android NDK not found. Downloading NDK r27c to /opt/android-ndk-r27c ..."
    wget -q --show-progress \
        "https://dl.google.com/android/repository/android-ndk-r27c-linux.zip" \
        -O /tmp/android-ndk.zip
    sudo unzip -q /tmp/android-ndk.zip -d /opt/
    sudo mv /opt/android-ndk-r27c /opt/android-ndk-r27c
    rm /tmp/android-ndk.zip
    NDK_HOME="/opt/android-ndk-r27c"
fi

echo "Using NDK: $NDK_HOME"

TOOLCHAIN="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
CLANG="$TOOLCHAIN/aarch64-linux-android35-clang"

if [ ! -f "$CLANG" ]; then
    echo "ERROR: clang not found at $CLANG"
    echo "Check your NDK version — expected r27+."
    exit 1
fi

# ── Rust target ─────────────────────────────────────────────────────────────

rustup target add aarch64-linux-android 2>/dev/null || true

# ── Build ────────────────────────────────────────────────────────────────────

echo ""
echo "Building node-agent for aarch64-linux-android..."

cd "$REPO_ROOT/agent"

# cc-rs (used by ring and other C dependencies) resolves the compiler via
# CC_<target-underscored>.  The linker env var alone is not enough.
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG" \
CC_aarch64_linux_android="$CLANG" \
CXX_aarch64_linux_android="$TOOLCHAIN/aarch64-linux-android35-clang++" \
AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar" \
RANLIB_aarch64_linux_android="$TOOLCHAIN/llvm-ranlib" \
    cargo build --release -p node-agent --target aarch64-linux-android

# ── Copy binary to jniLibs ───────────────────────────────────────────────────

mkdir -p "$JNILIBS"
cp "target/aarch64-linux-android/release/node-agent" "$JNILIBS/libnode_agent.so"
echo "✓ node-agent → $JNILIBS/libnode_agent.so"

# ── proot ────────────────────────────────────────────────────────────────────

if [ ! -f "$JNILIBS/libproot.so" ]; then
    echo ""
    echo "Downloading proot-static for Android ARM64..."
    # Use proot-static (statically linked) — the regular proot package requires
    # libtalloc.so.2 which is a Termux-only library not present on stock Android.
    PROOT_DEB="/tmp/proot-static-android.deb"
    wget -q --show-progress \
        "https://packages.termux.dev/apt/termux-main/pool/main/p/proot-static/proot-static_5.4.0_aarch64.deb" \
        -O "$PROOT_DEB"
    dpkg -x "$PROOT_DEB" /tmp/proot-static-pkg
    PROOT_BIN=$(find /tmp/proot-static-pkg -type f -name "proot*" | head -1)
    if [ -z "$PROOT_BIN" ]; then
        echo "ERROR: could not find proot binary in package"
        exit 1
    fi
    cp "$PROOT_BIN" "$JNILIBS/libproot.so"
    rm -rf /tmp/proot-static-pkg "$PROOT_DEB"
    echo "✓ proot-static → $JNILIBS/libproot.so"
fi

# ── Build APK ────────────────────────────────────────────────────────────────

echo ""
echo "To build the APK (requires Android SDK + Java 17):"
echo "  cd $REPO_ROOT/android"
echo "  ./gradlew assembleDebug"
echo ""
echo "Or open android/ in Android Studio and click Run."
echo ""
echo "After installing the APK, push your kubeconfig:"
echo "  adb push ~/.kube/config /data/data/com.droidnode/files/kubeconfig"
