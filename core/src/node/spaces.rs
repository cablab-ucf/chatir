//! Spaces and DMs: joined, left, posted to, rekeyed.

use super::*;

impl Node {
    /// Spaces you are in, by name; DMs last.
    pub fn spaces(&self) -> Vec<Id> {
        let mut v: Vec<_> = (self.all().into_iter())
            .filter(|s| s.id != self.own && s.inv.is_some())
            .filter_map(|s| {
                self.view(&s.id, |st| {
                    (st.dm.is_some(), st.name().to_lowercase(), s.id)
                })
            })
            .collect();
        v.sort();
        v.into_iter().map(|v| v.2).collect()
    }

    pub fn held(&self) -> Vec<(Id, usize)> {
        let all = self.all().into_iter().filter(|s| s.id != self.own);
        all.map(|s| (s.id, lock(&s.book).at.len())).collect()
    }

    /// Keys moved on without us: we can read the past only.
    /// Who left us out of the space's newest key with a box of ours that
    /// opens to no key of it (on purpose, or splitting the space): said.
    pub fn left_out_by(&self, id: &Id) -> Option<Id> {
        let sp = self.get(id).ok()?;
        let b = lock(&sp.book);
        let e = b.st.epoch()?;
        let held = b.keys.contains_key(&e.tag);
        let by = b.left_out.iter().find(|x| x.0 == e.tag).map(|x| x.1);
        by.filter(|_| !held)
    }

    pub fn locked(&self, id: &Id) -> bool {
        let Ok(sp) = self.get(id) else { return false };
        let Some(inv) = &sp.inv else { return false };
        let b = lock(&sp.book);
        // A DM with someone who retired their identity: a thief may read.
        let me = self.me.id();
        let gone = b.st.peer(&me).is_some_and(|p| b.st.out(&p));
        gone || b.key(inv).1
    }

    pub fn peers(&self, id: &Id) -> usize {
        self.get(id).map_or(0, |s| lock(&s.nbrs).len())
    }

    /// Connected devices here, each a member's, a storage node's, or unknown.
    pub fn neighbors(&self, id: &Id) -> Vec<(EndpointId, String)> {
        let Ok(sp) = self.get(id) else { return vec![] };
        let near: Vec<_> = lock(&sp.nbrs).iter().copied().collect();
        let mine = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        // Our own devices and storage nodes go by the names we gave them.
        let named: HashMap<_, _> =
            near.iter().filter_map(|d| Some((*d, self.label(d)?))).collect();
        let me = self.id();
        let b = lock(&sp.book);
        let host = |d: &EndpointId| {
            b.st.hosts().iter().any(|h| h.id == *d)
                || mine.iter().any(|(a, _)| a.id == *d)
        };
        let who = |d: &EndpointId| match (named.get(d), b.st.owner_of(d)) {
            (Some(n), _) => format!("{n} (yours)"),
            (_, Some(u)) if u == me => "a device of yours".into(),
            (_, Some(u)) => b.st.display_name(&u),
            _ if host(d) => "a storage node".into(),
            _ => "a device not known yet".into(),
        };
        near.into_iter().map(|d| (d, who(&d))).collect()
    }

    /// Members with a device here, and us unless invisible.
    pub fn online(&self, id: &Id) -> HashSet<Id> {
        let Ok(sp) = self.get(id) else { return HashSet::new() };
        let seen = lock(&sp.seen);
        let mut v: HashSet<_> =
            seen.iter().filter(|s| s.1.0).map(|s| s.0.0).collect();
        if !self.invisible() {
            v.insert(self.id());
        }
        v
    }

    pub(super) fn since(&self, id: &Id, ch: &Ch) -> u64 {
        let l = lock(&self.local);
        l.read.get(&(*id, *ch)).or(l.joined.get(id)).copied().unwrap_or(0)
    }

    /// Posts by others among `ids`, just had: noted as had (`rung`, so a
    /// ring that brings one again shows nothing); dated before what was
    /// read in their channel (written offline, a clock behind, a lane that
    /// lost them), unread again from there and, if `tell`, told once by
    /// `notices`. Never lost among the read. (Dated before we joined, it is
    /// history; after what was read, plain unread.)
    pub(super) fn late(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        ids: &[Id],
        tell: bool,
    ) {
        if sp.inv.is_none() || sp.id == self.own || ids.is_empty() {
            return;
        }
        let me = self.id();
        let posts: Vec<(Id, Ch, u64)> = self
            .view(&sp.id, |s| {
                let of = |i: &Id| s.message(i).map(|(ch, m)| (*i, ch, m));
                let others = ids.iter().filter_map(of);
                others
                    .filter(|x| x.2.author != me)
                    .map(|(i, ch, m)| (i, ch, m.ts))
                    .collect()
            })
            .unwrap_or_default();
        if posts.is_empty() {
            return;
        }
        let had: Vec<Id> = posts.iter().map(|x| x.0).collect();
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || rung_add(&dir, &had));
        let late: Vec<(Id, Ch, Id)> = {
            let mut l = lock(&self.local);
            let late = posts.into_iter().filter(|(_, ch, ts)| {
                let read = l.read.get(&(sp.id, *ch)).copied();
                read.is_some_and(|r| *ts <= r)
            });
            let late: Vec<_> = late.collect();
            for (_, ch, ts) in &late {
                let r = l.read.entry((sp.id, *ch)).or_default();
                *r = (*r).min(ts.saturating_sub(1));
            }
            late.into_iter().map(|(i, ch, _)| (sp.id, ch, i)).collect()
        };
        if !late.is_empty() {
            self.persist();
            if tell {
                lock(&self.late).extend(late);
            }
        }
    }

    /// Read here; in a DM, its sender is told (not while invisible).
    pub fn mark_read(self: &Arc<Self>, id: &Id, ch: &Ch) {
        let latest = self.view(id, |s| s.latest(ch)).flatten().unwrap_or(0);
        let latest = latest.min(now());
        if self.since(id, ch) < latest {
            lock(&self.local).read.insert((*id, *ch), latest);
            self.persist();
            let dm = self.view(id, |s| s.dm.is_some()) == Some(true);
            // Batched: one `Read` per channel per 2 s, the latest.
            let first =
                || lock(&self.reading).insert((*id, *ch), latest).is_none();
            if let (true, false, Ok(sp)) = (dm, self.invisible(), self.get(id))
                && first()
            {
                let (n, ch) = (self.clone(), *ch);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let ts = lock(&n.reading).remove(&(sp.id, ch));
                    let ts = ts.unwrap_or(latest);
                    if n.invisible() {
                        return;
                    }
                    let _ = n.post(&sp, Op::Read { channel: ch, ts }).await;
                });
            }
        }
    }

    /// `(unread, mentions)` since `ch` was last read here.
    pub fn unread(&self, id: &Id, ch: &Ch) -> (usize, usize) {
        let (since, me) = (self.since(id, ch), self.id());
        self.view(id, |s| s.unread(ch, since, &me)).unwrap_or_default()
    }

    pub(super) fn open(
        self: &Arc<Self>,
        inv: Option<Invite>,
        id: Id,
        peers: Vec<EndpointAddr>,
    ) -> Result<Arc<Space>> {
        if let Ok(sp) = self.get(&id) {
            return Ok(sp);
        }
        let (mut work, mut at) = (vec![], vec![]);
        let log = Log::open(&self.log(&id), |o, checked, raw| {
            let Ok(e) = postcard::from_bytes::<Env>(&raw) else { return };
            // A ring, kept unchecked, from too far ahead (`woken`, older).
            if checked == 0 && e.ts > now() + SKEW {
                return;
            }
            let i = Id(blake3::hash(&raw).into());
            at.push((i, e.ts, o, raw.len()));
            work.push((i, e, Src::Log(checked)));
        })?;
        let st = inv.as_ref().map(State::new).unwrap_or_default();
        let mut b = Book::new(log, st);
        at.into_iter().for_each(|(i, ts, o, l)| b.index(i, ts, o, l));
        let rej: Vec<(Id, u32)> = store::load(&self.rejected(&id));
        for (i, d) in rej {
            b.reject(i, d, true);
        }
        if let Some(i) = &inv {
            b.keys.insert(tag(&i.key), i.key);
            b.fold(&id, &self.me, work);
        }
        // Rung asleep (`Src::Log(0)`): `late` below.
        let rung = std::mem::take(&mut b.posted);
        let boot = peers.iter().chain(inv.iter().flat_map(|i| &i.peers));
        boot.filter(|a| a.id != self.device())
            .for_each(|a| self.lookup.add_endpoint_info(a.clone()));
        let (book, tx, nbrs) =
            (Mutex::new(b), Mutex::new(None), Mutex::default());
        let sp = Arc::new(Space {
            id,
            inv,
            book,
            tx,
            nbrs,
            seen: Mutex::default(),
            heard: Mutex::default(),
            calls: Mutex::default(),
            busy: Mutex::default(),
            vouched: AtomicUsize::new(0),
            room: Mutex::new([0; 32]),
            moved: Arc::default(),
            fixed: Mutex::default(),
            undelivered: Mutex::default(),
            reach: Mutex::default(),
            reach_dev: Mutex::default(),
            idle: Mutex::default(),
            ringing: Mutex::default(),
            task: OnceLock::new(),
        });
        {
            let mut m = write(&self.spaces);
            if let Some(sp) = m.get(&id) {
                return Ok(sp.clone());
            }
            m.insert(id, sp.clone());
        }
        // A contact card has no room: its holders meet nobody but its owner.
        if !sp.inv.as_ref().is_some_and(Invite::card) {
            let boot = peers.iter().map(|a| a.id).collect();
            let task = pump(self.clone(), Arc::downgrade(&sp), boot);
            let _ = sp.task.set(AbortOnDropHandle::new(tokio::spawn(task)));
        }
        self.bootstrap(&sp).into_iter().take(4).for_each(|p| self.sync(&sp, p));
        // Rung asleep (told there already): unread if they came late.
        self.late(&sp, &rung, false);
        self.emit(Event::Changed { space: id });
        tokio::spawn(self.clone().effects(sp.clone()));
        Ok(sp)
    }

    pub(super) fn log(&self, id: &Id) -> PathBuf {
        self.dir.join("spaces").join(format!("{id}.log"))
    }

    pub(super) fn rejected(&self, id: &Id) -> PathBuf {
        self.log(id).with_extension("rej")
    }

    pub(super) fn close(&self, id: &Id) {
        drop(write(&self.spaces).remove(id));
        let _ = std::fs::remove_file(self.log(id));
        let _ = std::fs::remove_file(self.rejected(id));
        let mut l = lock(&self.local);
        l.read.retain(|(s, _), _| s != id);
        l.joined.remove(id);
        drop(l);
        if lock(&self.hosts).remove(id).is_some() {
            self.save_hosts();
        }
        self.persist();
        if lock(&self.closed).contains_key(id) {
            self.hide(id, false);
        }
        self.emit(Event::Changed { space: *id });
    }

    /// Our own relays (`--relay`): no DNS to find a device by id, so each
    /// is looked for on them, where it homes as we do.
    /// No lookup service says where a device is: each is tried on the
    /// relays links home on (the lab's or our own), and on the one we are
    /// on now (n0's, while the lab's is down for us all), beside whatever
    /// addresses we met it at.
    pub(super) fn findable(&self, ids: &[EndpointId]) {
        let mut urls = homes(&self.net);
        let ours = self.ep().map(|e| e.addr());
        urls.extend(ours.iter().flat_map(|a| a.relay_urls().cloned()));
        if urls.is_empty() {
            return;
        }
        for id in ids {
            let at = urls.iter().cloned().map(iroh::TransportAddr::Relay);
            self.lookup.add_endpoint_info(EndpointAddr::from_parts(*id, at));
        }
    }

    pub(super) fn bootstrap(&self, sp: &Space) -> Vec<EndpointId> {
        let b = lock(&sp.book);
        let hosts = b.st.hosts();
        hosts.iter().for_each(|a| self.lookup.add_endpoint_info(a.clone()));
        let mut v: Vec<_> = hosts.into_iter().map(|a| a.id).collect();
        // Devices we synced with first (proven to answer), newest first
        // within each: dates are claimed, a sync is not.
        let mut ds = b.st.devices(16);
        let synced = lock(&self.synced);
        ds.sort_by_key(|d| !synced.contains_key(d));
        drop(synced);
        v.extend(ds);
        v.extend(sp.inv.iter().flat_map(|i| i.peers.iter().map(|a| a.id)));
        v.extend(self.owners(&sp.id).into_iter().take(16));
        let me = self.device();
        let mut seen = HashSet::new();
        v.retain(|p| *p != me && seen.insert(*p));
        self.findable(&v);
        v
    }

    pub(crate) fn ingest(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        raws: Vec<Vec<u8>>,
        mine: bool,
    ) -> usize {
        // A storage node keeps only the epochs its owners named.
        let tags = sp.inv.is_none().then(|| {
            let h = lock(&self.hosts);
            h.get(&sp.id).map(|h| h.tags.clone()).unwrap_or_default()
        });
        let mut posted = vec![];
        let new = {
            let mut b = lock(&sp.book);
            let rejects = b.rejects;
            let mut fresh = vec![];
            for raw in raws {
                let Ok(e) = postcard::from_bytes::<Env>(&raw) else { continue };
                // In its one form: an envelope with bytes after it would
                // be another id, kept again, for whoever cared to pad it.
                if postcard::to_allocvec(&e).is_ok_and(|c| c != raw) {
                    continue;
                }
                let i = Id(blake3::hash(&raw).into());
                if !e.live && e.ts <= now() + SKEW && !b.has(&i, e.ts) {
                    fresh.push((i, e, raw));
                }
            }
            match sp.inv {
                // A storage node keeps, unopened, what vouched devices send,
                // within the day's budget.
                None => {
                    let tags = tags.unwrap_or_default();
                    let ok =
                        |e: &Env| tags.is_empty() || tags.contains(&e.epoch);
                    let fits = |r: &[u8]| {
                        self.budget(&sp.id, r.len() as u64, false).is_ok()
                    };
                    let keep =
                        fresh.into_iter().filter(|(_, e, r)| ok(e) && fits(r));
                    keep.filter(|(i, e, r)| b.store(*i, e.ts, r, 0, false))
                        .count()
                }
                Some(_) => {
                    let w = fresh
                        .into_iter()
                        .map(|(i, e, r)| (i, e, Src::Net(r, mine)));
                    let new = b.fold(&sp.id, &self.me, w.collect());
                    posted = std::mem::take(&mut b.posted);
                    if b.rejects != rejects {
                        let r: Vec<_> = b.rejected.iter().collect();
                        if let Err(e) = store::save(&self.rejected(&sp.id), &r)
                        {
                            warn!("saving refused ops: {e}");
                        }
                    }
                    new
                }
            }
        };
        if new > 0 {
            if !mine {
                self.late(sp, &posted, true);
            }
            self.emit(Event::Changed { space: sp.id });
            tokio::spawn(self.clone().effects(sp.clone()));
            // A DM's news from them, here: they see ✓✓.
            let dm = lock(&sp.book).st.dm.is_some();
            if dm && !mine {
                let (n, sp) = (self.clone(), sp.clone());
                tokio::spawn(async move {
                    n.signal(&sp, Op::Got { ts: now() }).await;
                });
            }
        }
        new
    }

    /// DMs to accept, spaces to open or close, epochs to replace, keys to pass.
    async fn effects(self: Arc<Self>, sp: Arc<Space>) {
        // Our own space: a level another device of ours set, told here too.
        if sp.id == self.own {
            for id in self.spaces() {
                let _ = self.tell_push(&id).await;
            }
        }
        // A new key: a new room, which our storage nodes hear of at once.
        if self.rehome(&sp) && sp.inv.is_some() {
            self.phonebook();
            self.stores(&sp.id).into_iter().for_each(|s| self.sync(&sp, s.0));
            // Our ring code too: the old one, which those cut off hold, dies.
            let me = self.id();
            let theirs =
                sp.inv.as_ref().is_some_and(|i| i.card() && i.owner != me);
            if sp.id != self.own && !self.locked(&sp.id) && !theirs {
                let _ = self.tell_push(&sp.id).await;
            }
        }
        let (knocks, fix) = {
            let mut b = lock(&sp.book);
            let admin = b.st.role(&self.id()) == Role::Admin;
            let locked = sp.inv.as_ref().is_none_or(|i| b.key(i).1);
            let lost = !locked && !b.orphans().is_empty();
            let fix = admin && b.st.dm.is_none() && (b.st.leaks() || lost);
            (std::mem::take(&mut b.knocks), fix)
        };
        let me = self.id();
        for inv in knocks {
            let known =
                self.view(&self.own, |s| s.spaces().contains_key(&inv.id()));
            // One DM per person: once both are in one, or one of theirs
            // waits for them, knocks open no more. Before that (two made at
            // once, or one lost), take theirs. A few waiting in all: anyone
            // holding our card may knock, under any name.
            let by = |sp: &Arc<Space>| sp.inv.as_ref().map(|i| i.owner);
            let dms = self.dms_with(&inv.owner);
            let had = dms.iter().any(|x| x.1 || by(&x.0) == Some(inv.owner));
            if known != Some(false) || had || self.waiting() >= 64 {
                continue;
            }
            // All news: its first message came before we were in it.
            lock(&self.local).joined.insert(inv.id(), 0);
            lock(&self.prefs).told.insert(inv.id(), 0);
            if let Err(e) = self.enter(inv).await {
                warn!("accepting a DM: {e}");
            }
        }
        if sp.id == self.own {
            self.reconcile().await;
        }
        // A card we took: a DM with its owner, once their key is here.
        let card = sp.inv.as_ref().filter(|i| i.card() && i.owner != me);
        if let Some(o) = card.map(|i| i.owner)
            && self.dms_with(&o).is_empty()
        {
            let _ = self.dm(&sp.id, &o).await;
        }
        // A DM that another, the one both sides pick, replaced: put away.
        let peer = self.view(&sp.id, |s| s.peer(&me)).flatten();
        let pick = peer.and_then(|p| self.dm_of(&p));
        if pick.is_some_and(|p| p.id != sp.id) && !self.hidden(&sp.id) {
            self.hide(&sp.id, true);
        }
        // Members came or went: our storage nodes hear at once whom to take.
        let n = sp.devices().len();
        if sp.inv.is_some() && sp.vouched.swap(n, Relaxed) != n {
            // Their names too, for a ring while this node is down.
            self.phonebook();
            self.stores(&sp.id).into_iter().for_each(|s| self.sync(&sp, s.0));
        }
        if fix {
            // Admins may race to fix it; a little jitter keeps that rare.
            let wait = 1000 + u16::from_le_bytes(rand()) as u64 % 9000;
            tokio::time::sleep(Duration::from_millis(wait)).await;
            if let Err(e) = self.repair(&sp).await {
                warn!("rekey: {e}");
            }
        }
    }

    /// Replace an epoch a banned member may hold; else hand on keys it cut off.
    pub(super) async fn repair(
        self: &Arc<Self>,
        sp: &Arc<Space>,
    ) -> Result<()> {
        let (leaks, lost) = {
            let b = lock(&sp.book);
            (b.st.leaks(), b.orphans())
        };
        // Once a minute at most: a rekey that does not count (the fold's
        // word, not ours) must not call for the next.
        if leaks {
            {
                let mut at = lock(&sp.fixed);
                if at.is_some_and(|t| t.elapsed() < BEAT) {
                    return Ok(());
                }
                *at = Some(Instant::now());
            }
            return self.rekey(&sp.id).await;
        }
        for key in lost {
            self.post(sp, Op::Back { key }).await?;
        }
        Ok(())
    }

    /// As an admin, rekey now if someone banned may hold the current key.
    async fn secure(self: &Arc<Self>, id: &Id) -> Result<()> {
        let me = self.id();
        match self.view(id, |s| s.leaks() && s.role(&me) == Role::Admin) {
            Some(true) => self.rekey(id).await,
            _ => Ok(()),
        }
    }

    /// Open and close spaces to match our own space's record.
    pub(super) async fn reconcile(self: &Arc<Self>) {
        let me = self.id();
        let name = self.view(&self.own, |s| s.profile(&me).map(str::to_string));
        if let Some(name) = name.flatten() {
            *lock(&self.name) = name;
        }
        let want =
            self.view(&self.own, |s| s.spaces().clone()).unwrap_or_default();
        for (id, inv) in want {
            match inv {
                Some(inv) if self.get(&id).is_err() => {
                    lock(&self.local).joined.entry(id).or_insert_with(now);
                    let mine = self.get(&self.own).map(|o| self.bootstrap(&o));
                    let mine =
                        mine.unwrap_or_default().into_iter().map(Into::into);
                    if let Err(e) = self.open(Some(inv), id, mine.collect()) {
                        warn!("opening {id:?}: {e}");
                    }
                }
                None if self.get(&id).is_ok() => self.close(&id),
                _ => {}
            }
        }
        self.persist();
    }

    pub(super) async fn post(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        op: Op,
    ) -> Result<Id> {
        Ok(self.send(sp, op).await?.0)
    }

    pub(super) async fn send(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        op: Op,
    ) -> Result<(Id, Vec<u8>)> {
        let inv = sp.inv.as_ref().ok_or_else(|| anyhow!("only hosted here"))?;
        let (key, ts, moved) = {
            let b = lock(&sp.book);
            let (key, locked) = b.key(inv);
            // Locked out, admins can still ban and rekey, dated after the rest.
            let fix = matches!(op, Op::Role { .. } | Op::Rekey { .. });
            ensure!(!locked || fix, LOCKED);
            let me = self.me.id();
            let gone = b.st.peer(&me).is_some_and(|p| b.st.out(&p));
            ensure!(!gone || op.live(), "they retired this identity");
            let after = if fix { b.st.horizon() + 1 } else { 0 };
            // Our lane, moved (not first told), in a small space.
            let moved = matches!(op, Op::Push { .. })
                && b.st.push(&me, &self.device()).is_some()
                && b.st.devices(FANOUT + 1).len() <= FANOUT;
            (key, self.tick().max(after), moved)
        };
        let rekey = matches!(op, Op::Rekey { .. });
        let live = op.live();
        let (s, raw) = self.seal(sp, &key, ts, op);
        // Refused here (a role or key moved under us): said, never a
        // message that silently goes nowhere.
        let kept = self.ingest(sp, vec![raw.clone()], true);
        ensure!(
            live || kept > 0,
            "refused here: your role or this space's key changed"
        );
        self.tell(sp, &raw);
        // Asleep, members would ring the old lane until they next met us.
        if moved {
            self.wake(&sp.id, Some(raw.clone()));
        }
        // A rekey to those it cut off too: no key for them, only the news
        // that theirs moved on (they show the space locked).
        if rekey {
            let cut = lock(&sp.book).st.cut_devices();
            self.dial(sp.id, &raw, cut);
        }
        if sp.nbrs().is_empty() {
            lock(&sp.undelivered).insert(s.id);
        }
        match sp.tx().filter(|_| raw.len() <= GOSSIP_MAX) {
            Some(tx) => {
                drop(tx.broadcast(raw.clone().into()).await);
            }
            // Too big to tell (`tell`): a sync carries it.
            None if raw.len() > GOSSIP_MAX => {
                sp.nbrs().into_iter().for_each(|p| self.sync(sp, p))
            }
            None => {}
        }
        // Gossip with nobody in it holds it until the next sync, minutes
        // away: hand it to the space's devices directly, now.
        if sp.nbrs().is_empty() {
            self.bootstrap(sp)
                .into_iter()
                .take(4)
                .for_each(|p| self.sync(sp, p));
        }
        Ok((s.id, raw))
    }

    /// Publish a user-level op; the fold enforces who may do what.
    pub async fn publish(self: &Arc<Self>, id: &Id, op: Op) -> Result<Id> {
        let fits = |t: &str, max| !t.trim().is_empty() && t.len() <= max;
        let ok = match &op {
            Op::Post { text: t, files, .. } => {
                let empty = t.is_empty() && !files.is_empty();
                (empty || fits(t, MAX_TEXT)) && files.len() <= 10
            }
            Op::Edit { text: Some(t), .. } => fits(t, MAX_TEXT),
            Op::Rename { name: n } | Op::Channel { name: Some(n), .. } => {
                fits(n, MAX_NAME)
            }
            // Named nothing: deleted.
            Op::Topic { name: n, .. } => n.is_empty() || fits(n, MAX_NAME),
            Op::React { emoji, .. } => fits(emoji, 32),
            Op::Role { role, .. } => *role != Role::Banned,
            Op::Order { channels } => channels.len() <= 500,
            Op::Edit { .. } | Op::Channel { .. } | Op::Pin { .. } => true,
            // A second hand-over forfeits: only the owner sends one.
            Op::Owner { .. } => self.view(id, State::head) == Some(self.id()),
            _ => false,
        };
        let ok = ok && op::fits(&op);
        ensure!(ok, "that cannot be sent (empty, too long, or not allowed)");
        // Our own space takes notes to self, nothing that runs a space.
        let note = matches!(
            op,
            Op::Post { .. }
                | Op::Edit { .. }
                | Op::React { .. }
                | Op::Pin { .. }
                | Op::Topic { .. }
        );
        ensure!(*id != self.own || note, "not a space");
        let role = matches!(op, Op::Role { .. });
        let msg = matches!(op, Op::Post { .. }) && *id != self.own;
        let (r, raw) = self.send(&self.get(id)?, op).await?;
        if msg {
            self.wake(id, Some(raw));
        }
        // A demotion can revive an older epoch: replace it now, not jittered.
        if role {
            self.secure(id).await?;
        }
        Ok(r)
    }

    async fn enter(self: &Arc<Self>, inv: Invite) -> Result<Id> {
        let id = inv.id();
        ensure!(self.serve.is_none(), "storage nodes do not join spaces");
        let own = self.get(&self.own)?;
        let mut mine = inv.clone();
        mine.peers.insert(0, self.up().await?.addr());
        self.post(&own, Op::Joined { invite: mine }).await?;
        lock(&self.local).joined.entry(id).or_insert_with(now);
        // A card's holder says nothing there: its knock is all the owner
        // needs. Nobody gets a ring code from a card.
        let (card, ours) = (inv.card(), inv.owner == self.id());
        let sp = self.open(Some(inv), id, vec![])?;
        if !card || ours {
            let (name, kem) = (self.name(), self.me.pk());
            self.post(&sp, Op::Profile { name, kem }).await?;
        }
        // Our own card too: a holder's knock wakes us for it.
        if !card || ours {
            let _ = self.tell_push(&id).await;
        }
        // A ring in it rings at once even if this node never sleeps
        // cleanly (Android kills it): the phonebook has it now.
        self.phonebook();
        Ok(id)
    }

    pub async fn create(self: &Arc<Self>, name: &str) -> Result<Id> {
        let name = named(name)?;
        let inv = Invite::new(self.id(), name, None);
        let (epoch, id) = (tag(&inv.key), self.enter(inv).await?);
        let sp = self.get(&id)?;
        self.post(&sp, Op::Rekey { epoch, boxes: vec![] }).await?;
        self.post(&sp, Op::Rename { name: name.into() }).await?;
        let general =
            Op::Channel { id: Ch(rand()), name: Some("general".into()) };
        self.post(&sp, general).await?;
        Ok(id)
    }

    pub async fn join(self: &Arc<Self>, code: &str) -> Result<Id> {
        let inv = Invite::decode(code)?;
        // Someone's contact code: they are woken by the lane it carries.
        let lane = uncode::<(Invite, String)>("join", code).ok();
        let lane = lane.and_then(|x| crate::push::lane_of(&x.1));
        if inv.card()
            && inv.owner != self.id()
            && let Some(l) = lane
        {
            let (n, card, owner) = (self.clone(), inv.id(), inv.owner);
            tokio::spawn(async move {
                // In, first: `meet` dials what it holds.
                tokio::time::sleep(Duration::from_millis(500)).await;
                n.meet(card, owner, l).await
            });
        }
        let Ok(sp) = self.get(&inv.id()) else { return self.enter(inv).await };
        // Already in: a fresh invite carries the key a rekey left us out of.
        if lock(&sp.book).learn(&sp.id, &self.me, inv.key) {
            let mut mine = inv.clone();
            mine.peers.insert(0, self.up().await?.addr());
            self.post(&self.get(&self.own)?, Op::Joined { invite: mine })
                .await?;
            for a in inv.peers {
                self.lookup.add_endpoint_info(a.clone());
                self.sync(&sp, a);
            }
            self.emit(Event::Changed { space: sp.id });
            tokio::spawn(self.clone().effects(sp.clone()));
        }
        Ok(sp.id)
    }

    pub(super) async fn addr(&self) -> Result<EndpointAddr> {
        let relay = !matches!(self.net, Net::Lan | Net::Direct);
        let ep = self.up().await?;
        for _ in 0..100 {
            let a = ep.addr();
            if a.addrs.iter().any(|t| t.is_relay() || !relay && t.is_ip()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut a = ep.addr();
        // With a relay to meet at, keep IP addresses out of shared codes.
        if a.relay_urls().next().is_some() {
            a.addrs.retain(|a| a.is_relay());
        }
        Ok(a)
    }

    /// Our address, or our id alone if iroh cannot start (found by it).
    pub(super) async fn at(&self) -> EndpointAddr {
        let a = self.addr().await;
        a.unwrap_or_else(|_| EndpointAddr::new(self.device()))
    }

    pub async fn invite(&self, id: &Id) -> Result<String> {
        let sp = self.get(id)?;
        let mut inv =
            sp.inv.clone().ok_or_else(|| anyhow!("only hosted here"))?;
        let me = self.addr().await?;
        let (key, locked) = lock(&sp.book).key(&inv);
        ensure!(!locked, LOCKED);
        inv.key = key;
        inv.name = self.view(id, |s| s.name().to_string()).unwrap_or_default();
        let hosts = self.view(id, |s| s.hosts()).unwrap_or_default();
        let others = self.bootstrap(&sp).into_iter().map(EndpointAddr::new);
        let mut seen = HashSet::new();
        let all = std::iter::once(me).chain(hosts).chain(others);
        inv.peers = all.filter(|a| seen.insert(a.id)).take(6).collect();
        Ok(inv.encode())
    }

    pub async fn leave(self: &Arc<Self>, id: &Id) -> Result<()> {
        ensure!(*id != self.own, "not a space");
        let own = self.get(&self.own)?;
        // A DM deleted: their card goes too, or it would knock again.
        let p = self.view(id, |s| s.peer(&self.id())).flatten();
        let card =
            |s: &Space| s.inv.as_ref().filter(|i| i.card()).map(|i| i.owner);
        let cards =
            self.all().into_iter().filter(|s| p.is_some() && card(s) == p);
        let gone = cards.map(|s| s.id).chain([*id]).collect::<Vec<_>>();
        for c in gone {
            self.post(&own, Op::Left { space: c }).await?;
            self.close(&c);
        }
        // Its files stop being served from here.
        let mine = self.sharing().into_iter().filter(|f| f.1 == *id);
        for (h, ..) in mine.collect::<Vec<_>>() {
            let _ = self.unshare(&h).await;
        }
        Ok(())
    }

    /// This identity is lost (a stolen phone): say so in every space, for
    /// good. Admins rekey it out; nothing it signs later counts.
    pub async fn burn(self: &Arc<Self>) -> Result<()> {
        ensure!(self.serve.is_none(), "storage nodes have no identity");
        for sp in self.all().into_iter().filter(|s| s.inv.is_some()) {
            if let Err(e) = self.send(&sp, Op::Burn).await {
                warn!("retiring in {:?}: {e:#}", sp.id);
            }
        }
        Ok(())
    }

    /// This identity's devices, newest first: (device, last wrote).
    pub fn devices(&self) -> Vec<(EndpointId, u64)> {
        let me = self.id();
        self.view(&self.own, |s| s.devices_of(&me)).unwrap_or_default()
    }

    /// When a storage node or peer last synced with us.
    pub fn synced(&self, d: &EndpointId) -> Option<u64> {
        lock(&self.synced).get(d).copied()
    }

    pub async fn ban(
        self: &Arc<Self>,
        id: &Id,
        user: &Id,
        on: bool,
    ) -> Result<()> {
        let role = if on { Role::Banned } else { Role::Member };
        self.post(&self.get(id)?, Op::Role { user: *user, role }).await?;
        self.secure(id).await
    }

    /// Old invites stop working; history stays readable through `Back`.
    pub async fn rekey(self: &Arc<Self>, id: &Id) -> Result<()> {
        let sp = self.get(id)?;
        let inv = sp.inv.as_ref().ok_or_else(|| anyhow!("only hosted here"))?;
        let (new, old, to) = {
            let b = lock(&sp.book);
            ensure!(b.st.role(&self.id()) == Role::Admin, "only admins rekey");
            let (new, old): (Key, Key) = (rand(), b.key(inv).0);
            let to =
                b.st.members()
                    .into_iter()
                    .filter_map(|m| Some((m.id, b.st.kem(&m.id)?.to_vec())));
            (new, old, to.collect::<Vec<_>>())
        };
        // Past what a rekey carries (`op::fits`, with its commitment) it
        // would be refused everywhere, its own too: said now.
        ensure!(to.len() < 3000, "too many members to rekey (2999 at most)");
        // Sealed outside the book's lock: thousands of X-Wing seals.
        let aad = rekey_aad(id, &tag(&new));
        let mut boxes: Vec<_> = (to.iter())
            .filter_map(|(u, k)| Some((*u, kem_seal(k, &aad, &new)?)))
            .collect();
        boxes.push((crate::state::COMMIT, crate::state::commit(&new)));
        lock(&sp.book).keys.insert(tag(&new), new);
        self.post(&sp, Op::Rekey { epoch: tag(&new), boxes }).await?;
        self.post(&sp, Op::Back { key: old }).await?;
        Ok(())
    }

    /// The DM with `user`, announced to them in `via`: the one both are in (the lowest id, so
    /// both sides agree), else one of ours they never took, knocked
    /// again, else a new one.
    pub async fn dm(self: &Arc<Self>, via: &Id, user: &Id) -> Result<Id> {
        ensure!(*user != self.id(), "that is you");
        let me = self.id();
        if let Some(sp) = self.dm_of(user) {
            self.hide(&sp.id, false);
            return Ok(sp.id);
        }
        let them = self.view(via, |s| {
            (s.kem(user).map(<[u8]>::to_vec), s.display_name(user))
        });
        let (Some(pk), name) = them.context("not in that space")? else {
            bail!("they have not shared a key yet");
        };
        // Ours, never taken (their phone asleep, or theirs crossed it).
        let ours =
            |sp: &Arc<Space>| sp.inv.as_ref().is_some_and(|i| i.owner == me);
        let lonely = self.dms_with(user).into_iter().map(|x| x.0).find(ours);
        let (id, mut inv) = match lonely {
            Some(sp) => (sp.id, sp.inv.clone().expect("ours")),
            None => {
                let inv = Invite::new(me, &name, Some(*user));
                let (epoch, id) =
                    (tag(&inv.key), self.enter(inv.clone()).await?);
                let rekey = Op::Rekey { epoch, boxes: vec![] };
                self.post(&self.get(&id)?, rekey).await?;
                (id, inv)
            }
        };
        self.hide(&id, false);
        inv.peers = vec![self.addr().await?];
        let sealed = kem_seal(&pk, &via.0, &postcard::to_allocvec(&inv)?)
            .context("bad key")?;
        let knock = self.send(&self.get(via)?, Op::Knock { sealed }).await?;
        // Their sleeping phones: woken to take it, and what follows.
        self.wake_who(via, Some(user), Some(knock.1));
        Ok(id)
    }

    /// DMs others made for us that they never came to: knocks taken.
    pub(super) fn waiting(&self) -> usize {
        let me = self.id();
        let theirs = |sp: &Arc<Space>| {
            let i =
                sp.inv.as_ref().filter(|i| i.dm == Some(me) && !i.card())?;
            self.view(&sp.id, |s| s.profile(&i.owner).is_none())
        };
        self.all().iter().filter(|sp| theirs(sp) == Some(true)).count()
    }

    /// Our DMs with `user`, each with whether they are in it.
    fn dms_with(&self, user: &Id) -> Vec<(Arc<Space>, bool)> {
        let me = self.id();
        let with = |sp: Arc<Space>| {
            let (dm, here) = self.view(&sp.id, |s| {
                (s.peer(&me) == Some(*user), s.profile(user).is_some())
            })?;
            dm.then_some((sp, here))
        };
        let card = |sp: &Arc<Space>| sp.inv.as_ref().is_some_and(Invite::card);
        self.all().into_iter().filter(|s| !card(s)).filter_map(with).collect()
    }

    /// Of several DMs with one person, whether this is one to list: the
    /// one both are in (as both pick), else ours with the latest post; a
    /// duplicate only if something was said in it.
    pub fn listed(&self, id: &Id) -> bool {
        use crate::state::DM;
        let me = self.id();
        let Some(peer) = self.view(id, |s| s.peer(&me)).flatten() else {
            return true;
        };
        if self.view(id, |s| s.count(&DM) > 0) == Some(true) {
            return true;
        }
        let latest = |sp: &Arc<Space>| self.view(&sp.id, |s| s.latest(&DM));
        let best = self.dm_of(&peer).or_else(|| {
            let all = self.dms_with(&peer).into_iter().map(|x| x.0);
            all.max_by_key(|sp| (latest(sp), std::cmp::Reverse(sp.id)))
        });
        best.is_none_or(|b| b.id == *id)
    }

    pub fn card(&self, id: &Id) -> bool {
        self.get(id).is_ok_and(|s| s.inv.as_ref().is_some_and(Invite::card))
    }

    /// Ours to hand out: whoever joins it gets a DM with us.
    pub async fn contact(self: &Arc<Self>) -> Result<String> {
        let me = self.id();
        let ours =
            |s: &Arc<Space>| s.inv.as_ref().is_some_and(|i| i.owner == me);
        let has = self.all().into_iter().find(|s| ours(s) && self.card(&s.id));
        let id = match has {
            Some(sp) => sp.id,
            None => {
                let inv = Invite::new(me, &self.name(), Some(me));
                let (epoch, id) = (tag(&inv.key), self.enter(inv).await?);
                let rekey = Op::Rekey { epoch, boxes: vec![] };
                self.post(&self.get(&id)?, rekey).await?;
                id
            }
        };
        let card = self.invite(&id).await?;
        // Our lane rides the code, after the invite (a reader before 0.4.9
        // stops before it): whoever adds us wakes this device at once.
        Ok(match crate::push::lane_at(&self.dir) {
            Some(lane) => code("join", &(Invite::decode(&card)?, lane)),
            None => card,
        })
    }

    /// A card's owner, asleep or not, woken by the lane its code carried
    /// until their key is here and a DM with them stands: a dial on their
    /// card each time, and their card synced again (a phone's node runs a
    /// minute or so once woken).
    pub(super) async fn meet(
        self: Arc<Self>,
        card: Id,
        owner: Id,
        lane: pushir::Lane,
    ) {
        for wait in [0, 2, 3, 5, 10, 20, 30, 60, 120] {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            if !self.dms_with(&owner).is_empty() {
                return;
            }
            let Ok(sp) = self.get(&card) else { return };
            let Some(inv) = sp.inv.clone() else { return };
            let k = lock(&sp.book).key(&inv).0;
            let dial =
                [&[crate::push::DIAL][..], &ev_label(&card, &k)].concat();
            crate::push::send(&lane, &dial).await;
            for a in inv.peers {
                self.lookup.add_endpoint_info(a.clone());
                self.sync(&sp, a);
            }
        }
    }

    /// The DM both are in: of several, the lowest id, as both sides pick.
    fn dm_of(&self, user: &Id) -> Option<Arc<Space>> {
        let both = self.dms_with(user).into_iter().filter(|x| x.1);
        both.map(|x| x.0).min_by_key(|sp| sp.id)
    }

    pub async fn set_name(self: &Arc<Self>, name: &str) -> Result<()> {
        let name = named(name)?;
        *lock(&self.name) = name.into();
        for sp in self.all().into_iter().filter(|s| s.inv.is_some()) {
            let op = Op::Profile { name: name.into(), kem: self.me.pk() };
            self.post(&sp, op).await?;
        }
        Ok(())
    }
}
