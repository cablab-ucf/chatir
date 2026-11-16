//! A space's book: its ops kept, folded, keyed.

use super::*;

pub(super) fn rekey_aad(space: &Id, epoch: &[u8; 8]) -> Vec<u8> {
    [&space.0[..], epoch].concat()
}

impl Book {
    pub(super) fn new(log: Log, st: State) -> Self {
        let (idx, at, keys) = (BTreeMap::new(), HashMap::new(), HashMap::new());
        let (pending, backs, knocks) = (HashMap::new(), HashMap::new(), vec![]);
        Self {
            log,
            idx,
            at,
            keys,
            pending,
            parked: 0,
            backs,
            offered: vec![],
            st,
            knocks,
            rekeys: HashMap::new(),
            rejected: VecDeque::new(),
            rejects: 0,
        }
    }

    pub(super) fn has(&self, i: &Id, ts: u64) -> bool {
        let day = (ts / DAY) as u32;
        self.idx.get(&day).is_some_and(|d| d.contains(i))
            || self.pending.contains_key(i)
    }

    /// Counted as had in sync, so neither asked for again nor served;
    /// remembered if it came from the network.
    pub(super) fn reject(&mut self, i: Id, day: u32, net: bool) {
        if !net {
            self.at.remove(&i);
            return;
        }
        if self.rejected.len() >= REJECTED
            && let Some((o, d)) = self.rejected.pop_front()
            && !self.at.contains_key(&o)
        {
            self.idx.get_mut(&d).map(|s| s.remove(&o));
        }
        self.idx.entry(day).or_default().insert(i);
        self.rejected.push_back((i, day));
        self.rejects += 1;
    }

    pub(super) fn index(&mut self, i: Id, ts: u64, at: u64, len: usize) {
        self.at.insert(i, (at, len as u32));
        self.idx.entry((ts / DAY) as u32).or_default().insert(i);
    }

    /// Append to the log (flushed, for ours) and the day index sync compares.
    pub(super) fn store(
        &mut self,
        i: Id,
        ts: u64,
        raw: &[u8],
        checked: u8,
        flush: bool,
    ) -> bool {
        if self.at.contains_key(&i) {
            return false;
        }
        let at = self.log.append(checked, raw, flush);
        let Ok(at) = at.inspect_err(|e| warn!("log: {e}")) else {
            return false;
        };
        self.index(i, ts, at, raw.len());
        true
    }

    /// Open what checks out; learned keys open more. Returns how many new.
    pub(super) fn fold(
        &mut self,
        sp: &Id,
        me: &Me,
        mut work: Vec<(Id, Env, Src)>,
    ) -> usize {
        let (mut ops, mut kept) = (vec![], 0);
        // Refused by the state as it stands, which this batch may yet move
        // (an unban and the unbanned's post, synced together): checked
        // again once it has, refused for good once nothing more moves.
        let (mut later, mut roles) = (vec![], false);
        loop {
            while let Some((i, e, src)) = work.pop() {
                let Some(&key) = self.keys.get(&e.epoch) else {
                    self.park(i, e, src);
                    continue;
                };
                let known = match src {
                    Src::Log(0) | Src::Net(_, false) => None,
                    Src::Log(v) => Some(v),
                    Src::Net(_, true) => Some(2),
                };
                let net = matches!(src, Src::Net(..));
                let day = (e.ts / DAY) as u32;
                let Some((s, v)) = op::open(sp, &key, &e, known) else {
                    self.reject(i, day, net);
                    continue;
                };
                // A contact card carries its owner's ops alone; a holder's
                // knock reaches the owner, kept nowhere: holders never meet.
                let (st, my) = (&self.st, me.id());
                let alien = st.dm == Some(st.owner)
                    && s.author != st.owner
                    && s.author != my;
                let knock = matches!(s.op, Op::Knock { .. });
                // Banned, cut off or retired: dropped, or it fills every
                // member's disk. A key no rekey here set yet passes: its
                // rekey may come later (shown or not, `State` decides).
                let at = (s.ts, s.id);
                if alien && (my != st.owner || !knock) {
                    self.reject(i, day, net);
                    continue;
                }
                if st.role(&s.author) == Role::Banned
                    || st.known(&e.epoch) && st.stale(&e.epoch, &at, &s.author)
                    || st.burned_after(&at, &s.author)
                {
                    later.push((i, e, src));
                    continue;
                }
                if let Src::Net(raw, ours) = &src
                    && !alien
                {
                    if !self.store(i, e.ts, raw, v, *ours) {
                        continue;
                    }
                    kept += 1;
                }
                let key = match &s.op {
                    Op::Rekey { epoch, boxes } => boxes
                        .iter()
                        .find(|b| b.0 == me.id())
                        .and_then(|b| {
                            kem_open(&me.kem, &rekey_aad(sp, epoch), &b.1)
                        })
                        .and_then(|k| Key::try_from(k).ok())
                        .filter(|k| tag(k) == *epoch)
                        .map(|k| (k, false)),
                    Op::Back { key } => Some((*key, true)),
                    Op::Knock { sealed } => {
                        let inv = kem_open(&me.kem, &sp.0, sealed);
                        let inv = inv.and_then(|b| {
                            postcard::from_bytes::<Invite>(&b).ok()
                        });
                        self.knocks.extend(inv.filter(|i| {
                            i.dm == Some(me.id()) && i.owner == s.author
                        }));
                        None
                    }
                    _ => None,
                };
                if alien {
                    // Kept nowhere, but news: its knock.
                    self.reject(i, day, net);
                    kept += 1;
                    continue;
                }
                if let Some((k, back)) = key {
                    // 16 an author, 4096 in all: non-admins' go first.
                    let (o, st) = (&mut self.offered, &self.st);
                    let theirs = |x: &Offered| x.0 == s.author;
                    let gone = match o.iter().filter(|x| theirs(x)).count() {
                        16.. => o.iter().position(theirs),
                        _ if o.len() >= 4096 => o
                            .iter()
                            .position(|x| st.role(&x.0) != Role::Admin)
                            .or(Some(0)),
                        _ => None,
                    };
                    gone.map(|i| o.remove(i));
                    o.push((s.author, at, e.epoch, k, back));
                }
                roles |= net && matches!(s.op, Op::Role { .. });
                if matches!(s.op, Op::Rekey { .. }) {
                    self.rekeys.insert(s.id, i);
                }
                ops.push(s);
            }
            if !ops.is_empty() {
                work.append(&mut later);
            }
            self.st.extend(std::mem::take(&mut ops));
            let st = &self.st;
            let good =
                |(a, at, ep, ..): &(Id, (u64, Id), [u8; 8], Key, bool)| {
                    st.role(a) == Role::Admin
                        && !(st.known(ep) && st.stale(ep, at, a))
                };
            let good: Vec<_> =
                self.offered.extract_if(.., |o| good(o)).collect();
            for (_, _, under, k, back) in good {
                if back {
                    self.backs.entry(under).or_default().push(tag(&k));
                }
                if self.keys.insert(tag(&k), k).is_none() {
                    work.extend(self.unpark(&tag(&k)));
                }
            }
            if work.is_empty() {
                for (i, e, src) in later {
                    let net = matches!(src, Src::Net(..));
                    self.reject(i, (e.ts / DAY) as u32, net);
                }
                // A role moved: what a ban refused before, synced again.
                if roles {
                    self.unreject();
                }
                return kept;
            }
        }
    }

    /// Refused ops forgotten, so sync asks for them again: the state that
    /// refused them may have moved since (an unban).
    fn unreject(&mut self) {
        self.rejects += 1; // the list on disk, saved anew
        for (o, d) in std::mem::take(&mut self.rejected) {
            if !self.at.contains_key(&o) {
                self.idx.get_mut(&d).map(|s| s.remove(&o));
            }
        }
    }

    fn park(&mut self, i: Id, e: Env, src: Src) {
        if let Src::Net(raw, _) = &src {
            if self.parked > PARKED {
                self.pending.retain(|_, (_, s)| matches!(s, Src::Log(_)));
                self.parked = 0;
            }
            // A map entry too, not just its size.
            self.parked += raw.len() + 256;
        }
        self.pending.insert(i, (e, src));
    }

    fn unpark(&mut self, t: &[u8; 8]) -> Vec<(Id, Env, Src)> {
        let out = self.pending.extract_if(|_, (e, _)| e.epoch == *t);
        let v: Vec<_> = out.map(|(i, (e, s))| (i, e, s)).collect();
        for (_, _, s) in &v {
            if let Src::Net(raw, _) = s {
                self.parked -= raw.len() + 256;
            }
        }
        v
    }

    /// A key from an invite: one an admin set, or any while rekeys have
    /// left us out (its own `Rekey` may be sealed under a key we lack).
    pub(super) fn learn(&mut self, sp: &Id, me: &Me, k: Key) -> bool {
        let set = self.st.known(&tag(&k));
        let out =
            self.st.epoch().is_some_and(|e| !self.keys.contains_key(&e.tag));
        let new = (set || out) && self.keys.insert(tag(&k), k).is_none();
        if new {
            let w = self.unpark(&tag(&k));
            self.fold(sp, me, w);
        }
        new
    }

    /// Seal key, and if a newer epoch left us out; never older than our invite.
    pub(super) fn key(&self, inv: &Invite) -> (Key, bool) {
        let st = &self.st;
        if !st.known(&tag(&inv.key)) {
            return (inv.key, false);
        }
        let newest = st.epoch().map(|e| e.tag);
        let held =
            st.live().find_map(|e| Some((e.tag, *self.keys.get(&e.tag)?)));
        held.map_or((inv.key, true), |(t, k)| (k, Some(t) != newest))
    }

    /// Opened under a key held, by someone neither out nor cut off by a
    /// newer epoch (a key no rekey set counts as older than all): whom we
    /// meet, sync with and send to.
    pub(super) fn check(&self, sp: &Id, e: &Env) -> Option<Signed> {
        let s = op::open(sp, self.keys.get(&e.epoch)?, e, None)?.0;
        // As of now, not of the time its author gave it: a key since
        // replaced without them opens nothing, however the op is dated.
        let end = crate::state::END;
        let out =
            self.st.out(&s.author) || self.st.stale(&s.epoch, &end, &s.author);
        (!out).then_some(s)
    }

    /// The rekeys from epoch `t` on, oldest first, to 16 MB: whoever holds
    /// `t` opens them and sees for itself what left it out (or, a few at a
    /// time, the keys it missed).
    pub(super) fn proof(&self, t: &[u8; 8]) -> Vec<Id> {
        let e = self.st.epochs();
        let from = e.iter().position(|e| e.tag == *t).unwrap_or(0);
        let mut size = 0;
        let ids = e[from..].iter().filter_map(|e| self.rekeys.get(&e.at.1));
        let fit = |i: &&Id| {
            size += self.at.get(i).map_or(0, |a| a.1 as usize);
            size <= 16 << 20
        };
        ids.take_while(fit).copied().collect()
    }

    /// Valid epochs' keys `Back` misses: lost rekey races, demoted admins'.
    pub(super) fn orphans(&self) -> Vec<Key> {
        let Some(cur) = self.st.epoch() else { return vec![] };
        let (mut seen, mut todo) = (HashSet::from([cur.tag]), vec![cur.tag]);
        while let Some(t) = todo.pop() {
            let older = self.backs.get(&t).into_iter().flatten().copied();
            todo.extend(older.filter(|b| seen.insert(*b)));
        }
        let lost = self.st.epochs().iter().filter(|e| !seen.contains(&e.tag));
        lost.filter_map(|e| self.keys.get(&e.tag).copied()).collect()
    }
}

impl Space {
    pub fn buckets(&self) -> Vec<Bucket> {
        let b = lock(&self.book);
        let fp = |ids: &BTreeSet<Id>| {
            let mut h = blake3::Hasher::new();
            for i in ids {
                h.update(&i.0);
            }
            h.finalize().as_bytes()[..16].try_into().expect("16")
        };
        b.idx.iter().map(|(d, ids)| (*d, ids.len() as u32, fp(ids))).collect()
    }

    pub fn ids(&self, day: u32) -> Vec<Id> {
        let b = lock(&self.book);
        b.idx.get(&day).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    pub fn get(&self, ids: &[Id]) -> Vec<Vec<u8>> {
        let mut b = lock(&self.book);
        let at: Vec<_> =
            ids.iter().filter_map(|i| b.at.get(i).copied()).collect();
        at.into_iter().filter_map(|(o, l)| b.log.read(o, l).ok()).collect()
    }

    /// Devices of members not banned: whom a storage node may take from.
    pub fn devices(&self) -> Vec<EndpointId> {
        lock(&self.book).st.devices(usize::MAX)
    }

    pub(super) fn tx(&self) -> Option<GossipSender> {
        lock(&self.tx).clone()
    }

    pub(super) fn nbrs(&self) -> Vec<EndpointId> {
        lock(&self.nbrs).iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::tests::post;

    pub(super) fn book(name: &str, owner: Id) -> (Book, PathBuf) {
        let p = std::env::temp_dir().join(format!("chatir-{name}-{}", now()));
        let st = State::new(&Invite::new(owner, "", None));
        (Book::new(Log::open(&p, |_, _, _| {}).unwrap(), st), p)
    }

    #[test]
    fn what_no_key_opens_stays_off_disk_and_bounded() {
        let (mut b, p) = book("park", Id([1; 32]));
        let me = Me::new(rand());
        for n in 0..40 {
            let e =
                Env { epoch: [n; 8], ts: 0, live: false, ct: vec![n; 1 << 20] };
            let raw = postcard::to_allocvec(&e).unwrap();
            b.fold(
                &Id([2; 32]),
                &me,
                vec![(Id([n; 32]), e, Src::Net(raw, false))],
            );
        }
        assert!(b.parked <= PARKED + (2 << 20) && b.at.is_empty());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn refused_ops_count_as_had_and_are_never_served() {
        let (mut b, p) = book("refused", Id([1; 32]));
        let k: Key = rand();
        b.keys.insert(tag(&k), k);
        let e = Env { epoch: tag(&k), ts: DAY, live: false, ct: vec![7; 64] };
        let raw = postcard::to_allocvec(&e).unwrap();
        let i = Id(blake3::hash(&raw).into());
        let me = Me::new(rand());
        b.fold(&Id([2; 32]), &me, vec![(i, e, Src::Net(raw, false))]);
        assert!(b.has(&i, DAY) && b.at.is_empty() && b.rejected.len() == 1);
        std::fs::remove_file(p).unwrap();
    }

    /// A post refused while its author was banned stands once they are
    /// unbanned: taken in the batch that unbans them, or synced again.
    #[test]
    fn an_unban_lets_the_unbanneds_posts_in() {
        let (o, x) = (Me::new(rand()), Me::new(rand()));
        let inv = Invite::new(o.id(), "", None);
        let (sp, k, dev) = (inv.id(), inv.key, SecretKey::generate().public());
        let p = std::env::temp_dir().join(format!("chatir-unban-{}", now()));
        let mut b =
            Book::new(Log::open(&p, |_, _, _| {}).unwrap(), State::new(&inv));
        b.keys.insert(tag(&k), k);
        let ch = Ch([3; 8]);
        let op = |who: &Me, ts, op| {
            let (s, raw) = op::seal(&sp, &k, &who.user, dev, ts, op);
            (s.id, postcard::from_bytes(&raw).unwrap(), Src::Net(raw, false))
        };
        let role = |ts, role| op(&o, ts, Op::Role { user: x.id(), role });
        let post = |ts, t: &str| {
            let files = vec![];
            op(
                &x,
                ts,
                Op::Post { channel: ch, text: t.into(), reply: None, files },
            )
        };
        let shown = |b: &Book| b.st.messages(&ch, None, 9).len();
        b.fold(&sp, &o, vec![role(DAY, Role::Banned)]);
        // The post comes first off the batch, the unban after it.
        b.fold(&sp, &o, vec![role(DAY + 1, Role::Member), post(DAY + 2, "a")]);
        assert_eq!(shown(&b), 1, "taken with its unban");
        b.fold(&sp, &o, vec![role(DAY + 3, Role::Banned)]);
        let (i, ..) = post(DAY + 4, "b");
        b.fold(&sp, &o, vec![post(DAY + 4, "b")]);
        assert!(b.has(&i, DAY + 4) && b.rejected.len() == 1, "refused, had");
        b.fold(&sp, &o, vec![role(DAY + 5, Role::Member)]);
        assert!(!b.has(&i, DAY + 4), "asked for again");
        b.fold(&sp, &o, vec![post(DAY + 4, "b")]);
        assert_eq!(shown(&b), 2);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn only_admins_hand_on_keys() {
        let (o, x) = (Me::new(rand()), Me::new(rand()));
        let inv = Invite::new(o.id(), "", None);
        let (sp, k0, dev) = (inv.id(), inv.key, SecretKey::generate().public());
        let p = std::env::temp_dir().join(format!("chatir-keys-{}", now()));
        let mut b =
            Book::new(Log::open(&p, |_, _, _| {}).unwrap(), State::new(&inv));
        b.keys.insert(tag(&k0), k0);
        let (kb, kx): (Key, Key) = (rand(), rand());
        let (ch, n) = (Ch([3; 8]), |t: &str| t.to_string());
        let post = |t| Op::Post {
            channel: ch,
            text: n(t),
            reply: None,
            files: vec![],
        };
        let boxed = kem_seal(&o.pk(), &rekey_aad(&sp, &tag(&kx)), &kx).unwrap();
        let ops = [
            (&o, k0, 1, Op::Channel { id: ch, name: Some(n("g")) }),
            // Someone holding the key, not an admin, mints one of their own.
            (&x, k0, 2, Op::Back { key: kx }),
            (
                &x,
                k0,
                3,
                Op::Rekey { epoch: tag(&kx), boxes: vec![(o.id(), boxed)] },
            ),
            (&x, kx, 4, post("x, under its own key")),
            (&o, k0, 5, Op::Back { key: kb }),
            (&o, kb, 6, post("o, under a key handed on")),
        ];
        let work = ops.into_iter().map(|(who, k, ts, op)| {
            let (s, raw) = op::seal(&sp, &k, &who.user, dev, ts, op);
            (s.id, postcard::from_bytes(&raw).unwrap(), Src::Log(0))
        });
        b.fold(&sp, &o, work.rev().collect());
        assert!(
            b.keys.contains_key(&tag(&kb)) && !b.keys.contains_key(&tag(&kx))
        );
        // Nor from an invite pasted in: only an epoch an admin set.
        assert!(!b.learn(&sp, &o, kx) && !b.keys.contains_key(&tag(&kx)));
        let texts: Vec<_> =
            b.st.messages(&ch, None, 9).into_iter().map(|m| m.text).collect();
        assert_eq!(texts, ["o, under a key handed on"]);
        std::fs::remove_file(p).unwrap();
    }

    /// A key an admin handed out by itself (no rekey set it) is older than
    /// every epoch: still learned, so `Back` chains resolve, but once the
    /// admin is banned and rekeyed away, its sock puppets' writing under
    /// it shows nowhere, and admits them to no sync.
    #[test]
    fn a_key_an_admin_passed_outlives_no_ban() {
        let (o, x, p) = (Me::new(rand()), Me::new(rand()), Me::new(rand()));
        let inv = Invite::new(o.id(), "", None);
        let (sp, k0, dev) =
            (inv.id(), inv.key, SecretKey::from_bytes(&rand()).public());
        let path =
            std::env::temp_dir().join(format!("chatir-planted-{}", now()));
        let mut b = Book::new(
            Log::open(&path, |_, _, _| {}).unwrap(),
            State::new(&inv),
        );
        b.keys.insert(tag(&k0), k0);
        let (kb, k2, ch): (Key, Key, _) = (rand(), rand(), Ch([3; 8]));
        let boxed = kem_seal(&o.pk(), &rekey_aad(&sp, &tag(&k2)), &k2).unwrap();
        let mut fold = |ops: Vec<(&Me, Key, u64, Op)>| {
            let work = ops.into_iter().map(|(who, k, ts, op)| {
                let (s, raw) = op::seal(&sp, &k, &who.user, dev, ts, op);
                (s.id, postcard::from_bytes(&raw).unwrap(), Src::Log(0))
            });
            b.fold(&sp, &o, work.collect());
        };
        let g = Some("g".to_string());
        fold(vec![
            (&o, k0, 1, Op::Rekey { epoch: tag(&k0), boxes: vec![] }),
            (&o, k0, 2, Op::Channel { id: ch, name: g }),
            (&o, k0, 3, Op::Role { user: x.id(), role: Role::Admin }),
            (&x, k0, 4, Op::Back { key: kb }),
        ]);
        fold(vec![
            (&o, k0, 5, Op::Role { user: x.id(), role: Role::Banned }),
            (
                &o,
                k0,
                6,
                Op::Rekey { epoch: tag(&k2), boxes: vec![(o.id(), boxed)] },
            ),
            (&p, kb, 7, post(ch, "p, x's sock puppet")),
            (&o, k2, 8, post(ch, "o")),
        ]);
        assert!(b.keys.contains_key(&tag(&kb)), "learned while x was an admin");
        let texts: Vec<_> =
            b.st.messages(&ch, None, 9).into_iter().map(|m| m.text).collect();
        assert_eq!(texts, ["o"]);
        let end = &crate::state::END;
        assert!(
            b.st.stale(&tag(&kb), end, &p.id())
                && !b.st.stale(&tag(&k2), end, &o.id())
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn keys_a_rekey_race_cut_off_are_handed_on() {
        let o = Id([1; 32]);
        let (mut b, p) = book("race", o);
        let k: [Key; 3] = [rand(), rand(), rand()];
        let device = SecretKey::from_bytes(&[1; 32]).public();
        b.st.extend((0..3).map(|n| Signed {
            id: Id([n as u8; 32]),
            author: o,
            device,
            ts: n as u64,
            op: Op::Rekey { epoch: tag(&k[n]), boxes: vec![(o, vec![])] },
            epoch: tag(&k[0]),
        }));
        b.keys.extend(k.map(|k| (tag(&k), k)));
        // Both rekeys replaced the first key; the second lost the race.
        b.backs.insert(tag(&k[1]), vec![tag(&k[0])]);
        b.backs.insert(tag(&k[2]), vec![tag(&k[0])]);
        assert_eq!(b.orphans(), [k[1]]);
        b.backs.get_mut(&tag(&k[2])).unwrap().push(tag(&k[1]));
        assert!(b.orphans().is_empty());
        std::fs::remove_file(p).unwrap();
    }
}
