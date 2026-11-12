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
- [ ] Drop rings through storage nodes (wake.rs, push.rs's server): lanes
      are sealed and the lab's relay keeps frames, so they add a hop only
- [ ] A dial frame that carries the dialer's address, answered with the
      callee's: reach whoever homes on a relay we did not guess
- [ ] Measure the lane's proof (a frame to ourselves every five minutes)
      on a laptop's battery

## From the review, open
- [ ] The fold filters a batch against the state before it: an unban and
      the unbanned's post in one sync leave the post refused for good
- [ ] A storage node charges a file to the day's budget when asked, not
      when it has it; files and ops share one budget
- [ ] A demoted admin backdates admin ops (authors date their own)
- [ ] `--web`: the token is permanent, and the link through irp's page
      hands it to that page; a token per run, the link behind its own flag
- [ ] Images load on sight up to 25 MB each, with no pixel bound
- [ ] The installers check a signature's file, not its version, and
      nothing where minisign is absent
- [ ] Android: FLAG_SECURE under "private"; the caller's name shows on a
      locked screen; `hold`/`release` of the context race
- [ ] Build the Android project (pushir's library under
      `android.builtInKotlin=false`: build.gradle.kts applies its plugin)

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
