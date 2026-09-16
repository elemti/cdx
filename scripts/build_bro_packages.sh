#!/usr/bin/env bash
set -euo pipefail

# Build Bro's two release packages on brogmk.  This intentionally keeps all
# build state on the host's local disk; the source checkout may be NFS-backed.

repo_root=${CODEX_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
build_root=${BROCODEX_BUILD_ROOT:-/home/bro/brocodex-build}
sdk_root=${SDKROOT:-/home/bro/.local/share/apple-sdk/MacOSX26.2.sdk}
version=$(sed -n 's/^version = "\([0-9][^"]*\)"/\1/p' "$repo_root/codex-rs/Cargo.toml" | head -1)

[[ "$(hostname -s)" == "brogmk" ]] || { echo "run this script on brogmk" >&2; exit 1; }
git -C "$repo_root" rev-parse --show-toplevel >/dev/null 2>&1 || {
  echo "not a git worktree: $repo_root" >&2
  exit 1
}
[[ -n "$version" ]] || { echo "cannot determine workspace version" >&2; exit 1; }

mkdir -p "$build_root"
export PATH="$(dirname "$(rustup which cargo)"):/home/linuxbrew/.linuxbrew/bin:$PATH"
export CODEX_REPO_ROOT="$repo_root"
export CARGO_PROFILE_RELEASE_DEBUG=none
export CARGO_PROFILE_RELEASE_STRIP=symbols

if (( $(ulimit -n) < 65536 )); then
  ulimit -n 65536 2>/dev/null || true
fi

linux_target=x86_64-unknown-linux-musl
mac_target=aarch64-apple-darwin
linux_package="$build_root/package-linux"
mac_package="$build_root/package-macos"
linux_cargo="$build_root/cargo-target-linux"
mac_cargo="$build_root/cargo-target-macos"

rm -rf "$linux_package" "$mac_package"

echo "== Linux musl =="
export CARGO_TARGET_DIR="$linux_cargo"
export TARGET="$linux_target"
export GITHUB_ENV="$build_root/musl.env"
: >"$GITHUB_ENV"
bash "$repo_root/.github/scripts/install-musl-build-tools.sh"
while IFS= read -r line; do export "$line"; done <"$GITHUB_ENV"
export AWS_LC_SYS_NO_JITTER_ENTROPY=1
export AWS_LC_SYS_NO_JITTER_ENTROPY_X86_64_UNKNOWN_LINUX_MUSL=1
python3 "$repo_root/scripts/build_codex_package.py" \
  --target "$linux_target" --cargo-profile release --package-dir "$linux_package"

echo "== macOS arm64 =="
[[ -d "$sdk_root" ]] || { echo "missing Apple SDK: $sdk_root" >&2; exit 1; }
# Do not let the Linux musl toolchain exported above leak into the Darwin build.
unset TARGET_CC TARGET_CXX CMAKE_C_COMPILER CMAKE_CXX_COMPILER \
  PKG_CONFIG_PATH PKG_CONFIG_PATH_x86_64_UNKNOWN_LINUX_MUSL \
  PKG_CONFIG_LIBDIR_x86_64_UNKNOWN_LINUX_MUSL
zig=$(command -v zig)
ld_lld=$(command -v ld.lld)
mac_tools=$(mktemp -d "$build_root/macos-tools.XXXXXX")
cleanup_build() { rm -rf "$mac_tools"; }
trap cleanup_build EXIT

cat >"$mac_tools/ld" <<EOF
#!/usr/bin/env bash
exec -a ld64.lld "$ld_lld" -platform_version macos 11.0 26.2 "\$@"
EOF
chmod 755 "$mac_tools/ld"

make_cc_wrapper() {
  local wrapper=$1
  local mode=$2
  cat >"$wrapper" <<EOF
#!/usr/bin/env bash
set -euo pipefail
args=()
skip=0
for arg in "\$@"; do
  if (( skip )); then skip=0; continue; fi
  case "\$arg" in
    --target|-target) skip=1; continue ;;
    --target=*|-target=*) continue ;;
    -mmacosx-version-min=*) continue ;;
  esac
  args+=("\$arg")
done
exec "$zig" cc -target aarch64-macos -B"$mac_tools" \
  -isysroot "$sdk_root" -L"$sdk_root/usr/lib" \
  -F"$sdk_root/System/Library/Frameworks" "\${args[@]}" \
  -fno-sanitize=undefined
EOF
  chmod 755 "$wrapper"
}

make_cc_wrapper "$mac_tools/cc" cc
make_cc_wrapper "$mac_tools/cxx" cxx
export CC="$mac_tools/cc" CXX="$mac_tools/cxx"
export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$mac_tools/cc"
export SDKROOT="$sdk_root" MACOSX_DEPLOYMENT_TARGET=11.0
export CARGO_TARGET_DIR="$mac_cargo"
if [[ -f /usr/lib/x86_64-linux-gnu/libsqlite3.so.0 ]]; then
  export LD_PRELOAD=/usr/lib/x86_64-linux-gnu/libsqlite3.so.0
fi
python3 "$repo_root/scripts/build_codex_package.py" \
  --target "$mac_target" --cargo-profile release --package-dir "$mac_package"

# The Rust toolchain's strip helper may not be on PATH; load its LLVM runtime
# explicitly and strip both cross-built Darwin binaries before validation.
rustc_bin=$(dirname "$(rustup which rustc)")
rust_toolchain_root="$rustc_bin/.."
rust_objcopy=$(find "$rust_toolchain_root/lib/rustlib" -type f -name rust-objcopy -perm -111 -print -quit)
rust_lib="$rust_toolchain_root/lib"
if [[ -x "$rust_objcopy" ]]; then
  for binary in "$mac_package/bin/codex" "$mac_package/bin/codex-code-mode-host"; do
    LD_LIBRARY_PATH="$rust_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
      "$rust_objcopy" --strip-debug "$binary"
  done
fi

echo "== Validate packages =="
python3 - "$linux_package" "$mac_package" "$version" <<'PY'
import json, pathlib, subprocess, sys

linux, mac, expected = map(pathlib.Path, sys.argv[1:])
for package, target in ((linux, "x86_64-unknown-linux-musl"), (mac, "aarch64-apple-darwin")):
    metadata = json.loads((package / "codex-package.json").read_text())
    assert metadata["version"] == str(expected), metadata
    assert metadata["target"] == target, metadata
    for name in ("codex", "codex-code-mode-host"):
        path = package / "bin" / name
        assert path.is_file(), path
        subprocess.run(["file", str(path)], check=True)
if linux.joinpath("bin/codex").exists():
    subprocess.run([str(linux / "bin/codex"), "--version"], check=True)
    subprocess.run([str(linux / "bin/codex-code-mode-host"), "--help"], check=True,
                   stdout=subprocess.DEVNULL)
print(f"validated {linux} and {mac}")
PY

echo "== macOS arm64 smoke check on bromini =="
bromini_host=${BROMINI_HOST:-bromini}
smoke_dir=$(ssh "$bromini_host" mktemp -d /tmp/brocodex-package.XXXXXX)
cleanup_smoke() {
  ssh "$bromini_host" rm -rf "$smoke_dir" >/dev/null 2>&1 || true
  cleanup_build
}
trap cleanup_smoke EXIT
scp -q "$mac_package/bin/codex" "$mac_package/bin/codex-code-mode-host" \
  "$bromini_host:$smoke_dir/"
ssh "$bromini_host" "$smoke_dir/codex --version"
ssh "$bromini_host" "$smoke_dir/codex-code-mode-host --help" >/dev/null
echo "macOS smoke check passed on $bromini_host"

echo "Linux package: $linux_package"
echo "macOS package: $mac_package"
