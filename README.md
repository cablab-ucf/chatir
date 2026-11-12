# <img src="app/ui/logo.svg" width="40" align="top"> irch

Private, peer-to-peer community chat shaped like Discord, with no server:
spaces, channels, threads, DMs, calls, files and notes, end to end
encrypted over [iroh](https://iroh.computer). A Rust core, a terminal
client, and one Tauri app for desktops, Android and iPhones.

## Install

Each keeps itself up to date.

```sh
irm https://github.com/cab-ucf/irch/releases/latest/download/install.ps1 | iex  # Windows (x64; Arm emulates it)
curl -fsSL https://github.com/cab-ucf/irch/releases/latest/download/install.sh | sh  # macOS (Apple silicon), Linux
```

- **Android**: [irch.apk](https://github.com/cab-ucf/irch/releases/latest/download/irch.apk),
  or track this repo in [Obtainium](https://obtainium.imranr.dev).
- **iPhone**: [irch.ipa](https://github.com/cab-ucf/irch/releases/latest/download/irch.ipa)
  in [AltStore](https://altstore.io) or [SideStore](https://sidestore.io).
- **Storage node** (always on): a space's *Storage Nodes → Run one on this
  computer*, or `install.sh … | sh -s -- --server`, optionally with
  `--push-relay https://irpush.you.example`.
- **NixOS**: `inputs.irch.url = "github:cab-ucf/irch";` and
  `irch.nixosModules.default`: the app and a storage node, built from
  signed releases. `services.irch.enable = false;` leaves the node out;
  `services.irch.pushRelayFile` names a file holding your irpush relay.
- **From source**: `cargo run --release -p irch` (terminal; `--serve` for a
  storage node); `cd app && cargo tauri dev`, or
  `cargo tauri android build --apk`; on a Mac `cargo tauri ios init`, then
  `cargo tauri ios dev`. Linux needs webkit2gtk-4.1, gtk3, libsoup3, ALSA
  and, for calls, gstreamer plugins base, good, bad and libav.

Other modes of `irch`: `--web [ADDR]` (the app's page from your own node,
default 127.0.0.1:4724, and a link through
[irp](https://github.com/cab-ucf/irp)'s page; the browser holds only a
token), `--api` (JSON on stdin/stdout, for bots and bridges), `--headless`,
`--doctor [WHO]` (times each route to someone; rings nothing).

## Features

- Spaces, channels, threads, DMs, contacts, *Note to self*; replies,
  reactions, edits, pins, @mentions, Markdown, Typst or LaTeX math.
- Messages up to 16,000 characters, with Copy.
- Link previews made by the sender's device; papers (arXiv, DOIs, PubMed,
  ScienceDirect, Nature) show title, authors, venue, year and abstract,
  from arXiv, PubMed or Crossref rather than the publisher.
- Files sealed in chunks: up to 5 GB kept by storage nodes (more with
  `--big-password`), any size shared from your device. Media to 25 MB is
  kept by the space's storage nodes, so it shows inline with the sender
  away.
- Voice and video calls with screen sharing; a call rings all of someone's
  devices, answered or declined on one, the others stop.
- Delivery marks: here only, left here, kept by a storage node, on their
  device, read; counts in groups.
- Notifications per space (All, Mentions, Off), set once per account;
  *Silent* per device.
- Short codes (`k7m-x9q-p3a`, one minute, one try) for invites and device
  links; optional password.
- Search, Ctrl+K, vim keys; profiles, linked devices, themes, *Network
  use* and *Check connections*.
- *Add to notes* ([loroh](https://github.com/cab-ucf/siroh)) and *Copy Link*
  ([tydig](https://github.com/cab-ucf/tydig)).

## How it works

- **Identity**: one seed is you on every device; Ed25519 signatures. Your
  own private space records what you joined and your settings.
- **Spaces** are grow-only sets of sealed ops (XChaCha20-Poly1305 under the
  epoch key); state is a deterministic fold in `(time, id)` order.
- **Bans rekey**: the new key is boxed to each remaining member with X-Wing
  (ML-KEM-768 + X25519). Gossip topics and sync proofs derive from the
  key, so whoever was cut off cannot fetch even ciphertext.
- **Sync** compares per-day fingerprints and sends only missing ops. Spaces
  up to 64 devices dial ops to each device directly; larger ones gossip.
- **Lanes**: each device holds one irpush mailbox on a relay
  (irpush.cxn.center by default), shared by its apps. Small ops travel there as labelled
  events (the relay sees no space); acks are MACs. Unacked in 3 s, an op
  is dialed over iroh.
- **iroh only for links**: irch's endpoint runs only while a sync, file or
  call needs it, woken by a dial frame on the lane, and closes a minute
  after nothing is open. Storage nodes stay up.
- **Waking**: a sleeping device is woken only for what it would show (DMs,
  calls, posts by its notification level). It shows the sealed op from
  keys its node left, with no connection opened. Typing, presence and
  reads go only to devices that are up.
- **Storage nodes** keep ciphertext, never keys, so messages wait while
  everyone is offline; they keep only epochs their owners name. Each is a
  push server: phones register their mailbox with it, and it rings them
  sealed to each. Without one, a phone is rung on its lane (retried at 3,
  10, 45 s). Nothing is queued: a phone away catches up as it opens.
- **Calls** are a QUIC mesh: Opus datagrams; video in the best codec all
  decode (AV1, VP9, VP8 or H.264). Native voice uses WebRTC's echo
  canceller (sonora), except on iPhones.

| network | terminal (app env) | who else is involved |
|---|---|---|
| default | | relays irpush.cxn.center, iroh.cxn.center (a storage node's first), then n0's; n0 DNS |
| own relays | `--relay URL` (`IRCH_RELAY`) | your relays |
| LAN only | `--lan` (`IRCH_LAN=1`) | nobody (mDNS) |

A device holds one relay at a time, best first. Relays are open; a relay
key (from whoever runs it) lifts its rate limit for relayed traffic.

## Security and limits

- Ops are sealed end to end; relays and storage nodes see tags, times,
  sizes and endpoint ids. Peers see each other's addresses.
- Every TLS and QUIC handshake is post-quantum (X25519MLKEM768,
  aws-lc-rs); signatures are Ed25519.
- No forward secrecy within an epoch; keys rotate on bans and on request.
- Authors date their own ops (at most ten minutes ahead), so a demoted
  admin's backdated ops stand until undone.
- A linked device is you: lost, it is retired with the whole identity.
- A push server learns when a ring comes and for which space; irpush's
  relay learns when and the phone's address. Ring tokens and labels are
  per space and die with a rekey.
- Presence is said, not polled: a device that dies without `Gone` looks
  present for about an hour.
- An iPhone in the background is not told of messages (no APNs host).
- Calls are for a handful of people; Macs and iPhones need WebKit 26.
- First installs trust GitHub's TLS (and irch's key where `minisign`
  exists); updates are always checked. Unaudited.

## Layout

| | |
|---|---|
| `core/` | ops, crypto, fold, store, sync, node, push, the app's JSON API |
| `tui/` | `irch`: terminal client, `--serve`, `--install`, `--web`, `--api` |
| `app/` | Tauri 2 shell; `ui/` the page; `gen/android` the Android project |

Plan: [PLAN.md](PLAN.md). Tasks: [TODO.md](TODO.md). GPL-3.0-or-later.
