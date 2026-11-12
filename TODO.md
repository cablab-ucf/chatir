# TODO

Design: [PLAN.md](PLAN.md). pushir's own list: pushir's TODO.md.

## Next: servers anyone can have
- [ ] **Storage nodes fan out large channels**: a phone sends one copy to
      its storage node, which sends it on to every device of the space
      (one upload, not a thousand; the relay sees nothing it did not)
- [ ] **Push-button VPS**: a provider's API token in, an pushir relay and
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
