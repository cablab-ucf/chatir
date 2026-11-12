# Plan

Goal: any device rings any other in under a second, on any relay, while an
idle phone spends under 1 % of its battery a day. One way that always works
beats three that sometimes do. Tasks: [TODO.md](TODO.md).

## One quiet socket per device

NATs forget idle UDP in ~30 s, so iroh pings every open QUIC path every
5 s, its relay every 15 s, and runs address discovery every 20–26 s. NATs
keep idle TCP for hours, so one TLS/TCP socket to a long-ping relay is
nearly silent and makes a NAT'd device reachable by id.

| state | holds | wakes by |
|---|---|---|
| asleep | its pushir lane (one TCP socket) | an event or ring on the lane |
| awake, idle | the lane; iroh closed a minute after last use | a dial frame |
| syncing, file, call | direct QUIC, closed when quiet | |

- Every device has a lane: chatir's slot on the device's pushir mailbox
  (Android's library or a desktop's daemon, shared by its apps), else its
  own mailbox.
- Frames are `kind ‖ 32 bytes ‖ rest`: an op under its space's label, its
  ack (a MAC only a member makes), or a dial (start iroh, a link comes).
- iroh 1.4.1 cannot drop its home relay at runtime, so sleeping is closing
  the endpoint and waking is binding it again (same key, same port).

## Relays: open, one at a time, best first

Relays are open; abuse is bounded by per-client rates on relayed traffic,
connection caps and idle timeouts. Privacy never rested on relay access:
ops are sealed end to end, the relay hop is post-quantum TLS. Whoever runs
a relay hands out keys that lift its rate limit. Order: a storage node's
pushir relay, irpush.cxn.center, iroh.cxn.center, n0's; a minute without
one, the next; the best again every half hour.

## Waking: only for what would show

A sleeping device is rung for DMs, calls and posts at its notification
level, with the sealed op itself, and shows it without starting its node.
Levels belong to the account (its own space, kept by storage nodes);
Silent belongs to the device. Typing, presence and reads go only to
devices that are up. With a storage node, rings queue for a phone that is
off; without one, the phone is rung live and the rest syncs later.

## Delivery by device list

A space is users, each with devices. Up to 64 devices, ops are dialed to
each device (who wrote, storage nodes, devices met, our own); a sync is
how a device is first met. Larger spaces still gossip; gossip's code goes
if no space needs it, or storage nodes take over fan-out (TODO).

## Push servers that form themselves

Every storage node is a push server. A phone registers its route (mailbox,
slot, relay and a key rings are sealed under) with the storage nodes it
syncs with; members ring all at once, so a ring gets through while any is
up. Ring tokens are per space and epoch: a rekey cuts off
whoever it left out.

## Short codes

Long codes are bearer secrets (an invite holds the space key, a link code
the seed). A short code (`k7m-x9q-p3a`, ~45 bits) is stretched with
Argon2id into a throwaway iroh identity and a proof, answered for 60 s,
one try. The taker sends a fresh X-Wing key; the long code comes back
sealed to it. An optional password adds an Argon2id-keyed layer. It is
the pairir library, one pairing for every app; `core/src/code.rs` adapts it.

## Open directions

- Servers anyone can have: storage-node fan-out, push-button VPS, a
  personal desktop reachable through iroh (TODO).
- Probe only on a network change; patch iroh's fixed timers until upstream
  (#4589, #4590, #4651) makes them configurable.
- One DM per person, histories merged.
- Later, if simple: Tor or Nym routing, scheduled rekeys and per-sender
  chains, safety numbers, per-device keys, an outside audit.
