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
- [ ] A demoted admin backdates admin ops (authors date their own)
- [ ] `--web`: the token is permanent, and the link through irp's page
      hands it to that page; a token per run, the link behind its own flag
- [ ] The installers check a signature's file, not its version, and
      nothing where minisign is absent
- [ ] Android: FLAG_SECURE under "private"; the caller's name shows on a
      locked screen
- [ ] Build the Android project's Kotlin and Gradle (its Rust, `android.rs`
      and pushir's JNI, passes clippy for aarch64-linux-android)
- [ ] iOS: the data folder (seed, device key) goes to iCloud and computer
      backups: `NSURLIsExcludedFromBackupKey` on it, as Android's no_backup
- [ ] Whom the walk ever made an admin (or named owner) stays walked: a
      demoted admin's flood of backdated ops refolds each (a member's
      never: set apart, or judged where dated)
- [ ] A stolen seed's thief, retired by `Burn`, still posts ops dated
      before it (authors date their own)
- [ ] `--web` saves files up to 25 MB (`file` is in memory): a streaming GET
- [ ] The page's CSP and Tauri capabilities, narrowed to what it uses
      (`ipc:` only in connect-src; event and clipboard alone): run it after
- [ ] Alerts shows chatir's battery exemption, not that of the app that
      holds pushir's connection (`Pushir.ask` asks for it; pushir exposes
      no owner to show)
- [ ] File transfers run at a stranger's 8 MiB window: iroh-blobs' pooled
      connections get no `irnet::wide`

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
