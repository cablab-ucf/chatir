#!/bin/sh
# Self-updating chatir: curl -fsSL $R/install.sh | sh [-s -- --server]
# --server: an always-on storage node instead (a service); then
# --push-relay URL#TOKEN, the pushir relay its devices' lanes go on.
# Fetched by curl, nothing is quarantined: no Gatekeeper, no xattr.
set -eu
R=https://github.com/cablab-ucf/chatir/releases/latest/download
case $(uname -m) in arm64 | aarch64) cpu=aarch64 ;; *) cpu=x86_64 ;; esac
[ "$(uname -s)-$cpu" = Darwin-x86_64 ] &&
  { echo "chatir: Apple silicon only, for now; no Intel Mac build" >&2; exit 1; }
# chatir's key: with minisign here, every file is checked as updates are;
# without, said aloud.
KEY=RWSPfztwzR/MhakRL6cA70pN03n/nMCdTvoY5UHC2IcZuhcR+41RQI95
command -v minisign >/dev/null || echo "No minisign: nothing here is" \
  "checked but GitHub's TLS (chatir checks its own updates)." >&2
check() {
  command -v minisign >/dev/null || return 0
  curl -fsSL "$1.sig" | base64 -d >"$2.minisig"
  minisign -Vm "$2" -x "$2.minisig" -P "$KEY" | grep -q "file:${1##*/}" ||
    { echo "$1: not signed by chatir" && exit 1; }
  rm -f "$2.minisig"
}
get() {
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
url=$(curl -fsSL "$R/latest.json" | tr -d ' \n' |
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
