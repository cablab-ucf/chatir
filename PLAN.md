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

- Every device has a lane: a secret of chatir's on the device's pushir
  mailbox (Android's library or a desktop's daemon, shared by its apps),
  else on a mailbox of its own. pushir seals each frame under the lane's
  secret and pads it to 1024 bytes: the relay routes it and reads nothing.
- A message is `kind ‖ 32 bytes ‖ rest`: an op under its space's label,
  its ack (a MAC only a member makes), or a dial (start iroh, a link
  comes). Empty, the mailbox is back from a gap: catch up.
- One relay connection carries a burst and closes 20 s after its last
  frame. An unacked op is dialed after 3 s, and its ack still counts late.
- A lane is trusted only while it answers: every five minutes, and each
  time its mailbox connects, a device sends its own lane a frame through
  the relay. Unheard in 10 s, iroh comes up and stays up, and the device
  catches up; members' frames fail at once and they dial instead.
- iroh 1.4.1 cannot drop its home relay at runtime, so sleeping is closing
  the endpoint and waking is binding it again (same key, same port).

## Relays: best first, each enough alone

irnet binds the endpoint, as for every app of the lab, on the best relay
that answers: irpush.cxn.center (pushir's: X25519MLKEM768, a ping hours
apart), then iroh.cxn.center (a stock relay: X25519 only on that hop),
then n0's. The next joins once those before it have not answered for 10 s
and leaves as soon as a better one answers (`irnet::fallback`). A peer's
handshake is post-quantum or refused, whatever the relay. Lanes ride
irpush.cxn.center too, or a storage node's own relay. No lookup service
is asked where a device is (n0's DNS learned who looks for whom): a device
is dialed on the lab's relays, on ours of the moment, and at the addresses
it was met at. Our own relays (`--relay`) carry both. Relays are open;
abuse is bounded by per-client rates, connection caps and idle timeouts,
and a key from whoever runs one lifts its rate limit.

| down | what still works |
|---|---|
| irpush.cxn.center | no lanes: iroh stays up on every device, homed on iroh.cxn.center within 10 s, and is dialed |
| iroh.cxn.center | everything: it is only the second home |
| both | links on n0's relay and direct; iroh stays up |

## Waking: only for what would show

A sleeping device is rung for DMs, calls and posts at its notification
level, with the sealed op itself, and shows it without starting its node.
Levels belong to the account (its own space, kept by storage nodes);
Silent belongs to the device. Typing, presence and reads go only to
devices that are up. A frame is sent again at 3, 10 and 45 s until acked;
a phone away longer catches up as it opens. pushir's relay can keep a
frame until its mailbox has it (`relay-hold`); irpush.cxn.center as
deployed does not yet (TODO).

## Delivery by device list

A space is users, each with devices. Up to 64 devices, ops are dialed to
each device (who wrote, storage nodes, devices met, our own); a sync is
how a device is first met. Larger spaces still gossip; gossip's code goes
if no space needs it, or storage nodes take over fan-out (TODO).

## No push servers

A storage node used to ring phones for members. It queued nothing and
sent through the same relay a member reaches itself, so it added a hop
and a registry of phones: gone. A storage node keeps ops; a lane wakes.

## Short codes

Long codes are bearer secrets (an invite holds the space key, a link code
the seed). A short code (`k7m-x9q-p3a-b2c`) is pairir's: its first half
finds the giver at the relay, the rest runs CPace, a PAKE, so nothing on
the wire lets a code be tested offline; one try, one minute. The long code
comes back sealed to the taker's fresh X-Wing key; an optional password
adds an Argon2id layer. If someone else spends a code, its giver is told.

## Open directions

- Servers anyone can have: storage-node fan-out, push-button VPS, a
  personal desktop reachable through iroh (TODO).
- Probe only on a network change; patch iroh's fixed timers until upstream
  (#4589, #4590, #4651) makes them configurable.
- One DM per person, histories merged.
- Later, if simple: Tor or Nym routing, scheduled rekeys and per-sender
  chains, safety numbers, per-device keys, an outside audit.
