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
