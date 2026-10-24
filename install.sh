#!/bin/sh
# Omni2API installer: puts `omni2api` and `oh-seat` in ~/.omni2api/bin.
#   curl -fsSL https://raw.githubusercontent.com/Juwan-Hwang/omni2api/main/install.sh | sh
# Then: omni2api up
set -e
REPO=Juwan-Hwang/omni2api
DIR="${OMNI2API_HOME:-$HOME/.omni2api}/bin"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  T=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) T=aarch64-unknown-linux-musl ;;
  Darwin-arm64)  T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  *) echo "omni2api: no prebuilt binary for $(uname -s)-$(uname -m); build from source (cargo build --release in server/ and cli/)" >&2; exit 1 ;;
esac
mkdir -p "$DIR"
for b in omni2api oh-seat; do
  curl -fsSL "https://github.com/$REPO/releases/latest/download/$b-$T" -o "$DIR/$b"
  chmod +x "$DIR/$b"
done
echo "installed: $DIR/omni2api $DIR/oh-seat"

# Make the commands findable. A shell can't change its parent's PATH, so:
# 1) link into a directory that is already on PATH (works in this shell
#    too), else 2) add $DIR to the shell rc files for new shells.
# OMNI2API_NO_MODIFY_PATH=1 skips both.
on_path() { case ":$PATH:" in *":$1:"*) return 0 ;; esac; return 1; }
LINKED=""
if [ -z "$OMNI2API_NO_MODIFY_PATH" ] && ! on_path "$DIR"; then
  for d in "$HOME/.local/bin" "$HOME/bin" /usr/local/bin; do
    if on_path "$d" && { [ -d "$d" ] || mkdir -p "$d" 2>/dev/null; } && [ -w "$d" ]; then
      for b in omni2api oh-seat; do ln -sf "$DIR/$b" "$d/$b"; done
      LINKED="$d"; break
    fi
  done
  if [ -z "$LINKED" ]; then
    LINE="export PATH=\"$DIR:\$PATH\" # omni2api"
    for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
      case "$rc" in */.profile) ;; *) [ -f "$rc" ] || continue ;; esac
      grep -qs "# omni2api" "$rc" || printf '\n%s\n' "$LINE" >> "$rc"
    done
    [ -d "$HOME/.config/fish" ] && { mkdir -p "$HOME/.config/fish/conf.d"; echo "fish_add_path $DIR" > "$HOME/.config/fish/conf.d/omni2api.fish"; }
  fi
fi
if on_path "$DIR" || [ -n "$LINKED" ]; then
  [ -n "$LINKED" ] && echo "linked into $LINKED (already on PATH)"
  echo "next: omni2api up"
else
  echo "added $DIR to PATH for new shells; this shell does not see it yet."
  echo "next (works right now): $DIR/omni2api up"
fi
