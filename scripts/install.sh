#!/bin/sh
set -eu

repo='https://github.com/model-clis/jev'
version=${JEV_VERSION:-latest}
install_dir=${JEV_INSTALL_DIR:-"$HOME/.local/bin"}

case "$version" in
  latest) base="$repo/releases/latest/download" ;;
  *)
    printf '%s\n' "$version" | grep -Eq '^v[0-9]{4}\.[1-9][0-9]*\.[0-9]+$' || {
      echo "JEV_VERSION must be a complete vYYYY.MDD.REV tag" >&2; exit 1;
    }
    base="$repo/releases/download/$version"
    ;;
esac

os=$(uname -s); arch=$(uname -m)
case "$os:$arch" in
  Linux:x86_64|Linux:amd64) asset=jev-linux-x86_64 ;;
  Darwin:arm64|Darwin:aarch64) asset=jev-macos-aarch64 ;;
  *) echo "Unsupported platform: $os $arch (supported: Linux x86_64, Darwin arm64)" >&2; exit 1 ;;
esac

command -v curl >/dev/null 2>&1 || { echo 'curl is required' >&2; exit 1; }
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/jev-install.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
curl -fL --proto '=https' --tlsv1.2 "$base/$asset" -o "$tmp_dir/$asset"
curl -fL --proto '=https' --tlsv1.2 "$base/$asset.sha256" -o "$tmp_dir/$asset.sha256"
expected=$(awk 'NR==1 {print $1}' "$tmp_dir/$asset.sha256")
case "$expected" in *[!0-9A-Fa-f]*|'') echo 'Invalid SHA256 file' >&2; exit 1;; esac
[ ${#expected} -eq 64 ] || { echo 'Invalid SHA256 length' >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp_dir/$asset" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$tmp_dir/$asset" | awk '{print $1}')
else echo 'sha256sum or shasum is required' >&2; exit 1; fi
[ "$(printf %s "$actual" | tr A-F a-f)" = "$(printf %s "$expected" | tr A-F a-f)" ] || { echo 'SHA256 verification failed' >&2; exit 1; }
mkdir -p "$install_dir"
chmod 755 "$tmp_dir/$asset"
stage="$install_dir/.jev.$$"
cp "$tmp_dir/$asset" "$stage"
chmod 755 "$stage"
mv -f "$stage" "$install_dir/jev"
echo "Installed jev to $install_dir/jev"
case ":$PATH:" in *":$install_dir:"*) :;; *) echo "Add $install_dir to PATH.";; esac
