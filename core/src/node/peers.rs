//! Ops among peers: gossip, direct links, sync, who is admitted.

use super::*;

/// One gossip subscription per space, renewed if it ever closes.
/// The topic is the room: a rekey moves it, leaving whoever it cut off.
pub(super) async fn pump(n: Arc<Node>, sp: Weak<Space>, boot: Vec<EndpointId>) {
    loop {
        // Up to `FANOUT` devices, a space is told directly (`tell`): no
        // mesh held open, each link pinged every 5 s. Bigger, gossip.
        let small =
            sp.upgrade().map(|s| lock(&s.book).st.devices(FANOUT + 1).len());
        if small.is_none_or(|n| n <= FANOUT) {
            return;
        }
        let Some(s) = sp.upgrade() else { return };
        let (room, moved) = (n.room(&s), s.moved.clone());
        *lock(&s.room) = room;
        // A storage node not yet told where: wait to be.
        if room == [0; 32] {
            drop(s);
            moved.notified().await;
            continue;
        }
        let peers = boot.iter().copied().chain(n.bootstrap(&s)).collect();
        let g = match n.up().await {
            Ok(_) => n.gossip(),
            Err(_) => None,
        };
        let Some(g) = g else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let topic = g.subscribe(TopicId::from_bytes(room), peers).await;
        let Ok(topic) = topic else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let (tx, mut rx) = topic.split();
        *lock(&s.tx) = Some(tx.clone());
        drop(s);
        // Alone (a dial failed, say): dial again soon, backing off to a minute.
        let mut wait = Duration::from_secs(2);
        loop {
            let next = tokio::select! {
                // A rekey goes out on the old topic, to those it cut off:
                // stay a while (idle, it costs nothing), or our own move
                // outruns it on a busy machine.
                () = moved.notified() => {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    break;
                }
                r = tokio::time::timeout(wait, rx.next()) => r,
            };
            let ev = match next {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(_) => {
                    let Some(s) = sp.upgrade() else { return };
                    if s.nbrs().is_empty() {
                        let _ = tx.join_peers(n.bootstrap(&s)).await;
                    }
                    // Alone: a few redials, then quiet until dialed.
                    wait = if wait < ALONE { wait * 2 } else { QUIET };
                    continue;
                }
            };
            let Some(s) = sp.upgrade() else { return };
            match ev {
                Ok(Gossiped::Received(m)) => {
                    n.neighbor(&s, m.delivered_from);
                    n.signaled(&s, &m.content, m.delivered_from);
                }
                Ok(Gossiped::NeighborUp(p)) => n.neighbor(&s, p),
                Ok(Gossiped::NeighborDown(p)) => {
                    let mut nb = lock(&s.nbrs);
                    nb.remove(&p);
                    if nb.is_empty() {
                        wait = Duration::from_secs(2);
                    }
                    drop(nb);
                    n.emit(Event::Changed { space: s.id });
                }
                _ => break,
            }
        }
        if let Some(s) = sp.upgrade() {
            lock(&s.nbrs).clear();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

impl Node {
    pub(super) fn sync(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        peer: impl Into<EndpointAddr>,
    ) {
        tokio::spawn(self.clone().sync_with(sp.clone(), peer.into()));
    }

    /// Dial `raw` straight to each device of a small space (its members'
    /// and storage nodes): one stream each, over links kept while in use.
    pub(super) fn tell(&self, sp: &Space, raw: &[u8]) {
        if raw.len() > GOSSIP_MAX {
            return; // a sync carries it
        }
        let me = self.device();
        // Whoever wrote here, its storage nodes, devices met here (a
        // newcomer that has only said `Here`) and our own other devices.
        let to: Vec<EndpointId> = {
            let ours = self.devices().into_iter().map(|d| d.0);
            let met = sp.nbrs();
            let b = lock(&sp.book);
            let hosts = b.st.hosts().into_iter().map(|a| a.id);
            let wrote = b.st.devices(FANOUT + 1);
            let mut v: Vec<_> =
                wrote.into_iter().chain(hosts).chain(met).chain(ours).collect();
            let mut seen = HashSet::from([me]);
            v.retain(|d| seen.insert(*d));
            v
        };
        // Big, gossip's; a small space has no gossip (`pump`).
        if to.len() > FANOUT && sp.tx().is_some() {
            return;
        }
        self.findable(&to);
        self.dial(sp.id, raw, to);
    }

    /// `raw`, for space `id`, to each of `to`: one stream each, over links
    /// kept while in use.
    pub(super) fn dial(&self, id: Id, raw: &[u8], to: Vec<EndpointId>) {
        let to = self.event(id, raw, to);
        self.link(id, raw, to);
    }

    /// Over a direct link, kept while in use.
    pub(super) fn link(&self, id: Id, raw: &[u8], to: Vec<EndpointId>) {
        let frame: Arc<[u8]> = [&id.0[..], raw].concat().into();
        for d in to {
            let (links, f) = (self.links.clone(), frame.clone());
            let this = self.this.get().cloned().unwrap_or_default();
            tokio::spawn(async move {
                let held = lock(&links).get(&d).map(|l| l.0.clone());
                let c = match held.filter(|c| c.close_reason().is_none()) {
                    Some(c) => c,
                    None => {
                        let Some(n) = this.upgrade() else { return };
                        let five = Duration::from_secs(5);
                        let go =
                            tokio::time::timeout(five, n.connect(d, DIRECT));
                        let Ok(Ok(c)) = go.await else { return };
                        drop(n);
                        // Its acks, while the link lasts (`told`); closed
                        // once quiet.
                        tokio::spawn(acks(c.clone(), this));
                        tokio::spawn(quiet(links.clone(), d, c.clone()));
                        c
                    }
                };
                lock(&links).insert(d, (c.clone(), Instant::now()));
                if let Ok(mut s) = c.open_uni().await {
                    let _ = s.write_all(&f).await;
                    let _ = s.finish();
                }
            });
        }
    }

    /// Whether anything can leave this device now: a relay holds us, or a
    /// device of some space is reached.
    pub fn connected(&self) -> bool {
        use iroh::Watcher as _;
        let relay = self.ep().map(|e| e.home_relay_status().get());
        relay.iter().flatten().any(|s| s.is_connected())
            || self.wakes()
            || self.all().iter().any(|s| !s.nbrs().is_empty())
    }

    /// Ops a space's device dialed straight to us: taken as if gossiped,
    /// from devices that space knows only.
    pub(super) async fn told(self: &Arc<Self>, c: &Connection) -> Result<()> {
        let from = c.remote_id();
        // Quiet a while (its dialer closes a quiet link at `QUIET_LINK`):
        // closed, so nobody holds it open saying nothing.
        let next = || tokio::time::timeout(3 * QUIET_LINK, c.accept_uni());
        while let Ok(Ok(mut s)) = next().await {
            // One stream too big, cut short or slow: the next ones count.
            let read = sync::rest(&mut s, 32 + GOSSIP_MAX);
            let Ok(Ok(b)) = tokio::time::timeout(QUIET_LINK, read).await else {
                continue;
            };
            let Some((id, raw)) = b.split_first_chunk::<32>() else { continue };
            let Ok(sp) = self.get(&Id(*id)) else { continue };
            // A kept op, acked (space, its time): its sender shows it here.
            let kept = self.take(&sp, from, raw).filter(|e| !e.live);
            if let (Some(e), Ok(mut a)) = (kept, c.open_uni().await) {
                let _ =
                    a.write_all(&[&id[..], &e.ts.to_le_bytes()].concat()).await;
                let _ = a.finish();
            }
        }
        Ok(())
    }

    /// An op `from` a device of the space, as if gossiped; its envelope,
    /// if taken. A member opens it first: signed on `from`, by someone
    /// it may hear. Who knows only the space's id is neither met nor
    /// synced with, so never told where its members meet. A storage node
    /// can't open it: vouched devices only.
    pub(super) fn take(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        from: EndpointId,
        raw: &[u8],
    ) -> Option<Env> {
        let e = postcard::from_bytes::<Env>(raw).ok()?;
        let ok = match sp.inv {
            Some(_) => lock(&sp.book)
                .check(&sp.id, &e)
                .is_some_and(|s| s.device == from),
            None => self.takes(sp, &from),
        };
        if !ok {
            return None;
        }
        // Reached, directly: a neighbor (caught up with once).
        self.neighbor(sp, from);
        self.signaled(sp, raw, from);
        // Acked only once kept: refused (dated ahead, a full disk), its
        // sender keeps trying. A storage node fetches it first (a sync,
        // which tells its sender it is kept): no ack before.
        let i = Id(blake3::hash(raw).into());
        let kept = e.live || lock(&sp.book).has(&i, e.ts);
        kept.then_some(e)
    }

    /// What we tell `to` when we ask: our room, and who we are, signed.
    pub(crate) fn hello(
        &self,
        sp: &Space,
        to: &EndpointId,
        root: [u8; 32],
        token: Option<[u8; 16]>,
    ) -> Hello {
        let room = self.room(sp);
        let me = &self.me.user;
        let who = sp.inv.as_ref().map(|_| sync::sign(me, &sp.id, &room, to));
        let (devices, tags) = match token {
            Some(_) => {
                let t =
                    lock(&sp.book).keys.keys().copied().take(4096).collect();
                (sp.devices(), t)
            }
            None => (vec![], vec![]),
        };
        Hello { space: sp.id, root, room, who, token, devices, tags }
    }

    /// Whom to answer: a member, who holds a key no later epoch cut them
    /// off from; a storage node, only devices its owners vouched for. An
    /// owner (its token) has it keep a new space.
    pub(crate) fn find(
        self: &Arc<Self>,
        h: &Hello,
        from: EndpointId,
    ) -> (Option<Arc<Space>>, Admit) {
        let who = sync::who(h, &self.device());
        let sp = self.get(&h.space).ok();
        let owner = h.token.zip(self.serve).is_some_and(|(a, b)| same(&a, &b));
        let (sp, a) = if owner {
            self.vouch(h, from, who);
            let sp =
                sp.or_else(|| self.open(None, h.space, vec![from.into()]).ok());
            (sp, Admit::Yes)
        } else {
            let Some(sp) = sp else { return (None, Admit::No) };
            let a = self.admits(&sp, h, from, who);
            (Some(sp), a)
        };
        // Bound once admitted: strangers push out no member's binding.
        if let (Some(u), Admit::Yes) = (who, &a) {
            self.bind(from, u);
        }
        // A storage node: whom it syncs with may have its pushir relay.
        if self.serve.is_some() && matches!(a, Admit::Yes) {
            let mut s = lock(&self.served);
            if s.len() >= 1 << 14 {
                s.clear();
            }
            s.insert(from);
        }
        (sp, a)
    }

    pub(super) fn admits(
        &self,
        sp: &Space,
        h: &Hello,
        d: EndpointId,
        who: Option<Id>,
    ) -> Admit {
        let yes = |ok: bool| if ok { Admit::Yes } else { Admit::No };
        // A storage node: a vouched device, an owner's (signed), or one
        // holding the key its owners write with now (a newcomer).
        if sp.inv.is_none() {
            let hs = lock(&self.hosts);
            let host = hs.get(&sp.id);
            let owner = who.is_some_and(|u| {
                host.is_some_and(|h| h.users.contains_key(&u))
            });
            let now = host
                .is_some_and(|x| x.room != [0; 32] && same(&x.room, &h.room));
            drop(hs);
            return yes(owner || now || self.takes(sp, &d));
        }
        if self.mine(&d) {
            return Admit::Yes;
        }
        let b = lock(&sp.book);
        if b.st.hosts().iter().any(|a| a.id == d) {
            return Admit::Yes;
        }
        let Some(u) = who.filter(|u| !b.st.out(u)) else { return Admit::No };
        let k =
            b.keys.iter().find(|(_, k)| same(&sync::room(&sp.id, k), &h.room));
        match k {
            Some((t, _)) if b.st.stale(t, &crate::state::END, &u) => {
                Admit::Out(b.proof(t))
            }
            Some(_) => Admit::Yes,
            None => Admit::No,
        }
    }

    /// Who proved a device theirs; past 4096, the longest unheard goes.
    pub(super) fn bind(&self, d: EndpointId, u: Id) {
        let mut v = lock(&self.verified);
        if v.len() >= 4096 && !v.contains_key(&d) {
            let old = v.iter().min_by_key(|x| x.1.1).map(|x| *x.0);
            old.map(|o| v.remove(&o));
        }
        v.insert(d, (u, Instant::now()));
    }

    /// An owner's devices, room and epochs for a space this node keeps.
    fn vouch(&self, h: &Hello, from: EndpointId, who: Option<Id>) {
        let user = who.unwrap_or(Id(*from.as_bytes()));
        let ds = std::iter::once(from).chain(h.devices.iter().copied());
        let ds: Vec<_> = ds.take(4096).collect();
        let mut hs = lock(&self.hosts);
        let e = hs.entry(h.space).or_default();
        let was = (e.room, e.tags.len(), e.users.get(&user) == Some(&ds));
        e.users.insert(user, ds);
        if h.room != [0; 32] {
            e.room = h.room;
        }
        e.tags.extend(h.tags.iter().take(4096));
        while e.tags.len() > 4096 {
            e.tags.pop_first();
        }
        let moved = was.0 != e.room;
        let changed = was != (e.room, e.tags.len(), true);
        drop(hs);
        if changed {
            self.save_hosts();
        }
        if moved && let Ok(sp) = self.get(&h.space) {
            self.rehome(&sp);
        }
    }

    pub(super) fn save_hosts(&self) {
        self.stash("hosts", &*lock(&self.hosts));
    }

    /// Every device a storage node's owners vouched for in a space.
    pub(super) fn owners(&self, id: &Id) -> Vec<EndpointId> {
        let h = lock(&self.hosts);
        let all =
            h.get(id).into_iter().flat_map(|h| h.users.values().flatten());
        let mut v: Vec<_> = all.copied().collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Members check every op; a storage node cannot: vouched devices only.
    pub(crate) fn takes(&self, sp: &Space, peer: &EndpointId) -> bool {
        let h = lock(&self.hosts);
        let ours = |h: &Host| h.users.values().any(|v| v.contains(peer));
        sp.inv.is_some() || h.get(&sp.id).is_some_and(ours)
    }

    /// Where its members meet now: a member's current key, or what a
    /// storage node's owners last said.
    pub(crate) fn room(&self, sp: &Space) -> [u8; 32] {
        match &sp.inv {
            Some(inv) => sync::room(&sp.id, &lock(&sp.book).key(inv).0),
            None => lock(&self.hosts).get(&sp.id).map_or([0; 32], |h| h.room),
        }
    }

    /// Moved to a new room: true, and the gossip follows.
    pub(super) fn rehome(&self, sp: &Space) -> bool {
        let r = self.room(sp);
        let moved = std::mem::replace(&mut *lock(&sp.room), r) != r;
        if moved {
            sp.moved.notify_one();
        }
        moved
    }

    /// A sync with `peer` went through: what we wrote reached someone.
    pub(crate) fn reached(self: &Arc<Self>, sp: &Arc<Space>, peer: EndpointId) {
        lock(&self.synced).insert(peer, now());
        // A storage node synced: it keeps all we have here.
        let host = lock(&sp.book).st.hosts().iter().any(|a| a.id == peer)
            || self.stores(&sp.id).iter().any(|s| s.0.id == peer);
        if host {
            lock(&self.kept).insert(sp.id, now());
        }
        // No gossip (a small space): a sync is how a device is first met.
        if sp.tx().is_none() {
            self.neighbor(sp, peer);
        }
        // Reachable, proven: gossip with nobody joins it at once.
        if let Some(tx) = sp.tx().filter(|_| sp.nbrs().is_empty()) {
            tokio::spawn(async move { drop(tx.join_peers(vec![peer]).await) });
        }
        self.reach_device(&sp.id, &peer, now());
        // Synced: it has all we wrote here, whatever its lane missed.
        let d = *peer.as_bytes();
        self.out_done(|o| o.space == sp.id && o.device == d);
        if !std::mem::take(&mut *lock(&sp.undelivered)).is_empty() {
            self.emit(Event::Changed { space: sp.id });
        }
    }

    pub(super) async fn sync_with(
        self: Arc<Self>,
        sp: Arc<Space>,
        peer: EndpointAddr,
    ) {
        let id = peer.id;
        if !self.takes(&sp, &id) || lock(&sp.busy).insert(id, true).is_some() {
            return;
        }
        let again = || {
            let mut b = lock(&sp.busy);
            let again = b.insert(id, false) == Some(true);
            if !again {
                b.remove(&id);
            }
            again
        };
        while again() {
            let stores = self.stores(&sp.id);
            let token = stores.into_iter().find(|s| s.0.id == id).map(|s| s.1);
            let r = async {
                let c = self.connect(peer.clone(), sync::ALPN).await?;
                let n = sync::initiate(&self, &sp, &c, token).await;
                c.close(0u32.into(), b"done");
                n
            };
            let r = tokio::time::timeout(sync::LIMIT, r).await;
            match r.unwrap_or_else(|e| Err(e.into())) {
                Ok(count) => {
                    self.reached(&sp, id);
                    if token.is_some() {
                        self.ask_relays(peer.clone());
                    }
                    if count > 0 {
                        self.emit(Event::Synced { space: sp.id, count });
                    }
                }
                Err(e) => debug!("sync with {}: {e:#}", id.fmt_short()),
            }
        }
    }

    /// Once a run, as a sync with a storage node of ours ends: its pushir
    /// relay (token and all), ours from the next start, whatever our push.
    fn ask_relays(self: &Arc<Self>, to: EndpointAddr) {
        if self.serve.is_some() || self.relayed.swap(true, Relaxed) {
            return;
        }
        let n = self.clone();
        tokio::spawn(async move {
            let Ok(ep) = n.up().await else {
                return n.relayed.store(false, Relaxed);
            };
            match crate::push::relays_of(&ep, to).await {
                Ok(l) => {
                    drop(store::write(&n.dir.join("relays"), l.as_bytes()))
                }
                Err(_) => n.relayed.store(false, Relaxed),
            }
        });
    }

    pub(super) fn neighbor(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        peer: EndpointId,
    ) {
        // Twice a small space's devices, at most: each is synced with.
        let mut nb = lock(&sp.nbrs);
        if nb.len() >= 2 * FANOUT || !nb.insert(peer) {
            return;
        }
        drop(nb);
        self.emit(Event::Changed { space: sp.id });
        let (n, sp) = (self.clone(), sp.clone());
        tokio::spawn(async move {
            // Twice: ops broadcast while we were joining reach neither side.
            // The mesh changed: here, once (with our call); who knew, stays.
            n.clone().sync_with(sp.clone(), peer.into()).await;
            n.here(&sp).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            n.clone().sync_with(sp.clone(), peer.into()).await;
            let Some(ep) = n.ep() else { return };
            let Some(info) = ep.remote_info(peer).await else { return };
            let a = EndpointAddr::from_parts(
                peer,
                info.into_addrs().map(|a| a.into_addr()),
            );
            // Members and storage nodes only, the latest 512: anyone may join.
            let host =
                n.view(&sp.id, |s| s.hosts().iter().any(|h| h.id == peer));
            let ok = n.member_device(&peer) || host == Some(true);
            let keep = !a.is_empty() && ok;
            let mut l = lock(&n.local);
            if keep {
                if l.peers.len() >= 512 && !l.peers.contains_key(&peer) {
                    l.peers.pop_first();
                }
                l.peers.insert(peer, a);
            }
            drop(l);
            n.persist();
        });
    }
}
