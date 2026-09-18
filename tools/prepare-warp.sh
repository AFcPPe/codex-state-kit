#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resource_dir="$project_root/src-tauri/resources/warp"
download_dir="$project_root/target/warp-download"
arch="${1:-$(uname -m)}"

case "$arch" in
  arm64|aarch64) asset_arch="arm64"; expected="762e2dc875669566207a3c776a53dc6bb50770da25f90e1ab69fbc53e91f8da1" ;;
  amd64|x86_64) asset_arch="amd64"; expected="1eb41e34bf4cd0b81e06222ef844cea75eb56229a62fe29c07cf8e7bd253357d" ;;
  *) echo "Unsupported macOS architecture: $arch" >&2; exit 2 ;;
esac

url="https://github.com/Diniboy1123/usque/releases/download/v4.2.1/usque_4.2.1_darwin_${asset_arch}.zip"
archive="$download_dir/usque-darwin-${asset_arch}.zip"
unpacked="$download_dir/usque-darwin-${asset_arch}"
mkdir -p "$download_dir" "$resource_dir"

if [[ ! -f "$archive" ]] || [[ "$(shasum -a 256 "$archive" | awk '{print tolower($1)}')" != "$expected" ]]; then
  curl -fL --retry 3 --connect-timeout 15 --max-time 120 "$url" -o "$archive"
fi
actual="$(shasum -a 256 "$archive" | awk '{print tolower($1)}')"
[[ "$actual" == "$expected" ]] || { echo "usque archive checksum mismatch" >&2; exit 1; }

rm -rf "$unpacked"
mkdir -p "$unpacked"
unzip -q -o "$archive" -d "$unpacked"
rm -f "$resource_dir/usque" "$resource_dir/usque.exe"
install -m 0755 "$unpacked/usque" "$resource_dir/usque"
echo "Bundled usque v4.2.1 (Darwin ${asset_arch}); SHA-256 verified."
