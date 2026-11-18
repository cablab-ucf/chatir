//! Live ops, never kept: presence, typing, receipts, calls.

use super::*;

/// Local, sending only what changed: a late tick means we slept (so, as
/// any state change, `resume`); idle past `AWAY`, away (`Gone`, once); a
/// device unheard from twice that long, gone; catching up once as we
/// start; our storage nodes named where we became admin.
pub(super) async fn heartbeat(n: Weak<Node>) {
    let mut t = tokio::time::interval(BEAT);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = now();
    for i in 0u64.. {
        t.tick().await;
        let Some(n) = n.upgrade() else { return };
        if now().saturating_sub(last) > 2 * BEAT.as_millis() as u64 {
            n.resume().await;
        }
        last = now();
        lock(&n.waits).retain(|_, w| w.4.elapsed() < WAITED);
        // Unacked frames of ours whose time came: again (first as we start).
        n.flush_out(i == 0, false);
        // Our lane, proved now and then: only one that works wakes us. A
        // mailbox held here is proved as it connects, watched by pushir.
        if i % LANE_BEATS == 0 && n.serve.is_none() {
            n.phonebook();
            if n.mailbox_up().is_none() {
                n.check_lane().await;
            }
        }
        // Our mailbox off its relay: iroh up, where members dial, until
        // it is back (caught up then).
        if n.mailbox_up() == Some(false) && n.ep().is_none() {
            warn!("our mailbox is off its relay: iroh up");
            if n.up().await.is_ok() {
                n.resume().await;
            }
        }
        // Nothing needs iroh: asleep, our lane wakes it (`up`).
        n.rest_iroh(IDLE).await;
        n.tell_lane().await;
        // In a call is in use.
        if n.in_call().is_some() {
            n.active().await;
        }
        // Untouched a while: idle, still reachable (`Gone` is for quitting).
        if lock(&n.input).elapsed() > AWAY {
            n.rest().await;
        }
        for sp in n.all() {
            let idle = lock(&sp.idle).clone();
            let quiet = |k: &(Id, EndpointId)| match idle.contains(k) {
                true => DEAD_IDLE,
                false => 2 * ALIVE + Duration::from_secs(300),
            };
            let dead: Vec<_> = lock(&sp.seen)
                .iter_mut()
                .filter(|s| s.1.0 && s.1.2.elapsed() > quiet(s.0))
                .map(|((u, d), s)| {
                    s.0 = false;
                    (*u, *d, s.1 + 1)
                })
                .collect();
            // Gone quiet long, no longer a neighbor (a small space's).
            if sp.tx().is_none() {
                let mut nb = lock(&sp.nbrs);
                let before = nb.len();
                nb.retain(|d| !dead.iter().any(|w| w.1 == *d));
                if nb.len() != before {
                    n.emit(Event::Changed { space: sp.id });
                }
            }
            for &w in &dead {
                n.said(&sp, w, None, true);
            }
            if !dead.is_empty() {
                n.emit(Event::Presence { space: sp.id });
            }
            if i == 0 {
                n.bootstrap(&sp).iter().take(3).for_each(|p| n.sync(&sp, *p));
            }
            // Pairings from before `Hold`, and admins since made.
            if i % 10 == 0 {
                for (addr, _) in n.stores(&sp.id) {
                    n.list(&sp, &addr).await;
                }
            }
        }
        // What storage took today, kept across restarts.
        if n.serve.is_some() {
            n.stash("budget", &*lock(&n.budget));
        }
    }
}

impl Node {
    pub fn invisible(&self) -> bool {
        self.invisible.load(Relaxed)
    }

    pub async fn set_invisible(&self, on: bool) {
        self.invisible.store(on, Relaxed);
        lock(&self.local).invisible = on;
        self.persist();
        for sp in self.all() {
            match on {
                true => drop(self.signal(&sp, Op::Gone).await),
                false => self.here(&sp).await,
            }
        }
    }

    /// Typing in `channel` (`on`), or the input empty again: said once
    /// each way (and again only after `TYPING` of more typing).
    pub async fn typing(&self, id: &Id, channel: Ch, on: bool) {
        let t = |(i, c, at): (Id, Ch, Instant)| {
            (i, c) == (*id, channel) && at.elapsed() < TYPING
        };
        let was = {
            let mut cur = lock(&self.typing);
            if on && cur.is_some_and(t) || !on && cur.is_none() {
                return;
            }
            let was = cur.take();
            *cur = on.then(|| (*id, channel, Instant::now()));
            was
        };
        // Elsewhere before, or done: stopped there.
        if let Some((i, c, _)) =
            was.filter(|w| !on || (w.0, w.1) != (*id, channel))
            && let Ok(sp) = self.get(&i)
        {
            self.signal(&sp, Op::Typed { channel: c }).await;
        }
        if let (true, Ok(sp)) = (on, self.get(id)) {
            self.signal(&sp, Op::Typing { channel }).await;
        }
    }

    /// The user did something: back from away (`Here`), or, in use, `Here`
    /// again once `AWAY` has passed, so nobody takes us for gone.
    pub async fn active(&self) {
        *lock(&self.input) = Instant::now();
        let back = self.away.swap(false, Relaxed);
        if back || lock(&self.said).elapsed() > ALIVE {
            for sp in self.all() {
                self.here(&sp).await;
            }
        }
    }

    /// Untouched a while: idle, once, still reachable.
    pub async fn rest(&self) {
        if !self.away.swap(true, Relaxed) {
            for sp in self.all() {
                self.signal(&sp, Op::Idle).await;
            }
        }
    }

    /// Live ops; invisible, only leaving and calls. Returns the sealed op.
    pub(super) async fn signal(&self, sp: &Space, op: Op) -> Option<Vec<u8>> {
        let inv = sp.inv.as_ref()?;
        let (key, locked) = lock(&sp.book).key(inv);
        let quiet = !matches!(
            op,
            Op::Call { .. }
                | Op::Gone
                | Op::Ringing { .. }
                | Op::Declined { .. }
        );
        if locked || quiet && self.invisible() {
            return None;
        }
        let ts = self.tick();
        let raw = self.seal(sp, &key, ts, op).1;
        self.tell(sp, &raw);
        if let Some(tx) = sp.tx() {
            let _ = tx.broadcast(raw.clone().into()).await;
        }
        Some(raw)
    }

    /// What a ring brought (wake.rs): taken as if gossiped.
    pub fn deliver(self: &Arc<Self>, news: crate::push::News) {
        if let Ok(sp) = self.get(&news.0) {
            self.signaled(&sp, &news.1, self.device());
        }
    }

    /// Join a channel's call (leaving any other), or with `None` leave.
    pub async fn call(self: &Arc<Self>, to: Option<(Id, Ch)>) -> Result<()> {
        let old = lock(&self.call).take();
        if let Some(c) = old {
            let sp = self.get(&c.space);
            sp.iter().for_each(|sp| self.emit(Event::Call { space: sp.id }));
            if let Ok(sp) = sp {
                let raw = self.signal(&sp, Op::Call { channel: None }).await;
                // Phones ringing for it: hung up, they stop.
                if self.loud(&sp.id) {
                    self.wake(&sp.id, raw);
                }
            }
        }
        let Some((id, ch)) = to else { return Ok(()) };
        let sp = self.get(&id)?;
        ensure!(!self.locked(&id), LOCKED);
        let run = u64::from_le_bytes(rand());
        let dev = self.device();
        let on = self.callers(&sp, Some(ch)).iter().any(|c| c.0 != dev);
        *lock(&self.call) = Some(Call::new(id, ch, run));
        let raw = self.signal(&sp, Op::Call { channel: Some(ch) }).await;
        self.emit(Event::Call { space: id });
        // Sleeping phones: someone is calling; one answering rings no one.
        if !on {
            self.wake(&id, raw);
        }
        tokio::spawn(self.clone().ring(run));
        // Rung first: opening the sound devices can take a while.
        #[cfg(feature = "voice")]
        self.voice(run).await;
        Ok(())
    }

    /// The call's voice on our devices, by our route (`set_route`).
    #[cfg(feature = "voice")]
    pub(super) async fn voice(self: &Arc<Self>, run: u64) {
        let n = Arc::downgrade(self);
        let send =
            move |f: Vec<u8>| n.upgrade().iter().for_each(|n| n.send_media(&f));
        let (media, route) = (self.media(), *lock(&self.route));
        let start = move || crate::voice::Voice::start(send, media, route);
        let Ok(v) = tokio::task::spawn_blocking(start).await else { return };
        // No devices here: the page's audio, as before.
        let v = v.inspect_err(|e| warn!("voice: {e:#}")).ok();
        if let Some(c) = lock(&self.call).as_mut().filter(|c| c.run == run) {
            c.voice = v;
        }
    }

    /// Where calls are heard (kept for the next), and the routes there are.
    #[cfg(feature = "voice")]
    pub async fn set_route(
        self: &Arc<Self>,
        to: Option<crate::voice::Route>,
    ) -> (crate::voice::Route, Vec<crate::voice::Route>) {
        if let Some(r) = to.filter(|r| *r != *lock(&self.route)) {
            *lock(&self.route) = r;
            // In a call: its voice reopened there.
            let run = lock(&self.call).as_mut().map(|c| {
                c.voice = None;
                c.run
            });
            if let Some(run) = run {
                self.voice(run).await;
            }
        }
        (*lock(&self.route), crate::voice::routes())
    }

    /// A call here rings like a phone: a DM, or a space small enough to be
    /// a group of friends. A big one is told of it quietly.
    pub fn loud(&self, id: &Id) -> bool {
        let n = self.view(id, |s| {
            let ms = s.members();
            let few = ms.iter().filter(|m| m.role != Role::Banned).count();
            s.dm.is_some() || few <= RINGS
        });
        n == Some(true)
    }

    pub fn in_call(&self) -> Option<(Id, Ch)> {
        lock(&self.call).as_ref().map(|c| (c.space, c.channel))
    }

    pub fn calls(&self, id: &Id) -> HashMap<Ch, Vec<Id>> {
        let Ok(sp) = self.get(id) else { return HashMap::new() };
        let mut v: HashMap<Ch, Vec<Id>> = HashMap::new();
        for (d, ch, u) in self.callers(&sp, None) {
            if d != self.device() {
                v.entry(ch).or_default().push(u);
            }
        }
        if let Some((_, ch)) = self.in_call().filter(|c| c.0 == *id) {
            v.entry(ch).or_default().insert(0, self.id());
        }
        v
    }

    /// Whether the call's voice is ours, not the page's.
    pub fn native(&self) -> bool {
        #[cfg(feature = "voice")]
        return lock(&self.call).as_ref().is_some_and(|c| c.voice.is_some());
        #[cfg(not(feature = "voice"))]
        false
    }

    pub fn mute(&self, _on: bool) {
        #[cfg(feature = "voice")]
        if let Some(v) =
            lock(&self.call).as_ref().and_then(|c| c.voice.as_ref())
        {
            v.mute(_on);
        }
    }

    /// Ours and each peer's who spoke lately, while the voice is ours.
    pub fn levels(&self) -> Option<(f32, Vec<(Id, f32)>)> {
        #[cfg(feature = "voice")]
        return lock(&self.call).as_ref()?.voice.as_ref().map(|v| v.levels());
        #[cfg(not(feature = "voice"))]
        None
    }

    pub fn send_media(&self, f: &[u8]) {
        lock(&self.call).iter_mut().for_each(|c| c.send(f));
    }

    pub fn media(&self) -> broadcast::Receiver<(Id, Vec<u8>)> {
        self.media.subscribe()
    }

    /// Who says they are in a call (`ch`'s, if given): members.
    pub(super) fn callers(
        &self,
        sp: &Space,
        ch: Option<Ch>,
    ) -> Vec<(EndpointId, Ch, Id)> {
        // Keys moved on without us (a ban, say): out of every call.
        if self.locked(&sp.id) {
            return vec![];
        }
        let calls = lock(&sp.calls).clone();
        let ok = |u: &Id| self.view(&sp.id, |s| s.role(u) != Role::Banned);
        let v = calls.into_iter().filter_map(|(u, (c, d, _))| Some((d, c?, u)));
        v.filter(|(_, c, u)| {
            ch.is_none_or(|ch| ch == *c) && ok(u) == Some(true)
        })
        .collect()
    }

    /// In a call: one link per device in it, lower id dials. Said as it
    /// starts, and, unanswered, at 3 and 10 s (a mesh still forming drops
    /// what is said); newcomers hear it in answer to their `Here`.
    pub(super) async fn ring(self: Arc<Self>, run: u64) {
        let mut t = tokio::time::interval(Duration::from_secs(1));
        for i in 1u64.. {
            t.tick().await;
            let cur = {
                let c = lock(&self.call);
                c.as_ref()
                    .filter(|c| c.run == run)
                    .map(|c| (c.space, c.channel))
            };
            let Some((id, ch)) = cur else { return };
            let Ok(sp) = self.get(&id) else { return };
            let want = self.callers(&sp, Some(ch));
            if (i == 3 || i == 10) && want.iter().all(|w| w.2 == self.id()) {
                self.signal(&sp, Op::Call { channel: Some(ch) }).await;
            }
            let dial: HashSet<_> = {
                let mut c = lock(&self.call);
                let Some(c) = c.as_mut().filter(|c| c.run == run) else {
                    return;
                };
                c.retain(|d| want.iter().any(|w| w.0 == *d));
                let me = self.device();
                let new = want.into_iter().map(|w| w.0);
                new.filter(|d| *d > me && !c.has(d)).collect()
            };
            for d in dial {
                let hi = async {
                    let conn = self.connect(d, call::ALPN).await.ok()?;
                    let user = self.greet(&conn).await?;
                    Some((user, conn))
                };
                let ten = Duration::from_secs(10);
                if let Ok(Some((user, conn))) =
                    tokio::time::timeout(ten, hi).await
                {
                    self.admit(user, conn);
                }
            }
        }
    }

    pub(super) async fn answer(&self, conn: Connection) {
        let Some((id, ch)) = self.in_call() else { return };
        let Ok(sp) = self.get(&id) else { return };
        let d = conn.remote_id();
        if self.callers(&sp, Some(ch)).iter().any(|w| w.0 == d)
            && let Some(user) = self.greet(&conn).await
        {
            self.admit(user, conn);
        }
    }

    async fn greet(&self, conn: &Connection) -> Option<Id> {
        call::hello(conn, &self.me.user, self.device()).await
    }

    /// A device its user proved theirs and said, from it, is in the call.
    fn admit(&self, user: Id, conn: Connection) {
        let Some((id, ch)) = self.in_call() else { return };
        let Ok(sp) = self.get(&id) else { return };
        if !self.callers(&sp, Some(ch)).contains(&(conn.remote_id(), ch, user))
        {
            return;
        }
        self.bind(conn.remote_id(), user);
        let mut c = lock(&self.call);
        if let Some(c) = c.as_mut().filter(|c| (c.space, c.channel) == (id, ch))
        {
            c.add(user, conn, self.media.clone());
        }
    }

    /// A device of someone's has our ops here up to `ts`.
    pub(super) fn reach_device(&self, id: &Id, d: &EndpointId, ts: u64) {
        let Ok(sp) = self.get(id) else { return };
        let mut r = lock(&sp.reach_dev);
        let at = r.entry(*d).or_insert(ts);
        *at = (*at).max(ts);
        drop(r);
        let proven = lock(&self.verified).get(d).map(|v| v.0);
        let Some(u) = proven.or_else(|| lock(&sp.book).st.owner_of(d)) else {
            return;
        };
        let mut r = lock(&sp.reach);
        let at = r.entry(u).or_default();
        if *at < ts {
            *at = ts;
            drop(r);
            self.emit(Event::Changed { space: *id });
        }
    }

    /// The device `user` was last heard from here (presence, else what it
    /// wrote last), and up to when it has our ops.
    pub fn active_reach(&self, id: &Id, user: &Id) -> u64 {
        let Ok(sp) = self.get(id) else { return 0 };
        let heard = lock(&sp.seen);
        let last =
            heard.iter().filter(|(k, _)| k.0 == *user).max_by_key(|(_, s)| s.1);
        let d = last.map(|(k, _)| k.1).or_else(|| {
            lock(&sp.book).st.devices_of(user).first().map(|x| x.0)
        });
        drop(heard);
        d.and_then(|d| lock(&sp.reach_dev).get(&d).copied()).unwrap_or(0)
    }

    /// Members open but untouched a while: reachable, inactive.
    pub fn idle(&self, id: &Id) -> HashSet<Id> {
        let Ok(sp) = self.get(id) else { return HashSet::new() };
        let idle = lock(&sp.idle);
        let seen = lock(&sp.seen);
        // Idle only if every device of theirs here is.
        let on = |u: &Id| {
            seen.iter().any(|(k, s)| k.0 == *u && s.0 && !idle.contains(k))
        };
        idle.iter().map(|k| k.0).filter(|u| !on(u)).collect()
    }

    /// Up to when `user`'s devices have our ops here.
    pub fn reach(&self, id: &Id, user: &Id) -> Option<u64> {
        lock(&self.get(id).ok()?.reach).get(user).copied()
    }

    /// `user`'s device says our call in `ch` rings there, lately.
    pub fn rings(&self, id: &Id, user: &Id, ch: &Ch) -> bool {
        let Ok(sp) = self.get(id) else { return false };
        let r = lock(&sp.ringing);
        r.get(&(*user, *ch)).is_some_and(|t| t.elapsed() < RANG)
    }

    /// A call to us declined: said once, so its caller hangs up and our
    /// other devices stop ringing.
    pub async fn decline(&self, id: &Id, channel: Ch) {
        if let Ok(sp) = self.get(id) {
            self.signal(&sp, Op::Declined { channel }).await;
        }
    }

    /// A call to us rings here: its caller hears so, and hears ringback.
    pub async fn ringing(&self, id: &Id, channel: Ch) {
        if let Ok(sp) = self.get(id) {
            self.signal(&sp, Op::Ringing { channel }).await;
        }
    }

    /// Ours, not yet on any other device.
    pub fn undelivered(&self, id: &Id) -> HashSet<Id> {
        self.get(id).map(|s| lock(&s.undelivered).clone()).unwrap_or_default()
    }

    pub(super) fn signaled(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        raw: &[u8],
        from: EndpointId,
    ) {
        let Ok(e) = postcard::from_bytes::<Env>(raw) else { return };
        if sp.inv.is_none() {
            // A storage node can't check gossip: it fetches from who passed it.
            let i = Id(blake3::hash(raw).into());
            if !e.live && !lock(&sp.book).has(&i, e.ts) {
                self.sync(sp, from);
            }
            return;
        }
        if !e.live {
            self.ingest(sp, vec![raw.to_vec()], false);
            return;
        }
        // Presence, typing and calls only from who holds the key now,
        // unbanned (a ban's rekey may wait for an admin).
        let s = lock(&sp.book).check(&sp.id, &e);
        let Some(s) = s else { return };
        // Recent, and once: an old ring passed on again rings nothing.
        let mut heard = lock(&sp.heard);
        heard.retain(|_, t| t.elapsed() < Duration::from_secs(240));
        if s.ts.abs_diff(now()) > 120_000
            || heard.insert(s.id, Instant::now()).is_some()
        {
            return;
        }
        drop(heard);
        let (who, here) = ((s.author, s.device), s.op != Op::Gone);
        let changed = {
            let mut seen = lock(&sp.seen);
            let was = seen.get(&who).copied();
            let fresh = was.is_none_or(|w| w.1 < s.ts);
            if fresh {
                seen.insert(who, (here, s.ts, Instant::now()));
            }
            fresh && was.is_some_and(|w| w.0) != here
        };
        if changed {
            self.emit(Event::Presence { space: sp.id });
        }
        // Back: what waits on it (an offer, a pull) tries now.
        if changed && here {
            self.send.kick.notify_waiters();
        }
        // Gone, or back: a call it said before is over (if still on, it
        // says so again, after).
        if matches!(s.op, Op::Gone | Op::Here) {
            self.said(sp, (s.author, s.device, s.ts), None, true);
        }
        // Idle, or back: one op each way (anything else said is a touch).
        let was = {
            let mut idle = lock(&sp.idle);
            match s.op {
                Op::Idle => !idle.insert((s.author, s.device)),
                _ => idle.remove(&(s.author, s.device)),
            }
        };
        if was != (s.op == Op::Idle) {
            self.emit(Event::Presence { space: sp.id });
        }
        if s.op == Op::Gone && sp.tx().is_none() && lock(&sp.nbrs).remove(&from)
        {
            self.emit(Event::Changed { space: sp.id });
        }
        match s.op {
            Op::Typing { channel } | Op::Typed { channel } => {
                let (user, on) = (s.author, matches!(s.op, Op::Typing { .. }));
                self.emit(Event::Typing { space: sp.id, channel, user, on });
            }
            // New here, or back from sleep: answered once with our call
            // (none, mostly), which says we are here without asking back.
            // Not in a call: three times a minute a device at most, however
            // often it comes and goes (each answer goes to every device: N²
            // for a member toggling `Gone` and `Here`), a lost one retried.
            Op::Here => {
                let id = sp.id;
                let ch = move |n: &Node| n.in_call().filter(|c| c.0 == id);
                let fresh = || {
                    let mut a = lock(&self.answered);
                    a.retain(|_, t| t.0.elapsed() < Duration::from_secs(60));
                    let t =
                        a.entry((id, s.device)).or_insert((Instant::now(), 0));
                    t.1 += 1;
                    t.1 <= 3
                };
                if (changed && !self.invisible() && fresh())
                    || ch(self).is_some()
                {
                    let (n, sp) = (self.clone(), sp.clone());
                    // Our call as it is when said: a call begun meanwhile
                    // is never undone by a "none" stamped after it.
                    tokio::spawn(async move {
                        let call = Op::Call { channel: ch(&n).map(|c| c.1) };
                        n.signal(&sp, call).await
                    });
                }
            }
            // Their device has our DM's news up to `ts`: proof of it.
            Op::Got { ts } => {
                let mut r = lock(&sp.reach);
                let at = r.entry(s.author).or_default();
                if *at < ts.min(s.ts) {
                    *at = ts.min(s.ts);
                    drop(r);
                    self.emit(Event::Changed { space: sp.id });
                }
            }
            Op::Ringing { channel } => {
                if self.in_call() == Some((sp.id, channel)) {
                    lock(&sp.ringing)
                        .insert((s.author, channel), Instant::now());
                    self.emit(Event::Call { space: sp.id });
                }
            }
            Op::Declined { channel } => {
                let (space, user) = (sp.id, s.author);
                self.emit(Event::Declined { space, channel, user });
            }
            Op::Call { channel } => {
                self.said(sp, (s.author, s.device, s.ts), channel, false);
            }
            _ => {}
        }
    }

    /// A member's call as said at a time: older news (a ring come late)
    /// changes nothing; a device back or gone (`own`) ends only its own.
    pub(super) fn said(
        &self,
        sp: &Space,
        (u, d, ts): (Id, EndpointId, u64),
        ch: Option<Ch>,
        own: bool,
    ) {
        let mut c = lock(&sp.calls);
        let was = c.get(&u).copied();
        if was.is_some_and(|w| w.2 >= ts) || own && was.is_none_or(|w| w.1 != d)
        {
            return;
        }
        c.insert(u, (ch, d, ts));
        drop(c);
        if was.and_then(|w| w.0) != ch {
            self.emit(Event::Call { space: sp.id });
        }
    }

    /// Here (a state change: started, back, another network): one `Here`,
    /// and our call, if any, for whoever missed it.
    pub(super) async fn here(&self, sp: &Space) {
        if self.get(&sp.id).is_err() || self.away.load(Relaxed) {
            return; // shutting down, or away
        }
        *lock(&self.said) = Instant::now();
        self.signal(sp, Op::Here).await;
        if let Some((_, ch)) = self.in_call().filter(|c| c.0 == sp.id) {
            self.signal(sp, Op::Call { channel: Some(ch) }).await;
        }
    }
}
