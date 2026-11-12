# Imported, the app and a storage node at chatir's latest release: its source
# checked against chatir's signing key, then built here, hourly. Everyone with
# a home in /home pairs their chatir with the node in one step. To leave out:
#   programs.chatir.enable = false;     # the app, and `chatir` in a terminal
#   services.chatir.enable = false;     # the storage node, always on
#   sudo cat /run/chatir/code           # the storage code, for others
{ config, lib, pkgs, ... }:
let
  cfg = config.services.chatir;
  pinned = pkgs.callPackage ./package.nix { };
  latest = "https://github.com/cablab-ucf/chatir/releases";
  key = "RWSPfztwzR/MhakRL6cA70pN03n/nMCdTvoY5UHC2IcZuhcR+41RQI95";
  # The newer of the release built here and the flake's own (a branch, or
  # main ahead of a release): equal, the flake's.
  pick = bin: lib.hiPrio (pkgs.writeShellScriptBin bin ''
    b=/var/lib/chatir-app/app/bin/${bin} v=$(cat /var/lib/chatir-app/built 2>/dev/null)
    [ -x "$b" ] && [ "$(printf '%s\n' "$v" ${pinned.version} | sort -V | tail -1)" != ${pinned.version} ] ||
      b=${pinned}/bin/${bin}
    exec "$b" "$@"
  '');
in {
  options = {
    programs.chatir.enable = lib.mkEnableOption
      "chatir, the app and terminal client, kept up to date" // { default = true; };
    # The app at login, in the tray.
    programs.chatir.autostart = lib.mkOption {
      type = lib.types.bool;
      default = true;
    };
    services.chatir.enable = lib.mkEnableOption
      "an always-on chatir storage node, kept up to date" // { default = true; };
    # Accounts that read the node's code in /run/chatir, to pair unasked.
    services.chatir.users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = lib.attrNames (lib.filterAttrs
        (_: u: u.isNormalUser && lib.hasPrefix "/home/" u.home) config.users.users);
      defaultText = "everyone with a home in /home";
    };
    # A file holding pushir's relay for this node's devices, from pushir's
    # deploy/: "https://pushir.you.example". Given to the node as a systemd
    # credential. Unset, irpush.cxn.center.
    services.chatir.pushRelayFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/secrets/chatir-push-relay";
    };
    services.chatir.signers = lib.mkOption {
      type = lib.types.lines; default = ""; visible = false;
    };
  };

  config = lib.mkIf (cfg.enable || config.programs.chatir.enable) (lib.mkMerge [
    {
      assertions = [{
        assertion = cfg.pushRelayFile == null
          || !lib.hasPrefix builtins.storeDir cfg.pushRelayFile;
        message = "services.chatir.pushRelayFile: a path outside the Nix store (it is world-readable)";
      }];
      warnings = lib.optional (cfg.signers != "")
        "services.chatir.signers does nothing now (chatir's own key checks releases): delete it";
      users.users.chatir = {
        isSystemUser = true; group = "chatir"; home = "/var/lib/chatir";
      };
      users.groups.chatir.members = cfg.users;
      # Apart from the node: taken over, it cannot swap what all here run.
      users.users.chatir-update = {
        isSystemUser = true; group = "chatir-update"; home = "/var/lib/chatir-app";
      };
      users.groups.chatir-update = { };

      # Only source signed as this very release, never an older one.
      systemd.services.chatir-update = {
        startAt = "hourly";
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        path = with pkgs; [ curl jq minisign gnutar gzip coreutils gnused git nix ];
        environment.HOME = "/var/lib/chatir-app";
        serviceConfig = {
          Type = "oneshot"; User = "chatir-update";
          WorkingDirectory = "/var/lib/chatir-app";
          StateDirectory = "chatir-app"; StateDirectoryMode = "0755";
        };
        script = ''
          v=$(curl -fsSL ${latest}/latest/download/latest.json | jq -r .version)
          case $v in "" | *[!0-9.]*) echo "no version: $v"; exit 1 ;; esac
          old=$(cat built 2>/dev/null || echo 0)
          [ "$(printf '%s\n' "$old" "$v" | sort -V | tail -1)" = "$v" ] &&
            [ "$v" != "$old" ] || exit 0
          at=${latest}/download/v$v
          curl -fsSL -o src.tar.gz "$at/source.tar.gz"
          curl -fsSL "$at/source.tar.gz.sig" | base64 -d > src.minisig
          said=$(minisign -Vm src.tar.gz -x src.minisig -P ${key} |
            sed -n 's/^Trusted comment: //p')
          t=$(printf '\t')
          case $said in
            *"file:source.tar.gz''${t}version:$v") ;;
            *) echo "signed, but not as v$v's source: $said"; exit 1 ;;
          esac
          rm -rf src && mkdir src && tar -xzf src.tar.gz -C src
          nix-build -o app -E "(import ${pkgs.path} {
            config = { }; overlays = [ ]; }).callPackage $PWD/src/package.nix { }"
          rm -rf src src.tar.gz src.minisig
          echo "$v" > built
        '';
      };
      # Soon after it is enabled, not just on the hour.
      systemd.timers.chatir-update.timerConfig.OnBootSec = "2min";
    }

    (lib.mkIf config.programs.chatir.enable {
      environment.systemPackages = [ pinned (pick "chatir-app") (pick "chatir") ];
      environment.etc."xdg/autostart/chatir.desktop" =
        lib.mkIf config.programs.chatir.autostart {
          text = ''
            [Desktop Entry]
            Type=Application
            Name=chatir
            Exec=chatir-app --hidden
            NoDisplay=true
          '';
        };
    })

    (lib.mkIf cfg.enable {
      systemd.services.chatir = {
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        serviceConfig = {
          User = "chatir";
          ExecStart =
            "${pick "chatir"}/bin/chatir --serve --dir /var/lib/chatir/data";
          Restart = "always"; RestartSec = 5;
          NoNewPrivileges = true; PrivateTmp = true; PrivateDevices = true;
          ProtectSystem = "strict"; ProtectHome = true;
          StateDirectory = "chatir/data"; StateDirectoryMode = "0700";
          RuntimeDirectory = "chatir"; RuntimeDirectoryMode = "0750";
          UMask = "0027";
          Environment = "CHATIR_CODE_FILE=/run/chatir/code";
          LoadCredential = lib.optional (cfg.pushRelayFile != null)
            "push-relay:${cfg.pushRelayFile}";
          ProtectKernelTunables = true; ProtectControlGroups = true;
          RestrictSUIDSGID = true; LockPersonality = true;
          SystemCallArchitectures = "native";
          # No privileges, no other processes, no new code in memory; only
          # the network (netlink: iroh watches the interfaces).
          CapabilityBoundingSet = ""; AmbientCapabilities = "";
          ProtectProc = "invisible"; ProcSubset = "pid";
          ProtectKernelModules = true; ProtectKernelLogs = true;
          ProtectClock = true; ProtectHostname = true;
          RestrictNamespaces = true; RestrictRealtime = true;
          MemoryDenyWriteExecute = true;
          RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        };
      };
      systemd.paths.chatir-new = {
        wantedBy = [ "multi-user.target" ];
        pathConfig.PathChanged = "/var/lib/chatir-app/built";
      };
      systemd.services.chatir-new.script = "systemctl try-restart chatir";
    })
  ]);
}
