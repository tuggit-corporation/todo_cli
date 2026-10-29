#!/bin/sh
set -eu

repo=tuggit-corporation/todo_cli
install_dir=${TODO_INSTALL_DIR:-"$HOME/.local/bin"}

case "$(uname -s)" in
    Darwin) platform=apple-darwin ;;
    Linux) platform=unknown-linux-gnu ;;
    *) echo "Unsupported operating system; todo supports macOS and Linux." >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64) arch=x86_64 ;;
    *) echo "Unsupported architecture; todo supports x86_64 and ARM64." >&2; exit 1 ;;
esac

for tool in curl tar install mktemp; do
    command -v "$tool" >/dev/null 2>&1 || { echo "Required tool missing: $tool" >&2; exit 1; }
done
if command -v sha256sum >/dev/null 2>&1; then
    checksum=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum=shasum
else
    echo "Install sha256sum or shasum to verify the download." >&2
    exit 1
fi

latest=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' \
    "https://github.com/$repo/releases/latest") || {
    echo "Cannot find a release. Check connectivity and whether a release has been published." >&2
    exit 1
}
case "$latest" in
    "https://github.com/$repo/releases/tag/v"*) tag=${latest##*/} ;;
    *) echo "No published release found." >&2; exit 1 ;;
esac
case "$tag" in
    *[!a-zA-Z0-9._-]*) echo "Invalid release tag." >&2; exit 1 ;;
esac

archive="todo-${tag#v}-$arch-$platform.tar.gz"
base="https://github.com/$repo/releases/download/$tag"
tmp=$(mktemp -d)
trap 'rm -f "$tmp/archive.tar.gz" "$tmp/checksum" "$tmp/todo"; rmdir "$tmp"' 0
trap 'exit 1' HUP INT TERM
curl --proto '=https' --tlsv1.2 -fsSL "$base/$archive" -o "$tmp/archive.tar.gz"
curl --proto '=https' --tlsv1.2 -fsSL "$base/$archive.sha256" -o "$tmp/checksum"
read -r expected rest < "$tmp/checksum"
case "$expected" in
    ''|*[!a-fA-F0-9]*) echo "Invalid release checksum." >&2; exit 1 ;;
esac
[ "${#expected}" -eq 64 ] || { echo "Invalid release checksum." >&2; exit 1; }
if [ "$checksum" = sha256sum ]; then
    actual=$(sha256sum "$tmp/archive.tar.gz")
else
    actual=$(shasum -a 256 "$tmp/archive.tar.gz")
fi
actual=${actual%% *}
[ "$actual" = "$expected" ] || { echo "Checksum mismatch; nothing installed." >&2; exit 1; }
tar -xzf "$tmp/archive.tar.gz" -C "$tmp" todo
[ -f "$tmp/todo" ] && [ ! -L "$tmp/todo" ] || { echo "Invalid release executable." >&2; exit 1; }
mkdir -p "$install_dir"
install -m 755 "$tmp/todo" "$install_dir/todo"
printf 'Installed todo %s to %s/todo\n' "${tag#v}" "$install_dir"
case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) printf 'Add this directory to your PATH: %s\n' "$install_dir" ;;
esac
