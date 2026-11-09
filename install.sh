#!/bin/sh
# Build begone and put it on your PATH. Re-run it any time to update.
#
#   ./install.sh                       # build and install
#   BEGONE_BIN_DIR=/usr/local/bin ./install.sh   # choose where it goes
set -eu

cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
    echo "begone is built with Rust, which isn't installed." >&2
    echo "Get it from https://rustup.rs, then run ./install.sh again." >&2
    exit 1
fi

# Where to put the binary: the first of these already on your PATH, else ~/.local/bin.
choose_dir() {
    for d in "$HOME/.local/bin" /usr/local/bin "$HOME/bin"; do
        case ":$PATH:" in *":$d:"*) printf '%s' "$d"; return;; esac
    done
    printf '%s' "$HOME/.local/bin"
}

echo "Building begone (release)…"
cargo build --release

bin_dir="${BEGONE_BIN_DIR:-$(choose_dir)}"
mkdir -p "$bin_dir"
install -m 755 target/release/begone "$bin_dir/begone"
echo "Installed to $bin_dir/begone"

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo
       echo "Note: $bin_dir is not on your PATH. Add it, for example:"
       echo "  echo 'export PATH=\"$bin_dir:\$PATH\"' >> ~/.zshrc && source ~/.zshrc" ;;
esac

cat <<'EOF'

First run:
  • For begone to read your Trash and other protected folders, give your
    terminal Full Disk Access — System Settings › Privacy & Security ›
    Full Disk Access — then quit and reopen the terminal.
  • To uninstall apps from /Applications, it also needs App Management,
    in the same place.
Then just run:  begone
EOF
