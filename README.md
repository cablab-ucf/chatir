# <img src="app/ui/logo.svg" width="40" align="top"> chatir

Private, peer-to-peer community chat shaped like Discord, with no server:
spaces, channels, threads, DMs, calls, files and notes, end to end
encrypted over [iroh](https://iroh.computer). A Rust core, a terminal
client, and one Tauri app for desktops, Android and iPhones.

## Install

Each keeps itself up to date.

```sh
irm https://github.com/cablab-ucf/chatir/releases/latest/download/install.ps1 | iex  # Windows (x64; Arm emulates it)
curl -fsSL https://github.com/cablab-ucf/chatir/releases/latest/download/install.sh | sh  # macOS (Apple silicon), Linux
```

- **Android**: [chatir.apk](https://github.com/cablab-ucf/chatir/releases/latest/download/chatir.apk),
  or track this repo in [Obtainium](https://obtainium.imranr.dev).
- **iPhone**: [chatir.ipa](https://github.com/cablab-ucf/chatir/releases/latest/download/chatir.ipa)
  in [AltStore](https://altstore.io) or [SideStore](https://sidestore.io).
- **Storage node** (always on): a space's *Storage nodes → Run one on this
  computer*, or `install.sh … | sh -s -- --server`, optionally with
  `--push-relay https://pushir.you.example`.
- **NixOS**: `inputs.chatir.url = "github:cablab-ucf/chatir";` and
  `chatir.nixosModules.default`: the app and a storage node, built from
  signed releases. `services.chatir.enable = false;` leaves the node out;
  `services.chatir.pushRelayFile` names a file holding your pushir relay.
- **From source**: `cargo run --release -p chatir` (terminal; `--serve` for a
  storage node); `cd app && cargo tauri dev`, or
  `cargo tauri android build --apk`; on a Mac `cargo tauri ios init`, then
  `cargo tauri ios dev`. Linux needs webkit2gtk-4.1, gtk3, libsoup3, ALSA
  and, for calls, gstreamer plugins base, good, bad and libav.

**From 0.4.2 and before**: 0.4.3 starts afresh, its data in a new folder
(`v1` in the old one, which it leaves as it was): make your identity on one
device and link the others. Keys, ids and codes no longer name the app, so a
rename never changes them again.

Other modes of `chatir`: `--web [ADDR]` (the app's page from your own node,
default 127.0.0.1:4724; the browser holds only a token, new each run; with
`--web-link`, also a link from anywhere through
[irp](https://github.com/cablab-ucf/irp)'s page, which then holds it),
`--api` (JSON on stdin/stdout, for bots and bridges), `--headless`,
`--doctor [WHO]` (times each route to someone; rings nothing).

## Features

- Spaces, channels, threads, DMs, contacts, *Note to self*; replies,
  reactions, edits, pins, @mentions, Markdown, Typst or LaTeX math.
- Messages up to 16,000 characters, with Copy.
- Link previews made by the sender's device; papers (arXiv, DOIs, PubMed,
  ScienceDirect, Nature) show title, authors, venue, year and abstract,
  from arXiv, PubMed or Crossref rather than the publisher.
- Files sealed in chunks: up to 5 GB kept by storage nodes (more with
  `--big-password`; the app sends 64 MiB and up directly, below), any size
  shared from your device. Media to 25 MB is kept by the space's storage
  nodes, so it shows inline with the sender away.
- Files straight to a device, as scp: your own, or one you allowed by its
  key (as ssh's `authorized_keys`: the data folder's `allowed`, a key and
  name a line; each side allows the other), taken at once into Downloads
  (a storage node: `received` in its folder); in a chat, those from 64 MiB.
  Nothing hashed first, many streams over eight connections, each 4 MiB
  block checked, resumed after a drop or a restart (4 Gbit/s over
  loopback, both ends on one 4-core machine). One sent to a device that
  is off waits: its lane wakes it, it takes the file once up, and the
  sender stays up (asking before it quits) till it is done.
- Voice and video calls with screen sharing; a call rings all of someone's
  devices, answered or declined on one, the others stop.
- Delivery marks: here only, left here, kept by a storage node, on their
  device, read; counts in groups.
- Notifications per space (All, Mentions, Off), set once per account;
  *Silent* per device.
- Short codes (`k7m-x9q-p3a-b2c`, one minute, one try, a PAKE:
  [pairir](https://github.com/cablab-ucf/pairir)) for invites and device
  links; optional password. They are met at a relay: not on LAN only.
- Search, Ctrl+K, vim keys; profiles, linked devices, themes, *Network
  use* and *Check connections*.
- *Add to notes* ([loroh](https://github.com/cablab-ucf/siroh)) and *Copy link*
  ([tydig](https://github.com/cablab-ucf/tydig)).

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
- **Lanes**: each device holds one [pushir](https://github.com/cablab-ucf/pushir)
  mailbox on a relay (irpush.cxn.center by default), shared by its apps.
  Small ops travel there, sealed under the lane's secret and padded to one
  size (the relay sees a mailbox and a time); acks are MACs. One relay
  connection carries a burst. Unacked in 3 s, an op is dialed over iroh.
- **iroh only for links**: chatir's endpoint runs only while a sync, file or
  call needs it, woken by a dial frame on the lane, and closes a minute
  after nothing is open. Storage nodes stay up.
- **Waking**: a sleeping device is woken only for what it would show (DMs,
  calls, posts by its notification level). It shows the sealed op from
  keys its node left, with no connection opened. Typing, presence and
  reads go only to devices that are up.
- **Storage nodes** keep ciphertext, never keys, so messages wait while
  everyone is offline; they keep only epochs their owners name. A phone is
  rung on its lane by whoever writes (retried at 3, 10, 45 s), and the
  lab's relay keeps a frame until its mailbox has it (64, a day); one away
  longer catches up with its storage nodes as it opens. A storage node
  tells its devices the relay their lanes go on, if it runs one.
- **Calls** are a QUIC mesh: Opus datagrams; video in the best codec all
  decode (AV1, VP9, VP8 or H.264). Native voice uses WebRTC's echo
  canceller (sonora), except on iPhones.

| network | terminal (app env) | who else is involved |
|---|---|---|
| default | | irpush.cxn.center (pushes and links; pushes on a storage node's own first), then iroh.cxn.center, then n0's relay, each only while those before are down; no DNS, no lookup service |
| own relays | `--relay URL` (`CHATIR_RELAY`) | your relays |
| LAN only | `--lan` (`CHATIR_LAN=1`) | nobody (mDNS) |

The endpoint is [irnet](https://github.com/cablab-ucf/irnet)'s, as in every
app of the lab: it homes on irpush.cxn.center (the relay with post-quantum
TLS), joins iroh.cxn.center once that has not answered for 10 s, then
n0's, and returns as soon as a better one answers. With no lookup service,
a device is dialed on the lab's relays, on ours of the moment, and at the
addresses it was met at. A device proves its lane every five minutes (a
frame to itself through the relay) and lets iroh sleep only behind one
that answered: with pushes down, iroh stays up and is dialed.
Relays are open; a relay key (from whoever runs it) lifts its
rate limit for relayed traffic.

## Security and limits

- Ops are sealed end to end; relays and storage nodes see tags, times,
  sizes and endpoint ids. Peers see each other's addresses.
- A file sent device to device is checked block by block as its
  sender's device read it, not against a hash in a post: that device
  vouches for it (one rewritten since it was offered is not sent).
- Every QUIC handshake is post-quantum (X25519MLKEM768, aws-lc-rs) or
  refused, as is the hop to the push relay; signatures are Ed25519.
- No forward secrecy within an epoch; keys rotate on bans and on request.
- Authors date their own ops (at most ten minutes ahead), so a demoted
  admin's backdated ops stand until undone.
- A linked device is you: lost, it is retired with the whole identity,
  and whoever holds it can still add ops dated before the retirement.
- pushir's relay learns when a device is sent to and its address, not by
  whom, for which space, or how much. A space's labels die with a rekey.
- Presence is said, not polled: a device that dies without `Gone` looks
  present for about an hour.
- An iPhone in the background is not told of messages (no APNs host).
- Calls are for a handful of people; Macs and iPhones need WebKit 26.
- First installs check chatir's key where `minisign` exists; without it,
  they ask before trusting GitHub's TLS alone. Updates are always checked.
  Unaudited.

## Layout

| | |
|---|---|
| `core/` | ops, crypto, fold, store, sync, node, push, the app's JSON API |
| `tui/` | `chatir`: terminal client, `--serve`, `--install`, `--web`, `--api` |
| `app/` | Tauri 2 shell; `ui/` the page; `gen/android` the Android project |

Plan: [PLAN.md](PLAN.md). Tasks: [TODO.md](TODO.md). GPL-3.0-or-later.
