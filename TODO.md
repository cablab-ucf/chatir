# TODO

Design: [PLAN.md](PLAN.md). pushir's own list: pushir's TODO.md.

## Next: servers anyone can have
- [ ] **Storage nodes fan out large channels**: a phone sends one copy to
      its storage node, which sends it on to every device of the space
      (one upload, not a thousand; the relay sees nothing it did not)
- [ ] **Push-button VPS**: a provider's API token in, a pushir relay and
      a storage node out (free or cheap tiers), told to your devices
- [ ] **A personal desktop as that server**, reachable from anywhere
      through iroh: no router settings, no open ports

## Reach and rings
- [ ] Probe only on a network change, not every 20–26 s
- [ ] Patch iroh's relay ping and net_report until #4589/#4590/#4651 land
- [ ] Watch iroh #4665: a pooled link pinned to a stale path
- [ ] Measure the direct-path rate, and the doctor's numbers on real devices
- [ ] The ring carries the caller's address; a woken phone dials back
- [ ] DM calls: the callee dials the caller, not "lower id dials"

## Lanes
- [ ] A desktop daemon's relay is assumed pushir's (`pushir::relay`)
- [ ] pushir on Windows has no daemon (`local` is Unix's): each app holds
      a mailbox of its own there
- [ ] A dial frame that carries the dialer's address, answered with the
      callee's: reach whoever homes on a relay we did not guess
- [ ] Measure the lane's proof (a frame to ourselves every five minutes)
      on a laptop's battery

## From the review, open
- [ ] Android and iOS code edited since v0.4.0 built them ("private"
      keeps chatir out of screenshots and the lock screen, Alerts checks
      pushir's owner, iOS backups exclude the data folder): built only by
      a release run, which the next one is
- [ ] Run the app (each OS) with its narrowed CSP and capabilities
      (`ipc:` only in connect-src; event listen/unlisten and clipboard
      write alone): built and validated, never run
- [ ] Whom the walk ever made an admin (or named owner) stays walked: a
      demoted admin's flood of backdated ops refolds each (a member's
      never: set apart, or judged where dated)
- [ ] A stolen seed's thief, retired by `Burn`, still posts ops dated
      before it (authors date their own). A burn could list its author's
      ops as its device holds them and refuse the rest (a hash of them
      tells no peer which: the ids, 32 bytes each), but the thief holds
      the seed and burns too, dated earlier, listing its own: only what
      every burn lists is sound (a thief erases, never adds). A new op
      0.4.0 drops, and a pass before the walk (as `forfeit`): not yet
      worth it. A demoted admin backdating admin ops: the same list on
      the demoting `Role`, the same cost

## Files device to device (`send.rs`)
- [ ] The app and `--web` let `send_to` read only a picked file, and
      `accept` write only where the user chose, as they gate offers
- [ ] Each stream waits a round trip between blocks: ask the next before
      the last ends, for links far away (100 ms: about 7 Gbit/s at 8×8)
- [ ] Fewer streams on a slow link: 64 blocks in flight finish late, so
      little is kept if it drops early
- [ ] A chat offer's sender sees no transfers (who fetched, how far)
- [ ] Measure between two machines, and on a phone's battery

## Delivery
- [ ] One DM per person: one row, histories merged; `dm` and calls use the
      DM both are in
- [ ] Two DMs begun at once, both talked in before they meet: two rows
- [ ] Message one device of someone else's
- [ ] Remove gossip's code if no space needs it (over 64 devices)
- [ ] Measure traffic on real devices (Network use)

## Done (for context)
- [x] Small ops as pushir events on each device's lane; acks are MACs
- [x] iroh only while a link needs it, woken by a dial frame
- [x] Notification levels per account; Silent per device
- [x] Short codes for invites and device links
- [x] irnet's endpoint, n0's relay only as its fallback, no lookup service
- [x] One relay connection per burst; iroh sleeps only behind a proved lane
- [x] No push servers: a lane wakes, a storage node keeps
- [x] irpush.cxn.center keeps a frame for a mailbox that is away (seen
      after 90 s, 2026-11-12)
