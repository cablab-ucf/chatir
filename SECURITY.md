# Security

chatir is end-to-end encrypted group and direct chat, calls and files over
iroh, with pushes through pushir and pairing through pairir. This file is
the threat model the code is built and reviewed against. Report a
vulnerability privately to the maintainers (GitHub security advisory on
this repository), not in an issue.

## Who is trusted with what

| Party | Can | Cannot |
|---|---|---|
| Network, relay operator (irnet, pushir) | see endpoint ids, timing, sizes; drop or delay | read or forge ops, frames, calls; ack for a device |
| Storage node (yours) | keep and serve sealed ops of spaces you name; see sizes, times, device and user ids that sync with it | read content; forge or reorder ops; ack an op it does not hold |
| Non-member (knows a device id) | open at most 16 links to it (256 in all), each closed when silent | join, read, sync, wake a device, hold iroh open for long |
| Member | read and write the space; claim any device id in its own ops; see members' devices and lanes | forge another member's ops or acks (acks are signed by the device, v0.5.0); silence a device whose owner was proven to it |
| Admin | change roles, rekey, ban; date control ops back | reuse an epoch's tag (counts for nothing); make devices disagree on a ban (control ops are judged at their place) |
| Removed or banned member | keep what it read; be told it is out | read anything written after the rekey that removed it |
| Device thief (unlocked, data readable) | everything that device holds | anything other devices write after it is removed |

## Cryptography

- Ops: XChaCha20-Poly1305 under the space's epoch key, random 24-byte nonces;
  AAD binds space, epoch, time and liveness. Each op is signed (Ed25519,
  the user's key) over protocol, space, epoch, time, device and op; its id
  excludes the signature, so malleability changes nothing.
- Rekeys: a fresh 256-bit key, boxed to each member with X-Wing
  (ML-KEM-768 + X25519); a key is adopted only if its tag matches and an
  admin set it. Epoch tags are 8 bytes (see limits).
- Transport: iroh QUIC with TLS 1.3, X25519MLKEM768 only (irnet); peers that
  do not offer it are refused. Calls ride the same connections.
- Files: a random key per file; chunked AEAD with the index and a last-chunk
  flag in the nonce, so truncation, reordering and extension fail.
- Push frames (pushir): per-frame keys from a 192-bit nonce, padded to 1 KiB,
  128-bit route tags; labels and acks are KDFs under the space key, and acks
  are signed by the acking device (`acks:signed` in its lane).
- Pairing and device links (pairir): CPace short codes (one online guess per
  code), X-Wing sealing to the taker.
- Updates: minisign (Ed25519) over file and version; downgrades refused.

## Delivery guarantees (v0.5.0)

A message, ring, device text or file notice is kept in a persistent outbox
before its first push and pushed again (start, network back, backoff to
daily) until the device it is for acks it, syncs with us, or 30 days pass
(logged). A receiver acks only what it stored; a device's ack ends that op's
retries alone. Asleep or running, an op is shown once (`rung`, shared under a
lock), including one dated before what was shown or read (it comes back
unread). Nothing is dropped silently: refusals are returned to the sender.

## Known limits (accepted, with reason)

- No forward secrecy inside an epoch, and `Back` hands every older key to
  holders of the current one: newcomers read history by design. A leaked
  current key reads all history; rekey after a suspected leak.
- The own space's key derives from the seed and never rotates. The seed,
  device key and `phonebook.json` (current space keys, for a sleeping phone)
  are files of mode 0600 in a 0700 folder: full-disk or Android file
  encryption is assumed. Secrets are not zeroized in memory.
- Admins may date control ops back (roles, rekeys); devices converge on the
  same result whatever order ops arrive in.
- Epoch tags are 8 bytes: an admin can find two keys with one tag in about
  2^32 work and split members between them. Admins are trusted with keys.
- An admin's garbage rekey box is not detected by others; the member left
  out sees the space locked and the head can rekey.
- Hosts an admin listed stay listed after that admin is banned; members tell
  hosts the user id behind each device (sync `Hello`).
- A member can claim another's device id in its own `Push`: until that
  device's owner is proven to a sender (a sync with it), the sender wakes
  the device by no lane there; messages still arrive by gossip and sync.
- Display names need not be unique; mentions match by name.
- Linking a device needs no confirmation on your other devices.
- Ops may be dated up to 10 minutes ahead (pins and topics hold that long).
- Link previews are fetched by the sender (the site sees its IP), off with
  the setting; https to public addresses only.
- Peers' voice is decoded by unsafe-libopus (c2rust libopus 1.3.1).
- Math is rendered by Typst with a sandbox denylist.
- If the lab's relays are blocked, the relay hop falls back to iroh's
  classical-TLS relays; peer links stay post-quantum.
- Spaces above 64 devices keep gossip, so iroh, up while the app runs.

## Wire compatibility

Peers from v0.4.5 on still talk (CLAUDE.md); `core/src/wire.rs` pins the
wire. v0.5.0 adds only what older peers skip: the `acks:signed` word in a
lane, and a signature after an ack.
</content>
</invoke>
