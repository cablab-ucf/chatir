# Notes for Claude

- The one rule, as Linux never breaks userspace: chatir never breaks
  communication. A message, ring, device text or file that reached a
  device must always reach it, at once, by push, whatever version either
  side runs. A change that could stop one is wrong, whatever else it gains;
  when unsure, change less. What a sleeping device reads (the phonebook)
  stays readable across versions, and a sender's own push trouble never
  keeps anyone else from being woken.
- CI and release builds are deliberate, never routine: Actions minutes are
  scarce (private repos' Actions do not run; chatir's ci.yml is off by hand).
  Verify locally (`cargo fmt`, `clippy -D warnings`, tests). Never dispatch a
  workflow or push a tag to see whether something builds. A release is built
  once, when everything it ships is verified, merged and built as it should
  ship; not a version that is replaced hours later.
- The transport stays compatible with peers from v0.4.5 on: change it only
  when nothing else works, and then so that old and new still talk. That
  is the ALPNs, `PROTO` and every KDF context, ops, envelopes and codes
  (op.rs), sync, device sends and texts, calls and voice frames, push
  frames and lanes (pushir), relay TLS (irnet), short codes (pairir), and
  the iroh-relay version the lab's relay speaks. With postcard only an
  enum variant appended at the end is compatible; for anything else the
  old form stays beside the new. `core/src/wire.rs` pins it (pushir's
  `the_wire_is_as_chatir_v0_4_5` too): repin only for such a change, and
  say so in the PR.
- Messages, calls, device texts and files reach every device at once and
  always: asleep or not, whatever other apps do. A change that could delay
  or drop one (a wake, a retry, a lane, a nap, a permission) comes with a
  test that it still arrives, and nothing fails silently: what cannot be
  delivered is said.
- Every communication goes one way. A push on the device's lane wakes it
  and carries what is small: a message, a ring, a device text, a notice
  that a file waits. It is shown with no node running and acked back.
  What is larger, or a stream (sync, files, calls), follows on an irnet
  connection, which falls back to standard iroh relays (`irnet::fallback`)
  when the lab's do not answer. Nothing waits on the other side's app
  being open. The push mailbox is what is always on (a ping minutes or
  hours apart); iroh starts when something must stream and rests after.
- What keeps pushes alive, learnt the hard way (0.4.8 to 0.4.14):
  - On Android the process outlives the activity: pushir's foreground
    service and the phone's mailbox live in it. Tauri must never exit on
    its last window (`ExitRequested { code: None }` is prevented); opened
    again, `MainActivity` starts a fresh process for the page.
  - Native code reaches Kotlin through the activity only while one exists
    (`android::PAGE`): wry panics without one, and `panic = "abort"`
    kills the process, mailbox and all. Whatever a push brings is shown
    by Kotlin from `Wake.ring` (`alert`), never through the Bridge.
  - Timers stop while a phone sleeps (tokio, `Instant`): what must happen
    asleep runs on an alarm, a received packet or a wall-clock check, and
    holds a wakelock while it works.
  - A push taken asleep is shown with no node and acked to its sender; a
    running node takes a pushed op even without its sender's lane; our
    own lane's trouble never stops a wake (`push::send`).
  - What keeps a device from being reached is said in the health bar (the
    push service not held, notifications or a channel off, the lane
    unproved), never dropped.
  - Done means seen on phones as people use them: two accounts, the
    receiver swiped from recents and locked; a DM, a channel message, a
    call, a device text each ding within seconds and tick ✓✓. Tests and
    the live relay first (solo.rs's two accounts, run once with
    `Net::Default` against irpush.cxn.center); never call it fixed from
    tests alone.
