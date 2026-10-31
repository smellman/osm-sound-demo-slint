#!/usr/bin/env bash
# Builds the Android APK.
#
# Android needs more setting up than the other targets, and this is all of it in
# one place. Two things are worth knowing before reading further.
#
# The app is a shared library here, not a binary. An android-activity app has no
# `main`: the platform loads a `cdylib` and calls `android_main`, which is why
# the crate carries a `[lib]` alongside its binary. `cargo-apk` builds the
# library and wraps it in an APK.
#
# The native MapLibre library is downloaded, as on every other target. It used
# to be built from source here, because the published `android-arm64` artifact
# was a floating snapshot that drifted away from the git-pinned binding. The
# binding now comes from crates.io and pins its artifact by version and
# checksum, so the two cannot drift.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- what to build -----------------------------------------------------------

# arm64 for a phone; x86_64 is what the emulator on an Intel host wants.
ABI="${ABI:-arm64-v8a}"
case "$ABI" in
  arm64-v8a) RUST_TARGET=aarch64-linux-android; CLANG_ARCH=aarch64-linux-android ;;
  x86_64)    RUST_TARGET=x86_64-linux-android;  CLANG_ARCH=x86_64-linux-android ;;
  *) echo "error: unsupported ABI $ABI (try arm64-v8a or x86_64)" >&2; exit 1 ;;
esac
# 26 because cpal plays through AAudio and `libaaudio.so` is not in a sysroot
# below that; it has to match `min_sdk_version` in Cargo.toml, which is what
# cargo-apk compiles the Rust against.
API="${API:-26}"

# --- the SDK -----------------------------------------------------------------

# The SDK is the directory holding platform-tools, platforms and ndk. On macOS
# that is ~/Library/Android/sdk, one below the ~/Library/Android that Android
# Studio shows.
ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
if [[ ! -d "$ANDROID_HOME/ndk" ]]; then
  if [[ -d "$ANDROID_HOME/sdk/ndk" ]]; then
    echo "note: ANDROID_HOME had no ndk/ but $ANDROID_HOME/sdk did; using that" >&2
    ANDROID_HOME="$ANDROID_HOME/sdk"
  else
    echo "error: no Android SDK at $ANDROID_HOME (no ndk/ inside it)" >&2
    echo "Set ANDROID_HOME to the directory holding platform-tools and ndk." >&2
    exit 1
  fi
fi
export ANDROID_HOME
export ANDROID_SDK_ROOT="$ANDROID_HOME"

# Newest installed NDK unless one is named. `sort -V` so 28.1 beats 28.0 and 9
# does not beat 30.
NDK_VERSION="${NDK_VERSION:-$(ls "$ANDROID_HOME/ndk" | sort -V | tail -1)}"
NDK="$ANDROID_HOME/ndk/$NDK_VERSION"
[[ -d "$NDK" ]] || { echo "error: no NDK at $NDK" >&2; exit 1; }
export ANDROID_NDK_HOME="$NDK"
export ANDROID_NDK_ROOT="$NDK"
# The prebuilt directory is named for the host the toolchain was built on, which
# on Apple silicon is still the x86_64 one.
TOOLCHAIN="$(echo "$NDK"/toolchains/llvm/prebuilt/*)"
[[ -d "$TOOLCHAIN" ]] || { echo "error: no LLVM toolchain under $NDK" >&2; exit 1; }
export PATH="$TOOLCHAIN/bin:$PATH"

# Anything that compiles C for Android — `ring`, behind ureq's TLS, is the one
# that will stop you — reads these rather than finding the NDK on its own.
upper_target="$(echo "$RUST_TARGET" | tr 'a-z-' 'A-Z_')"
export "CC_${RUST_TARGET//-/_}=$TOOLCHAIN/bin/${CLANG_ARCH}${API}-clang"
export "CXX_${RUST_TARGET//-/_}=$TOOLCHAIN/bin/${CLANG_ARCH}${API}-clang++"
export "AR_${RUST_TARGET//-/_}=$TOOLCHAIN/bin/llvm-ar"
export "CARGO_TARGET_${upper_target}_LINKER=$TOOLCHAIN/bin/${CLANG_ARCH}${API}-clang"

# bindgen runs libclang itself and does not read the CFLAGS above, so it has to
# be told the triple separately. It must be the versioned one — the NDK's
# sys/cdefs.h refuses an unversioned target outright ("Unversioned target
# triples are not supported!"), which is what a bare `--target=aarch64-linux-android`
# gets you. cargo-apk sets a bare `clang` as CC and passes the version in
# CFLAGS, so without this the C compiles and the bindings do not.
export "BINDGEN_EXTRA_CLANG_ARGS_${RUST_TARGET//-/_}=--target=${CLANG_ARCH}${API} --sysroot=$TOOLCHAIN/sysroot"

# This app's pin, named outright so the preflight below checks the targets of
# the toolchain that will build, wherever the script is run from. It has to
# match the one the downloaded native library was built with; see
# rust-toolchain.toml.
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$(sed -n 's/^ *channel *= *"\(.*\)".*/\1/p' "$here/rust-toolchain.toml" | head -1)}"
[[ -n "$RUSTUP_TOOLCHAIN" ]] ||
  { echo "error: no channel in rust-toolchain.toml; set RUSTUP_TOOLCHAIN" >&2; exit 1; }
export RUSTUP_TOOLCHAIN

# --- preflight ---------------------------------------------------------------

missing=0
need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: $1 is not installed — $2" >&2
    missing=1
  fi
}
need cargo-apk "cargo install cargo-apk"
rustup target list --installed | grep -qx "$RUST_TARGET" ||
  { echo "error: rustup +$RUSTUP_TOOLCHAIN target add $RUST_TARGET" >&2; missing=1; }
(( missing == 0 )) || exit 1

# --- the signing key ---------------------------------------------------------

# Even a build that never leaves the desk has to be signed. This is a
# development key, made once and kept out of git; see the note in Cargo.toml.
KEYSTORE="$here/build/android-dev.keystore"
if [[ ! -f "$KEYSTORE" ]]; then
  echo "==> making a development signing key at ${KEYSTORE#"$here/"}"
  mkdir -p "$(dirname "$KEYSTORE")"
  keytool -genkeypair -v -keystore "$KEYSTORE" -alias androidkey \
    -keyalg RSA -keysize 2048 -validity 10000 \
    -storepass android -keypass android \
    -dname "CN=OSM Sound Demo development, O=unsigned, C=JP" >/dev/null
fi

# --- the APK -----------------------------------------------------------------

echo "==> building the APK for $ABI"
cd "$here"
# OpenGL, which is what the app's EGL path drives on Android (the FFI also
# publishes a Vulkan artifact), and one has to be named because the crate has
# no default.
cargo apk build --release --lib --features opengl --target "$RUST_TARGET" "$@"

echo
echo "APK:"
find "$here/target/release/apk" -name '*.apk' -maxdepth 2 2>/dev/null ||
  find "$here/target" -name '*.apk' 2>/dev/null | head -3
