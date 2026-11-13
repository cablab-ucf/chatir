#!/bin/sh
# Self-updating chatir: curl -fsSL $R/latest/download/install.sh | sh
# [-s -- --server]
# --server: an always-on storage node instead (a service); then
# --push-relay URL#TOKEN, the pushir relay its devices' lanes go on.
# Fetched by curl, nothing is quarantined: no Gatekeeper, no xattr.
set -eu
R=https://github.com/cablab-ucf/chatir/releases
case $(uname -m) in arm64 | aarch64) cpu=aarch64 ;; *) cpu=x86_64 ;; esac
[ "$(uname -s)-$cpu" = Darwin-x86_64 ] &&
  { echo "chatir: Apple silicon only, for now; no Intel Mac build" >&2; exit 1; }
# The latest release, its files fetched by its tag: one release, whole.
j=$(curl -fsSL "$R/latest/download/latest.json" | tr -d ' \n')
v=$(printf %s "$j" | sed 's/.*"version":"\([^"]*\)".*/\1/')
case $v in "" | *[!0-9.]*) echo "chatir: no version: $v" >&2; exit 1 ;; esac
R=$R/download/v$v
# chatir's key: with minisign here, each file is checked as updates are:
# signed as itself, of this version. Without it, nothing stock checks
# Ed25519 (not macOS's LibreSSL, Windows' .NET, Python), and a checker
# fetched here would be vouched for by GitHub's TLS alone, as the files
# are: so installing unchecked is asked for, never silent.
KEY=RWQW5LH1agYG+mSdtDVXPOZJl9ANp4NS/IQ9lTqASnxSmJ4N33gv7Msy
tab=$(printf '\t')
check() {
  command -v minisign >/dev/null || return 0
  curl -fsSL "$1.sig" | base64 -d >"$2.minisig"
  said=$(minisign -Vm "$2" -x "$2.minisig" -P "$KEY" |
    sed -n 's/^Trusted comment: //p')
  rm -f "$2.minisig"
  case $said in
    *"${tab}file:${1##*/}${tab}version:$v") ;;
    *) rm -f "$2"; echo "$1: not signed by chatir as v$v" >&2; exit 1 ;;
  esac
}
get() {
  command -v minisign >/dev/null || [ "${CHATIR_UNCHECKED:-}" = 1 ] || {
    echo "chatir: no minisign here to check chatir's signature: only" \
      "GitHub's TLS would vouch for it. Install minisign (brew, apt, dnf," \
      "pacman) and run this again, or answer y (CHATIR_UNCHECKED=1 says so" \
      "ahead)." >&2
    printf 'Install unchecked? [y/N] ' >&2
    read -r a 2>/dev/null </dev/tty && [ "$a" = y ] || exit 1
  }
  curl -fL --progress-bar "$1" -o "$2"
  check "$1" "$2"
}
bin=$HOME/.local/bin

if [ "${1:-}" = --server ]; then
  [ "$(uname -s)" = Darwin ] && t=apple-darwin || t=unknown-linux-musl
  mkdir -p "$bin"
  get "$R/chatir-$cpu-$t" "$bin/chatir.part"
  chmod +x "$bin/chatir.part" && mv "$bin/chatir.part" "$bin/chatir"
  shift
  exec "$bin/chatir" --install "$@"
fi

if [ "$(uname -s)" = Darwin ]; then
  to=/Applications
  [ -w $to ] || { to=$HOME/Applications && mkdir -p "$to"; }
  t=$(mktemp -d)
  get "$R/chatir_aarch64.app.tar.gz" "$t/chatir.app.tar.gz"
  rm -rf "$to/chatir.app"
  tar -xzf "$t/chatir.app.tar.gz" -C "$to" && rm -rf "$t"
  open "$to/chatir.app"
  echo "chatir is in $to, and updates itself."
  exit
fi

if [ -e /etc/NIXOS ]; then
  echo "NixOS: add chatir's module instead (it keeps it up to date too):"
  echo "  https://github.com/cablab-ucf/chatir#nixos"
  exit 1
fi

# The AppImage: one file, yours to replace, so updates need no password.
url=$(printf %s "$j" |
  sed "s/.*\"linux-$cpu-appimage\":{[^}]*\"url\":\"\([^\"]*\)\".*/\1/")
app=$bin/chatir.AppImage
mkdir -p "$bin" "$HOME/.local/share/applications" \
  "$HOME/.local/share/icons/hicolor/512x512/apps"
get "$url" "$app.part"
chmod +x "$app.part" && mv "$app.part" "$app"
curl -fsSL "$R/chatir.png" \
  -o "$HOME/.local/share/icons/hicolor/512x512/apps/chatir.png" || true
# Without FUSE 2 an AppImage still runs, unpacked each time.
env=
ldconfig -p 2>/dev/null | grep -q libfuse.so.2 || {
  env="env APPIMAGE_EXTRACT_AND_RUN=1 " && export APPIMAGE_EXTRACT_AND_RUN=1
}
cat > "$HOME/.local/share/applications/chatir.desktop" <<E
[Desktop Entry]
Type=Application
Name=chatir
Comment=Private, peer-to-peer chat
Exec=$env"$app" %u
Icon=chatir
Categories=Network;Chat;
MimeType=x-scheme-handler/chatir;
E
command -v update-desktop-database >/dev/null &&
  update-desktop-database "$HOME/.local/share/applications" || true
command -v xdg-mime >/dev/null &&
  xdg-mime default chatir.desktop x-scheme-handler/chatir || true
echo "chatir is in your menu, and updates itself."
(nohup "$app" >/dev/null 2>&1 &) || true
