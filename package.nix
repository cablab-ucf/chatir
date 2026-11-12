# The app and `chatir`; nixos.nix builds each release with this.
{ lib, rustPlatform, pkg-config, wrapGAppsHook3, webkitgtk_4_1, libsoup_3
, glib-networking, gst_all_1, openssl, makeDesktopItem, copyDesktopItems
, libayatana-appindicator, alsa-lib }:
rustPlatform.buildRustPackage {
  pname = "chatir";
  version = (lib.importTOML ./Cargo.toml).workspace.package.version;
  # Not cargo's target/ (gigabytes).
  src = lib.cleanSourceWith {
    src = lib.cleanSource ./.;
    filter = p: _: baseNameOf p != "target";
  };
  # pushir comes from git, at the commit Cargo.lock names.
  cargoLock = { lockFile = ./Cargo.lock; allowBuiltinFetchGit = true; };
  # typst asks for rustc 1.92 and builds with NixOS 25.11's 1.91.
  cargoBuildFlags = [ "-p" "chatir" "-p" "chatir-app" "--ignore-rust-version" ];
  buildFeatures = [ "chatir-app/custom-protocol" ];
  # NixOS updates it, not the app.
  env.CHATIR_NIXOS = "1";
  doCheck = false; # its tests need a network
  nativeBuildInputs = [ pkg-config wrapGAppsHook3 copyDesktopItems ];
  # Calls' codecs through WebKit's GStreamer: Opus (base), VP8 and VP9
  # (good), AV1 and echo cancelling (bad), H.264 (libav).
  # A call's voice: ALSA, which PipeWire and PulseAudio serve too.
  buildInputs = [ webkitgtk_4_1 libsoup_3 glib-networking openssl alsa-lib ]
    ++ (with gst_all_1; [
      gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav
    ]);
  # The tray loads it at run time. WebKit's DMA-BUF renderer leaves a blank
  # window on some GPUs under Wayland (sway): off unless set otherwise.
  preFixup = ''
    gappsWrapperArgs+=(--prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ libayatana-appindicator ]})
    gappsWrapperArgs+=(--set-default WEBKIT_DISABLE_DMABUF_RENDERER 1)
  '';
  desktopItems = [
    (makeDesktopItem {
      name = "chatir";
      desktopName = "chatir";
      comment = "Private, peer-to-peer chat";
      exec = "chatir-app %u";
      icon = "chatir";
      categories = [ "Network" "Chat" ];
      mimeTypes = [ "x-scheme-handler/chatir" ];
    })
  ];
  postInstall = ''
    install -Dm644 app/icons/icon.png \
      $out/share/icons/hicolor/512x512/apps/chatir.png
  '';
  meta = {
    description = "Private, peer-to-peer community chat over iroh";
    homepage = "https://github.com/cablab-ucf/chatir";
    license = lib.licenses.gpl3Plus;
    mainProgram = "chatir-app";
  };
}
