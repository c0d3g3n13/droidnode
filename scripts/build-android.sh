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

if [ ! -f "$JNILIBS/libproot.so" ] || [ ! -f "$JNILIBS/libtalloc2.so" ] || [ ! -f "$JNILIBS/libproot_loader.so" ]; then
    echo ""
    echo "Downloading proot + libtalloc for Android ARM64..."

    command -v patchelf >/dev/null 2>&1 || { echo "ERROR: patchelf not found. Run: sudo apt install patchelf"; exit 1; }

    WORK=/tmp/proot-android-work
    rm -rf "$WORK" && mkdir -p "$WORK"

    # Download Termux proot (dynamically linked against libtalloc.so.2)
    wget -q --show-progress \
        "https://packages.termux.dev/apt/termux-main/pool/main/p/proot/proot_5.1.107.76_aarch64.deb" \
        -O "$WORK/proot.deb"
    dpkg -x "$WORK/proot.deb" "$WORK/proot-pkg"
    PROOT_BIN=$(find "$WORK/proot-pkg" -type f -name "proot" | head -1)

    # Termux ships the ELF loader separately at usr/lib/proot/loader.
    # We ship it as libproot_loader.so so it lands in nativeLibraryDir, which has
    # nativelib_data_file SELinux type — exec-able by untrusted_app processes.
    # code_cache/tmp has dalvikcache_data_file type; on most Android 10+ devices
    # untrusted_app lacks the execute (not execmod) SELinux permission for that type,
    # so proot's own loader extraction there silently fails.
    # Try to find the loader as a separate file in the package.
    # Termux proot 5.1.107+ ships usr/lib/proot/loader; older builds embed it.
    PROOT_LOADER_BIN=$(find "$WORK/proot-pkg" -type f \
        \( -path "*/lib/proot/loader" -o -path "*/proot/loader" \) \
        ! -name "loader32" ! -name "*.so" | head -1)

    if [ -z "$PROOT_LOADER_BIN" ]; then
        echo "Loader not found as a separate package file."
        echo "Package contents:"
        find "$WORK/proot-pkg" -type f | sort | sed 's|^|  |'
        echo ""
        echo "Extracting embedded loader ELF from proot binary..."

        # The loader is compiled into proot as a C byte array (see src/loader/elf.h).
        # Scan the binary for a second ELF magic, determine its file size using
        # the section-header table (with a program-header fallback for stripped ELFs),
        # and prefer the aarch64 64-bit ELF when multiple candidates are found.
        cat > "$WORK/extract_loader.py" << 'PYEOF'
import sys, struct

proot_path, out_path = sys.argv[1], sys.argv[2]
with open(proot_path, 'rb') as f:
    data = f.read()

ARCH_NAMES = {0xB7: 'aarch64', 0x28: 'arm32', 0x3E: 'x86_64', 0x03: 'x86'}

def elf_size(data, pos):
    """Return (size, bits, arch_str) for the ELF at data[pos], or None."""
    if pos + 64 > len(data) or data[pos:pos+4] != b'\x7fELF':
        return None
    ei_class, ei_data = data[pos+4], data[pos+5]
    if ei_data != 1 or ei_class not in (1, 2):   # require little-endian
        return None
    try:
        if ei_class == 2:   # 64-bit
            e_machine = struct.unpack_from('<H', data, pos+18)[0]
            e_phoff   = struct.unpack_from('<Q', data, pos+32)[0]
            e_phentsz = struct.unpack_from('<H', data, pos+54)[0]
            e_phnum   = struct.unpack_from('<H', data, pos+56)[0]
            e_shoff   = struct.unpack_from('<Q', data, pos+40)[0]
            e_shentsz = struct.unpack_from('<H', data, pos+58)[0]
            e_shnum   = struct.unpack_from('<H', data, pos+60)[0]
            bits = 64
        else:               # 32-bit
            e_machine = struct.unpack_from('<H', data, pos+18)[0]
            e_phoff   = struct.unpack_from('<I', data, pos+28)[0]
            e_phentsz = struct.unpack_from('<H', data, pos+42)[0]
            e_phnum   = struct.unpack_from('<H', data, pos+44)[0]
            e_shoff   = struct.unpack_from('<I', data, pos+32)[0]
            e_shentsz = struct.unpack_from('<H', data, pos+46)[0]
            e_shnum   = struct.unpack_from('<H', data, pos+48)[0]
            bits = 32
    except struct.error:
        return None

    arch = ARCH_NAMES.get(e_machine, f'0x{e_machine:x}')
    size = 0

    # Primary: section-header table end (works for non-stripped ELFs)
    if e_shoff > 0 and e_shnum > 0:
        size = e_shoff + e_shentsz * e_shnum

    # Fallback: last PT_LOAD segment end (handles stripped ELFs)
    if size < 4096 and e_phoff > 0 and e_phnum > 0:
        max_end = 0
        for i in range(min(e_phnum, 64)):
            ph = pos + e_phoff + i * e_phentsz
            if ph + e_phentsz > len(data):
                break
            if bits == 64:
                p_type   = struct.unpack_from('<I', data, ph)[0]
                p_offset = struct.unpack_from('<Q', data, ph+8)[0]
                p_filesz = struct.unpack_from('<Q', data, ph+32)[0]
            else:
                p_type   = struct.unpack_from('<I', data, ph)[0]
                p_offset = struct.unpack_from('<I', data, ph+4)[0]
                p_filesz = struct.unpack_from('<I', data, ph+16)[0]
            if p_type == 1:  # PT_LOAD
                max_end = max(max_end, p_offset + p_filesz)
        if max_end > 4096:
            size = (max_end + 4095) & ~4095   # page-align

    if 4096 <= size <= 10*1024*1024 and pos + size <= len(data):
        return size, bits, arch
    return None

candidates = []
magic = b'\x7fELF'
pos = data.find(magic, 4)   # skip outer ELF (proot itself at offset 0)
while pos != -1:
    result = elf_size(data, pos)
    if result:
        size, bits, arch = result
        print(f'  candidate ELF at offset {pos}: {bits}-bit {arch}, size {size}')
        candidates.append((pos, size, bits, arch))
    pos = data.find(magic, pos + 4)

# Prefer aarch64 64-bit, then any 64-bit, then anything
for want_arch, want_bits in [('aarch64', 64), (None, 64), (None, None)]:
    for pos, size, bits, arch in candidates:
        if (want_arch is None or arch == want_arch) and (want_bits is None or bits == want_bits):
            with open(out_path, 'wb') as f:
                f.write(data[pos:pos+size])
            print(f'Extracted {arch} {bits}-bit loader: offset={pos}, size={size}')
            sys.exit(0)

print('ERROR: no embedded loader ELF found in proot binary', file=sys.stderr)
sys.exit(1)
PYEOF

        python3 "$WORK/extract_loader.py" "$PROOT_BIN" "$WORK/proot-loader"
        if ! readelf -h "$WORK/proot-loader" 2>/dev/null | grep -q "Machine"; then
            echo "ERROR: extracted file is not a valid ELF (readelf check failed)"
            exit 1
        fi
        echo "Loader info: $(readelf -h "$WORK/proot-loader" 2>/dev/null | grep -E 'Class|Machine|Type' | tr '\n' ' ')"
        PROOT_LOADER_BIN="$WORK/proot-loader"
        echo "✓ loader extracted from proot binary"
    else
        echo "✓ loader found in package: $PROOT_LOADER_BIN"
    fi

    # Download matching libtalloc
    wget -q --show-progress \
        "https://packages.termux.dev/apt/termux-main/pool/main/libt/libtalloc/libtalloc_2.4.3_aarch64.deb" \
        -O "$WORK/libtalloc.deb"
    dpkg -x "$WORK/libtalloc.deb" "$WORK/talloc-pkg"
    TALLOC_LIB=$(find "$WORK/talloc-pkg" -name "libtalloc.so*" -type f | head -1)

    # Patch proot: rename NEEDED libtalloc.so.2 → libtalloc2.so (valid Android .so name)
    # and set RPATH=$ORIGIN so the linker finds it next to libproot.so in nativeLibraryDir.
    cp "$PROOT_BIN" "$WORK/proot-patched"
    patchelf --replace-needed libtalloc.so.2 libtalloc2.so "$WORK/proot-patched"
    patchelf --set-rpath '$ORIGIN' "$WORK/proot-patched"

    cp "$WORK/proot-patched"   "$JNILIBS/libproot.so"
    cp "$TALLOC_LIB"           "$JNILIBS/libtalloc2.so"
    cp "$PROOT_LOADER_BIN"     "$JNILIBS/libproot_loader.so"
    rm -rf "$WORK"

    # Verify the NEEDED entry was actually renamed
    if readelf -d "$JNILIBS/libproot.so" | grep -q "libtalloc.so.2"; then
        echo "ERROR: patchelf did not rename the NEEDED entry — libproot.so still requires libtalloc.so.2"
        exit 1
    fi
    echo "✓ libproot.so + libtalloc2.so + libproot_loader.so → $JNILIBS/"
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
