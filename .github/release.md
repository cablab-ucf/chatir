## Install

Each of these keeps itself up to date.

| | |
|---|---|
| **Windows** | In PowerShell: `irm https://github.com/cablab-ucf/chatir/releases/latest/download/install.ps1 \| iex` |
| **macOS, Linux** | In a terminal: `curl -fsSL https://github.com/cablab-ucf/chatir/releases/latest/download/install.sh \| sh` |
| **NixOS** | `programs.chatir.enable = true;` with [chatir's module](https://github.com/cablab-ucf/chatir#nixos) |
| **Android** | On the phone, open [chatir.apk](https://github.com/cablab-ucf/chatir/releases/latest/download/chatir.apk) and allow your browser to install it, once. Or add `https://github.com/cablab-ucf/chatir` to [Obtainium](https://obtainium.imranr.dev). |
| **iPhone** | Install [chatir.ipa](https://github.com/cablab-ucf/chatir/releases/latest/download/chatir.ipa) with [AltStore](https://altstore.io) or [SideStore](https://sidestore.io), which re-sign it weekly; new versions install the same way. |

**An always-on storage node**, so your spaces wait for you while everyone is offline:
- In the app: a space's menu → Storage nodes → *Run one on this computer*.
- From a terminal: add `-s -- --server` after `sh` above, or on Windows run `& ([scriptblock]::Create((irm https://github.com/cablab-ucf/chatir/releases/latest/download/install.ps1))) -Server`.
- On NixOS: `services.chatir.enable = true;`.

The installers fetch the files below. The `.dmg` and `.msi` work too, but a browser marks them as downloaded from the internet. macOS then asks you to allow chatir in System Settings → Privacy & Security, and Windows asks you to choose *More info → Run anyway*. `chatir-<platform>` is the terminal client and storage node; `.sig` files are chatir's signatures, checked by every update.

**From irch**: chatir is irch renamed, the same people and spaces. Quit irch, then start chatir on each computer: it takes irch's folder. A phone links from a computer that has chatir and syncs its history.
