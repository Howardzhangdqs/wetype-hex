#!/bin/sh
# Install a desktop launcher for this checkout's Hex (WeType fork).
#
# Copies the release binary to ~/.local/bin/wetype-hex and writes
# ~/.local/share/applications/wetype-hex.desktop, so the app appears in the
# desktop's application menu. Re-run after `cargo build --release`.
#
# Usage: packaging/linux/install-desktop-entry.sh [--build]
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
bin_dir=${XDG_BIN_HOME:-$HOME/.local/bin}
data_dir=${XDG_DATA_HOME:-$HOME/.local/share}
apps_dir=$data_dir/applications
source_bin=$repo/target/release/hex
target_bin=$bin_dir/wetype-hex

if [ ! -x "$source_bin" ]; then
    echo "No release binary at $source_bin; run: cargo build --release --bin hex" >&2
    exit 1
fi

mkdir -p "$bin_dir" "$apps_dir"
install -m 755 "$source_bin" "$target_bin"

# The path as one quoted Exec argument, escaped as the Desktop Entry spec
# reads it. Shell parameter expansion below inserts it without sed
# interpreting `&`, `|`, or backslashes in the path.
quoted=$(printf '%s' "$target_bin" | awk '{gsub(/[\\"`$]/, "\\\\&"); gsub(/%/, "%%"); print}')
desktop=$(mktemp)
trap 'rm -f "$desktop"' EXIT
while IFS= read -r line; do
    case $line in
        *@HEX_BIN@*)
            prefix=${line%%@HEX_BIN@*}
            suffix=${line#*@HEX_BIN@}
            printf '%s\n' "$prefix\"$quoted\"$suffix"
            ;;
        *) printf '%s\n' "$line" ;;
    esac
done < "$repo/packaging/linux/wetype-hex.desktop" > "$desktop"
install -m 644 "$desktop" "$apps_dir/wetype-hex.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
fi

echo "Installed $target_bin"
echo "Installed $apps_dir/wetype-hex.desktop"
echo "The launcher is named \"Hex (WeType)\" in your application menu."
echo "Its icon appears after the first launch, when Hex installs its hicolor icons."
