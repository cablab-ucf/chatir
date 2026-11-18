//! Push lanes: waking devices, what a push brings, the phonebook.

use super::*;

/// The phonebook as a sleeping device reads it: JSON, which an older or
/// newer chatir's added fields never break; before 0.4.12, postcard.
fn phonebook_of(dir: &Path) -> BTreeMap<Id, Page> {
    let json = std::fs::read(dir.join("phonebook.json")).ok();
    json.and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| store::load(&dir.join("phonebook")))
}

/// An op taken with no node running, acked to the lane of the device it
/// came from (as the phonebook has it): sent on the runtime we run in.
fn ack_asleep(dir: &Path, space: &Id, p: &Page, ts: u64, from: &EndpointId) {
    let to = p.lanes.get(from).and_then(|l| crate::push::lane_of(l));
    let me = std::fs::read(dir.join("device")).ok();
    let me =
        me.and_then(|k| k.try_into().ok()).map(|k| SecretKey::from_bytes(&k));
    let rt = tokio::runtime::Handle::try_current();
    let (Some(to), Some(me), Ok(rt)) = (to, me, rt) else { return };
    let a = ev_ack(space, &p.key, ts, &me.public());
    let ack = [&[crate::push::ACK][..], &a].concat();
    rt.spawn(async move { crate::push::send(&to, &ack).await });
}

/// What reaches our lane, while we hear it.
pub(super) async fn hear(
    n: Weak<Node>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::push::Heard>,
) {
    while let Some(b) = rx.recv().await {
        let Some(n) = n.upgrade() else { return };
        n.hear(&b);
    }
}

/// Our lane proved and told as soon as it is on a relay; later moves, by
/// the heartbeat.
pub(super) async fn lane_up(n: Weak<Node>) {
    for _ in 0..120 {
        let Some(n) = n.upgrade() else { return };
        if crate::push::lane_at(&n.dir).is_some() {
            n.check_lane().await;
            return n.tell_lane().await;
        }
        drop(n);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Acks a device sends back on a direct link we opened: `space ‖ ts`.
pub(super) async fn acks(c: Connection, n: Weak<Node>) {
    let d = c.remote_id();
    while let Ok(mut s) = c.accept_uni().await {
        let Ok(b) = sync::rest(&mut s, 40).await else { continue };
        let (Some((id, ts)), Some(n)) =
            (b.split_first_chunk::<32>(), n.upgrade())
        else {
            continue;
        };
        let Ok(ts) = <[u8; 8]>::try_from(ts) else { continue };
        n.acked(Id(*id), d, u64::from_le_bytes(ts));
    }
}

/// Whether `user`, at `level` in `s`, is shown `op` (as `Node::woken`
/// shows it): rung for that and nothing else. Unread, anything rings.
fn wants(s: &State, op: Option<&Op>, level: Level, user: &Id) -> bool {
    let dm = s.dm.is_some();
    match op {
        Some(Op::Post { text, reply, .. }) => match level {
            Level::All => true,
            Level::Off => false,
            Level::Mentions => {
                let tag = format!("@{}", s.display_name(user).to_lowercase());
                dm || text.to_lowercase().contains(&tag)
                    || reply
                        .and_then(|r| s.message(&r))
                        .is_some_and(|m| m.1.author == *user)
            }
        },
        Some(Op::Call { .. }) => {
            level != Level::Off && (dm || level == Level::All)
        }
        _ => true,
    }
}

/// A phonebook another chatir wrote, with a field this one lacks and none
/// of those added since: still read (an unreadable one left every push to
/// a sleeping device unshown).
#[test]
fn a_phonebook_reads_across_versions() {
    let page = Page {
        own: false,
        name: "lab".into(),
        dm: false,
        ring: true,
        level: Level::All,
        tag: "@me".into(),
        ours: vec![],
        key: [7; 32],
        names: BTreeMap::new(),
        rooms: BTreeMap::new(),
        me: None,
        lanes: BTreeMap::from([(
            SecretKey::from_bytes(&[3; 32]).public(),
            "pushir:x".into(),
        )]),
    };
    let mut v =
        serde_json::to_value(BTreeMap::from([(Id([1; 32]), page)])).unwrap();
    let p = v.as_object_mut().unwrap().values_mut().next().unwrap();
    let p = p.as_object_mut().unwrap();
    for added in ["own", "ours", "me", "lanes"] {
        p.remove(added);
    }
    p.insert("from_a_newer_one".into(), serde_json::json!(1));
    let dir = std::env::temp_dir()
        .join(format!("chatir-pb-{}", u64::from_le_bytes(rand())));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("phonebook.json"), serde_json::to_vec(&v).unwrap())
        .unwrap();
    let pb = phonebook_of(&dir);
    assert_eq!(pb.get(&Id([1; 32])).map(|p| p.key), Some([7; 32]));
    let _ = std::fs::remove_dir_all(dir);
}

/// One at a time, across threads and processes, the small files a sleeping
/// device and its node share are read and rewritten (`rung`, `shown`,
/// `handed`): each frame comes on a thread of its own, and a node may be
/// starting or stopping. Not `lock`: a running node holds that one. No
/// folder yet: as it is.
pub(super) fn shared<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let mut o = std::fs::OpenOptions::new();
    let l = o
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("shared.lock"));
    let held = l.ok().filter(|l| l.lock().is_ok());
    let r = f();
    if let Some(l) = held {
        let _ = l.unlock();
    }
    r
}

/// Ops this device has (by a ring, or folded by its node), newest last:
/// what comes again is not shown again.
const RUNG: usize = 2048;
/// Dated this long ago, a ring is not shown asleep: unread, it waits.
const WEEK: u64 = 7 * 24 * 3600 * 1000;

/// `ids` noted as had (`rung`).
pub(super) fn rung_add(dir: &Path, ids: &[Id]) {
    let f = dir.join("rung");
    shared(dir, || {
        let mut rung: Vec<Id> = store::load(&f);
        let new: Vec<Id> =
            ids.iter().filter(|i| !rung.contains(i)).copied().collect();
        if new.is_empty() {
            return;
        }
        rung.extend(new);
        rung.drain(..rung.len().saturating_sub(RUNG));
        let _ = store::save(&f, &rung);
    })
}

impl Node {
    /// Left for a sleeping phone (`peek`): each space's key, as on disk
    /// beside the seed that unlocks them all anyway.
    pub(super) fn phonebook(&self) {
        let me = self.id();
        let mut pb = BTreeMap::new();
        for sp in self.all() {
            let Some(inv) = &sp.inv else { continue };
            let (loud, level) = (self.loud(&sp.id), self.level(&sp.id));
            let ids: Vec<Id> = self
                .view(&sp.id, |s| s.members().into_iter())
                .into_iter()
                .flatten()
                .filter(|m| m.role != Role::Banned && m.id != me)
                .map(|m| m.id)
                .collect();
            let names = ids
                .iter()
                .map(|u| (*u, self.name_of(u, None).unwrap_or_default()));
            let names = names.collect();
            let lanes = self.lanes(&sp.id).into_iter();
            let mut lanes: BTreeMap<_, _> =
                lanes.map(|(d, l)| (d, l.to_string())).collect();
            if lanes.len() > FANOUT {
                lanes.clear(); // a big space's would weigh on every push
            }
            let b = lock(&sp.book);
            let (key, locked) = b.key(inv);
            let (s, dm) = (&b.st, b.st.dm.is_some());
            let ring = level != Level::Off && (dm || level == Level::All);
            let tag = format!("@{}", s.display_name(&me).to_lowercase());
            let ours = s.recent_by(&me, 200);
            let page = Page {
                own: sp.id == self.own,
                name: s.name().to_string(),
                dm,
                ring: ring && loud,
                level,
                tag,
                ours,
                key,
                names,
                rooms: s.rooms().into_iter().collect(),
                me: Some(me),
                lanes,
            };
            if !locked {
                pb.insert(sp.id, page);
            }
        }
        // JSON: a field added later (`serde(default)`) never leaves a
        // sleeping device unable to read it, so every push unshown.
        let json = serde_json::to_vec(&pb).unwrap_or_default();
        let r = store::write(&self.dir.join("phonebook.json"), &json);
        let _ = r.inspect_err(|e| warn!("saving the phonebook: {e}"));
    }

    /// A frame on our lane with no node running, read from the phonebook:
    /// an op's news; `Some(None)` to wake for (a member's device wants a
    /// link to us, or a space we do not know yet); `None` for an ack.
    pub fn news(dir: &Path, b: &[u8]) -> Option<Option<crate::push::News>> {
        let pb = phonebook_of(dir);
        let label = |l: &Key| {
            pb.iter().find(|(i, p)| ev_label(i, &p.key) == *l).map(|x| *x.0)
        };
        match crate::push::split(b)? {
            (crate::push::OP, x, raw) => {
                Some(label(&x).map(|id| (id, raw.to_vec())))
            }
            (crate::push::DIAL, x, _) => label(&x).map(|_| None),
            _ => None,
        }
    }

    /// A text from one of our devices on our lane, with no node running:
    /// its notice (once), read under our own space's key in the phonebook,
    /// and its ack back to the device it came from.
    pub async fn text(dir: &Path, b: &[u8]) -> Option<Notice> {
        let (kind, l, rest) = crate::push::split(b)?;
        if kind != crate::push::TEXT {
            return None;
        }
        let pb = phonebook_of(dir);
        let mine = |(i, p): &(&Id, &Page)| p.own && ev_label(i, &p.key) == l;
        let k = pb.iter().find(mine)?.1.key;
        let ((from, _, back, name, text), mac) = open_text(&k, rest)?;
        if let Some(at) = crate::push::lane_of(&back) {
            let ack = [&[crate::push::ACK][..], &text_ack(&k, &mac)].concat();
            crate::push::send(&at, &ack).await;
        }
        if taken_text(dir, &from, &text, Some(&mac)) {
            return None;
        }
        let who = match name.is_empty() {
            true => from.fmt_short().to_string(),
            false => name,
        };
        let id = Id(*from.as_bytes());
        Some(Notice::new(id, Ch::default(), who, text, false, false))
    }

    /// A ring to a phone with no node running, handled without one: a
    /// call shown (or hushed); a message shown as our level asks, and kept
    /// for the node to fold (and check) when it next opens. Also whether
    /// that node must start now: for what the ring could not carry, from
    /// someone the phonebook does not know, or a DM only it can take.
    pub fn woken(
        dir: &Path,
        (space, raw): &crate::push::News,
    ) -> (Option<Notice>, bool) {
        crate::usage::note(dir, "woken", serde_json::json!({}));
        let pb = phonebook_of(dir);
        let opened = postcard::from_bytes::<Env>(raw).ok().and_then(|e| {
            let p = pb.get(space)?;
            let (s, _) = op::open(space, &p.key, &e, None)?;
            Some((p, e, s))
        });
        let Some((p, e, s)) = opened else { return (None, true) };
        // Dated ahead past the skew: refused here as `ingest` refuses it.
        if e.ts > now() + SKEW {
            return (None, false);
        }
        // Here now: acked to the device it came from, as a running node acks
        // an event, so its sender says it is on this device and rings no more.
        let ack = || ack_asleep(dir, space, p, e.ts, &s.device);
        // Once: a ring comes again until acked (its ack may be lost), and
        // what the node took another way is in `rung` too (`late`).
        let f = dir.join("rung");
        let log = dir.join("spaces").join(format!("{space}.log"));
        let had = shared(dir, || {
            let mut rung: Vec<Id> = store::load(&f);
            if rung.contains(&s.id) {
                return Some(true);
            }
            // Live ops (calls) pass; the rest wait in the log, then count
            // as rung (not before: a failed append is taken again).
            if !e.live && store::append(&log, raw).is_err() {
                return None;
            }
            rung.push(s.id);
            rung.drain(..rung.len().saturating_sub(RUNG));
            let _ = store::save(&f, &rung);
            Some(false)
        });
        match had {
            None => return (None, true),
            Some(true) => {
                ack();
                return (None, false);
            }
            Some(false) => ack(),
        }
        // Ours, from another device of ours: nothing to tell us, but the
        // node starts for it (a space joined there, a level set).
        if p.me == Some(s.author) {
            return (None, true);
        }
        // One the phonebook does not name yet (it is older than their
        // profile): shown all the same, by a short id (only a member opens
        // ops under this key), and the node starts to learn who.
        let unknown = !p.names.contains_key(&s.author);
        let who = p.names.get(&s.author).filter(|w| !w.is_empty());
        let who =
            who.cloned().unwrap_or_else(|| s.author.to_string()[..8].into());
        let room = |ch| p.rooms.get(ch).map_or("call", String::as_str);
        let at = |ch| match p.dm {
            true => who.clone(),
            false => format!("{who} in #{} ({})", room(ch), p.name),
        };
        let n = |room, title, body, ring, hush| {
            Notice::new(*space, room, title, body, ring, hush)
        };
        let shown = match s.op {
            Op::Post { channel, text, reply, .. } => {
                let mention = p.dm
                    || text.to_lowercase().contains(&p.tag)
                    || reply.is_some_and(|r| p.ours.contains(&r));
                let tell = match p.level {
                    Level::All => true,
                    Level::Mentions => mention,
                    Level::Off => false,
                };
                // Told now: the node does not tell it again.
                let f = dir.join("shown");
                shared(dir, || {
                    let mut told: BTreeMap<Id, u64> = store::load(&f);
                    let t = told.entry(*space).or_default();
                    *t = (*t).max(e.ts.min(now()));
                    let _ = store::save(&f, &told);
                });
                // Once (`rung`), even dated before what was shown (written
                // offline, a clock behind): it is new here. A week old, it
                // waits, unread, for the app (`late`).
                let tell = tell && e.ts + WEEK > now();
                let body = if text.is_empty() { "(file)".into() } else { text };
                tell.then(|| n(channel, at(&channel), body, false, false))
            }
            Op::Call { channel: None } => Some(n(
                crate::state::DM,
                String::new(),
                String::new(),
                false,
                true,
            )),
            // A minute either way: clocks differ, and late it has ended.
            Op::Call { channel: Some(ch) }
                if p.ring && e.ts.abs_diff(now()) < 60_000 =>
            {
                let body = match p.dm {
                    true => "is calling you",
                    false => "is calling the channel",
                };
                Some(n(ch, at(&ch), body.into(), true, false))
            }
            Op::Knock { .. } => return (None, true),
            _ => None,
        };
        (shown, unknown)
    }

    /// Whether our lane wakes us: it is on a relay, and a frame we sent it
    /// through that relay came back (`check_lane`): on its mailbox's
    /// connection, which is still up, where we hold the mailbox; else of
    /// late. Only then may iroh sleep.
    pub fn wakes(&self) -> bool {
        let echo = lock(&self.echo).1;
        match self.mailbox_up() {
            Some(up) => up && echo.is_some(),
            None => {
                let proved = echo.is_some_and(|t| t.elapsed() < PROVED);
                proved && crate::push::lane_at(&self.dir).is_some()
            }
        }
    }

    /// Whether our lane's mailbox, if this process holds it, is on its
    /// relay now: pushir's own connection says, at no cost.
    pub(super) fn mailbox_up(&self) -> Option<bool> {
        let lane = crate::push::lane_at(&self.dir)?;
        pushir::up(&lane.parse::<pushir::Lane>().ok()?.id)
    }

    /// The relay our lane is on, if it has one.
    pub fn lane_relay(&self) -> Option<String> {
        let serve = self.serve.is_some();
        crate::push::relay(&self.dir, &self.net, serve).map(|r| r.to_string())
    }

    /// A frame to our own lane, through its relay as any sender's: heard,
    /// the whole way in works (relay, mailbox, whoever holds it for us,
    /// our secret). Unheard, nothing would wake iroh: it stays up, where
    /// members dial it, and we catch up with what a dead lane dropped.
    pub(super) async fn check_lane(self: &Arc<Self>) {
        let lane = crate::push::lane_at(&self.dir);
        let Some(l) = lane.and_then(|l| l.parse().ok()) else { return };
        let k: Key = rand();
        lock(&self.echo).0 = k;
        crate::push::send(&l, &[&[crate::push::ACK][..], &k].concat()).await;
        let n = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let Some(n) = n.upgrade() else { return };
            // Heard, the nonce is cleared (`hear`); a later check, replaced.
            if lock(&n.echo).0 != k {
                return;
            }
            // Our mailbox pinged now, once: lost (a NAT forgot it), it
            // connects anew and its return proves the lane.
            tokio::task::spawn_blocking(pushir::kick);
            if lock(&n.echo).1.take().is_some() || n.ep().is_none() {
                warn!("our lane does not answer: iroh stays up");
                if n.up().await.is_ok() {
                    n.resume().await;
                }
            }
        });
    }

    /// Wake sleeping devices: each lane gets `raw` as one labelled event.
    pub(super) fn wake(self: &Arc<Self>, id: &Id, raw: Option<Vec<u8>>) {
        self.wake_who(id, None, raw);
    }

    pub(super) fn wake_who(
        self: &Arc<Self>,
        id: &Id,
        who: Option<&Id>,
        raw: Option<Vec<u8>>,
    ) {
        let dev = self.device();
        let env =
            raw.as_deref().and_then(|r| postcard::from_bytes::<Env>(r).ok());
        let ts = env.as_ref().map_or_else(now, |e| e.ts);
        let live = env.as_ref().is_some_and(|e| e.live);
        // Only devices that would show it: a sleeping phone is woken for a
        // message to it (or as loud as its owner asked), never for the rest.
        let Ok(sp) = self.get(id) else { return };
        let (mut urls, labels, at, unjoined) = {
            let b = lock(&sp.book);
            let key = sp.inv.as_ref().map(|i| b.key(i).0);
            // A lane's frame is labelled as events are: by the op's epoch
            // key, or (bare, too big to carry) by the current one.
            let at = env.as_ref().and_then(|e| b.keys.get(&e.epoch)).copied();
            let labels =
                (at.map(|k| ev_label(id, &k)), key.map(|k| ev_label(id, &k)));
            let op = env.zip(key).and_then(|(e, k)| op::open(id, &k, &e, None));
            let op = op.map(|o| o.0.op);
            let all = b.st.pushes_of(who).into_iter();
            let all = all.filter(|(d, url)| {
                let user = b.st.owner_of(d).unwrap_or_default();
                wants(&b.st, op.as_ref(), level_of(url), &user)
            });
            let all = all.collect::<Vec<_>>();
            // A DM they have not joined yet (our knock on its way): no lane
            // of theirs here.
            let peer = b.st.peer(&self.id());
            let theirs = |(d, _): &(EndpointId, String)| {
                peer.is_some_and(|p| b.st.owner_of(d) == Some(p))
            };
            let unjoined = peer.filter(|_| !all.iter().any(theirs));
            (all, labels, at, unjoined)
        };
        // Then their lane from their card wakes them; the op is taken once
        // they are in (it comes again until acked).
        if let Some(p) = unjoined {
            urls.extend(self.card_lanes(&p));
        }
        // Told by its lane: the op on it; too big for a frame, the space
        // alone (catch up). Again for a minute, until acked (asleep too:
        // `woken` acks what it took, and shows it once); past that from the
        // outbox, kept on disk first, until acked (calls are live: no).
        let mut keep = vec![];
        let mut go = vec![];
        for (d, url) in urls.iter().filter(|u| u.0 != dev) {
            let Some(to) = crate::push::lane_of(url) else { continue };
            let raw = raw.as_deref().unwrap_or_default();
            let full =
                labels.0.map(|l| [&[crate::push::OP][..], &l, raw].concat());
            let full = full.filter(|f| f.len() <= pushir::MAX);
            let a =
                at.filter(|_| full.is_some()).map(|k| ev_ack(id, &k, ts, d));
            let bare = labels.1.map(|l| [&[crate::push::OP][..], &l].concat());
            let Some(f) = full.or(bare) else { continue };
            if !live {
                keep.push(Out::new(a, *id, *d, ts, url.clone(), f.clone()));
            }
            go.push((to, f, a, *d));
        }
        self.keep_out(keep);
        for (to, f, a, d) in go {
            // Sent just now as an event (`event`: it is up): not twice.
            let w = (*id, ts, d, live, Instant::now());
            let sent = a.and_then(|a| lock(&self.waits).insert(a, w));
            let n = Arc::downgrade(self);
            tokio::spawn(async move {
                for wait in [0, 3, 10, 45] {
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    let Some(n) = n.upgrade() else { return };
                    let acked = |a| !lock(&n.waits).contains_key(&a);
                    if wait > 0 && a.is_some_and(acked) {
                        return;
                    }
                    drop(n);
                    // Without a lane of ours it still goes: unacked, asleep
                    // or not, it is shown (CLAUDE.md).
                    if wait > 0 || sent.is_none() {
                        crate::push::send(&to, &f).await;
                    }
                }
            });
        }
    }

    /// The lanes `user` told in their own contact card that we hold.
    fn card_lanes(&self, user: &Id) -> Vec<(EndpointId, String)> {
        let theirs = |s: &Arc<Space>| {
            s.inv.as_ref().is_some_and(|i| i.card() && i.owner == *user)
        };
        let cards = self.all().into_iter().filter(theirs);
        cards
            .flat_map(|s| {
                self.view(&s.id, |st| st.pushes_of(None)).unwrap_or_default()
            })
            .collect()
    }

    /// `raw` as one pushir event to each of `to` with a lane; the rest.
    /// Not sent, or unacked in 3 s, dialed after all.
    pub(super) fn event(
        &self,
        id: Id,
        raw: &[u8],
        to: Vec<EndpointId>,
    ) -> Vec<EndpointId> {
        let Some(e) = postcard::from_bytes::<Env>(raw).ok() else { return to };
        let Ok(sp) = self.get(&id) else { return to };
        let k = lock(&sp.book).keys.get(&e.epoch).copied();
        let Some(k) = k else { return to };
        // Present (said `Here`, not `Gone`) or a storage node: a sleeping
        // phone is not woken for typing, presence or reads; it learns them
        // from whoever is up when it next opens.
        let mut at = self.lanes(&id);
        at.retain(|d, _| self.present(&sp, d));
        let ev = [&[crate::push::OP][..], &ev_label(&id, &k), raw].concat();
        let (lanes, rest): (Vec<_>, Vec<_>) =
            to.into_iter().partition(|d| at.contains_key(d));
        for d in lanes {
            // Too big for a frame: dialed at once.
            if ev.len() > pushir::MAX {
                self.link(id, raw, vec![d]);
                continue;
            }
            let a = ev_ack(&id, &k, e.ts, &d);
            let w = (id, e.ts, d, e.live, Instant::now());
            lock(&self.waits).insert(a, w);
            let n = self.this.get().cloned().unwrap_or_default();
            let (to, dir) = (at[&d].clone(), self.dir.clone());
            let (ev, raw) = (ev.clone(), raw.to_vec());
            tokio::spawn(async move {
                let sent = crate::push::event(&dir, &to, &ev).await;
                if let Some(n) = n.upgrade().filter(|_| sent) {
                    n.evs.0.fetch_add(1, Relaxed);
                    drop(n);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                // Unacked: dialed after all. Its wait stands, so an ack
                // that comes late still counts, and a ring's retries go on.
                let Some(n) = n.upgrade() else { return };
                if lock(&n.waits).contains_key(&a) {
                    n.evs.1.fetch_add(sent as usize, Relaxed);
                    n.link(id, &raw, vec![d]);
                }
            });
        }
        rest
    }

    /// On another relay (or first on one): members told where.
    pub async fn tell_lane(self: &Arc<Self>) {
        let lane = crate::push::lane_at(&self.dir);
        if lane.is_some() && *lock(&self.told_lane) != lane {
            *lock(&self.told_lane) = lane;
            let me = self.id();
            let theirs = |s: &Space| {
                s.inv.as_ref().is_some_and(|i| i.card() && i.owner != me)
            };
            for sp in self.all().into_iter().filter(|s| !theirs(s)) {
                let _ = self.tell_push(&sp.id).await;
            }
        }
    }

    /// Ops sent as events, and how many of those were dialed after all.
    pub fn events(&self) -> (usize, usize) {
        (self.evs.0.load(Relaxed), self.evs.1.load(Relaxed))
    }

    /// Each device of space `id` that told its events address; one two
    /// members claim is neither's (dialed, never sent events).
    pub(super) fn lanes(&self, id: &Id) -> HashMap<EndpointId, pushir::Lane> {
        let p = self.view(id, |s| s.pushes_of(None)).unwrap_or_default();
        let mut claims: HashMap<EndpointId, usize> = HashMap::new();
        p.iter().for_each(|x| *claims.entry(x.0).or_default() += 1);
        let at = |(d, u): (EndpointId, String)| {
            (claims[&d] == 1).then_some((d, crate::push::lane_of(&u)?))
        };
        p.into_iter().filter_map(at).collect()
    }

    /// A message on our lane (`push::split`): from a member's device an
    /// event (an op under its space's label, taken as if dialed and acked
    /// to its author's lane; bare, catch up), its ack, or a dial (iroh up:
    /// a link is coming). Empty, frames may have been missed: catch up.
    pub fn hear(self: &Arc<Self>, b: &[u8]) {
        if b.is_empty() {
            // The mailbox is on its relay (again): our lane, proved anew;
            // frames of ours that reached no relay go now.
            let n = self.clone();
            tokio::spawn(async move { n.check_lane().await });
            self.flush_out(false, true);
            return self.catch_up_all();
        }
        match crate::push::split(b) {
            Some((crate::push::OP, l, rest)) => {
                let env = postcard::from_bytes::<Env>(rest).ok();
                // Under a key we lack (a rekey missed): catch up.
                let Some((sp, k)) = self.labelled(&l, env.as_ref()) else {
                    return self.catch_up_all();
                };
                if rest.is_empty() {
                    return self.catch_up(&sp);
                }
                let by = env.and_then(|e| lock(&sp.book).check(&sp.id, &e));
                let Some(d) = by.map(|s| s.device) else { return };
                // Taken, then acked if its device told us a lane: never
                // dropped for want of one (asleep, `woken` takes it too).
                let Some(e) = self.take(&sp, d, rest) else { return };
                let Some(at) = self.lanes(&sp.id).remove(&d) else { return };
                let a = ev_ack(&sp.id, &k, e.ts, &self.device());
                let ack = [&[crate::push::ACK][..], &a].concat();
                tokio::spawn(async move { crate::push::send(&at, &ack).await });
            }
            Some((crate::push::TEXT, l, rest)) => {
                let Some((sp, k)) = self.labelled(&l, None) else { return };
                let Some(((from, _, back, _, text), mac)) =
                    open_text(&k, rest).filter(|_| sp.id == self.own)
                else {
                    return;
                };
                if !self.devices().iter().any(|d| d.0 == from) {
                    return;
                }
                if let Some(at) = crate::push::lane_of(&back) {
                    let a = text_ack(&k, &mac);
                    let ack = [&[crate::push::ACK][..], &a].concat();
                    tokio::spawn(
                        async move { crate::push::send(&at, &ack).await },
                    );
                }
                if !taken_text(&self.dir, &from, &text, Some(&mac)) {
                    self.show_text(from, text);
                }
            }
            Some((crate::push::ACK, a, _)) => {
                // Delivered: off the outbox, whatever it was.
                self.out_done(|o| o.ack == a);
                // A text of ours, taken by the device it went to.
                if let Some(tx) = lock(&self.told).remove(&a) {
                    let _ = tx.send(());
                    return;
                }
                // Our own, back through the relay: our lane works.
                let mut echo = lock(&self.echo);
                if same(&echo.0, &a) && a != Key::default() {
                    *echo = (Key::default(), Some(Instant::now()));
                    return;
                }
                drop(echo);
                let w = lock(&self.waits).remove(&a);
                if let Some((id, ts, d, false, _)) = w {
                    self.acked(id, d, ts);
                }
            }
            Some((crate::push::DIAL, l, _))
                if self.labelled(&l, None).is_some() =>
            {
                let n = self.clone();
                tokio::spawn(async move { drop(n.up().await) });
            }
            _ => {}
        }
    }

    /// Ops of ours not yet acked by a device they went to: its retries
    /// (`wake_who`) still run.
    pub fn delivering(&self) -> bool {
        lock(&self.waits).values().any(|w| w.4.elapsed() < WAITED)
    }

    /// Every space caught up with, once a `BEAT` at most.
    fn catch_up_all(self: &Arc<Self>) {
        let mut at = lock(&self.caught);
        if at.is_none_or(|t| t.elapsed() >= BEAT) {
            *at = Some(Instant::now());
            self.all().iter().for_each(|sp| self.catch_up(sp));
        }
    }

    /// The space whose key makes label `l`: the op's epoch key, or (with
    /// no op) the current one.
    fn labelled(&self, l: &Key, e: Option<&Env>) -> Option<(Arc<Space>, Key)> {
        self.all().into_iter().find_map(|sp| {
            let k = {
                let b = lock(&sp.book);
                match e {
                    Some(e) => *b.keys.get(&e.epoch)?,
                    None => b.key(sp.inv.as_ref()?).0,
                }
            };
            (ev_label(&sp.id, &k) == *l).then_some((sp, k))
        })
    }

    /// Caught up with the space's storage nodes, or with none, a few of
    /// its devices: news too big to carry, or frames a gap may have lost.
    fn catch_up(self: &Arc<Self>, sp: &Arc<Space>) {
        let stores = self.stores(&sp.id);
        if stores.is_empty() {
            let peers = self.bootstrap(sp);
            return peers.into_iter().take(3).for_each(|p| self.sync(sp, p));
        }
        stores.into_iter().for_each(|s| self.sync(sp, s.0));
    }

    /// `d` has our ops in `space` up to `ts`: a storage node keeps them,
    /// or a member's device has them.
    pub(super) fn acked(&self, space: Id, d: EndpointId, ts: u64) {
        let Ok(sp) = self.get(&space) else { return };
        let host = lock(&sp.book).st.hosts().iter().any(|a| a.id == d)
            || self.stores(&space).iter().any(|s| s.0.id == d);
        // A device acks the op it took, not those before (one may be lost:
        // its retries go on); a storage node keeps all it was sent.
        let done = |t: u64| if host { t <= ts } else { t == ts };
        lock(&self.waits).retain(|_, w| (w.0, w.2) != (space, d) || !done(w.1));
        if !host {
            let d = *d.as_bytes();
            self.out_done(|o| (o.space, o.device, o.ts) == (space, d, ts));
        }
        match host {
            true => {
                let mut k = lock(&self.kept);
                let at = k.entry(space).or_default();
                *at = (*at).max(ts);
            }
            false => self.reach_device(&space, &d, ts),
        }
        lock(&sp.undelivered).clear();
        self.emit(Event::Changed { space });
    }

    /// Up to when a storage node of `space` has our ops.
    pub fn kept(&self, space: &Id) -> u64 {
        lock(&self.kept).get(space).copied().unwrap_or(0)
    }
}
