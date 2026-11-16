//! Files: sealed, offered, fetched, kept by storage nodes.

use super::*;

impl Node {
    /// A preview of `url` (in our message `message`), from its site, now.
    pub async fn preview(
        self: &Arc<Self>,
        space: &Id,
        message: &Id,
        url: &str,
    ) -> Result<()> {
        let me = self.id();
        let ours = self.view(space, |s| s.message(message).map(|m| m.1));
        let m = ours.flatten().context("no such message")?;
        ensure!(m.author == me && m.text.contains(url), "not a link of ours");
        ensure!(open(&reqwest::Url::parse(url)?), "https links only");
        let (mut link, image) = crate::link::fetch(url).await?;
        if let Some((b, ext)) = image {
            link.image =
                self.attach(space, &format!("preview.{ext}"), &b).await.ok();
        }
        let target = *message;
        self.send(&self.get(space)?, Op::Preview { target, link }).await?;
        Ok(())
    }

    /// Whether our storage node `node` keeps `space` too.
    pub async fn store(
        self: &Arc<Self>,
        space: &Id,
        node: &EndpointId,
        on: bool,
    ) -> Result<()> {
        let paired = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        let (addr, _) = paired
            .into_iter()
            .find(|s| s.0.id == *node)
            .context("not one of your storage nodes")?;
        let (own, sp) = (self.get(&self.own)?, self.get(space)?);
        let (node, space) = (*node, *space);
        self.post(&own, Op::Store { node, space, on }).await?;
        let me = self.id();
        let admin = self
            .view(&space, |s| s.role(&me) == Role::Admin || s.dm == Some(me));
        match on {
            true => self.host(&sp, addr).await,
            false if admin == Some(true) => {
                let _ =
                    self.post(&sp, Op::Host { node: addr, on: false }).await;
            }
            false => {}
        }
        Ok(())
    }

    /// Off our list, on every device of ours. The node runs on: it is
    /// stopped where it runs (`chatir --uninstall`).
    pub async fn forget_storage(
        self: &Arc<Self>,
        node: &EndpointId,
    ) -> Result<()> {
        let all = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        let (addr, token) = all
            .into_iter()
            .find(|s| s.0.id == *node)
            .context("not one of your storage nodes")?;
        let own = self.get(&self.own)?;
        self.post(&own, Op::Storage { addr, token, on: false }).await?;
        Ok(())
    }

    pub fn stored(&self, space: &Id) -> Vec<(EndpointId, bool)> {
        let all = *space == self.own;
        let v = self.view(&self.own, |s| {
            let v = s.storage().into_iter().map(|(a, _)| a.id);
            v.map(|n| (n, all || s.keeps(&n, space))).collect()
        });
        v.unwrap_or_default()
    }

    pub(super) fn stores(&self, space: &Id) -> Vec<(EndpointAddr, [u8; 16])> {
        let all = *space == self.own;
        let v = self.view(&self.own, |s| {
            let mut v = s.storage();
            v.retain(|(a, _)| all || s.keeps(&a.id, space));
            v
        });
        v.unwrap_or_default()
    }

    pub(super) async fn host(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        addr: EndpointAddr,
    ) {
        self.lookup.add_endpoint_info(addr.clone());
        self.list(sp, &addr).await;
        let id = addr.id;
        self.sync(sp, addr);
        if let Some(tx) = sp.tx() {
            let _ = tx.join_peers(vec![id]).await;
        }
    }

    /// An admin names our storage node in the space, and how members have
    /// it keep their small files.
    pub(super) async fn list(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        addr: &EndpointAddr,
    ) {
        let me = self.id();
        let token = self.stores(&sp.id).into_iter().find(|s| s.0.id == addr.id);
        let token = token.map(|s| hold_token(&s.1, &sp.id));
        let (list, hold) = self
            .view(&sp.id, |s| {
                let admin = s.role(&me) == Role::Admin || s.dm == Some(me);
                let listed = s.hosts().iter().any(|h| h.id == addr.id);
                let held = token.is_none() || s.hold(&addr.id) == token;
                (admin && !listed, admin && !held && sp.id != self.own)
            })
            .unwrap_or_default();
        if list {
            let _ =
                self.post(sp, Op::Host { node: addr.clone(), on: true }).await;
        }
        if let Some(token) = token.filter(|_| hold) {
            let _ = self.post(sp, Op::Hold { node: addr.id, token }).await;
        }
    }

    /// Seal a file (in chunks, from disk) and keep its ciphertext here and
    /// on this user's storage nodes; send it in an `Op::Post`.
    pub async fn seal_file(
        self: &Arc<Self>,
        space: &Id,
        path: &Path,
        name: &str,
        password: Option<&str>,
    ) -> Result<File> {
        use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
        let size = std::fs::metadata(path)?.len();
        let key: Key = rand();
        let out = self.scratch("sealed")?;
        let (from, to) = (path.to_path_buf(), out.clone());
        tokio::task::spawn_blocking(move || {
            let r = std::io::BufReader::new(std::fs::File::open(from)?);
            let w = std::io::BufWriter::new(std::fs::File::create(to)?);
            crate::crypto::seal_stream(&key, r, w)
        })
        .await??;
        let (format, mode) = (Default::default(), ImportMode::TryReference);
        let opts = AddPathOptions { path: out, format, mode };
        let tag = self.files.add_path_with_opts(opts).with_tag().await?;
        let hash = Id(*tag.hash.as_bytes());
        self.keep(space, &hash, password).await?;
        if size <= PREVIEW as u64 {
            let (n, space) = (self.clone(), *space);
            tokio::spawn(async move { n.hold(&space, &hash).await });
        }
        let name = short(name);
        Ok(File { name, size, hash, key: Id(key) })
    }

    /// `seal_file`, from memory.
    pub async fn attach(
        self: &Arc<Self>,
        space: &Id,
        name: &str,
        data: &[u8],
    ) -> Result<File> {
        let tmp = self.scratch("in")?;
        std::fs::write(&tmp, data)?;
        let f = self.seal_file(space, &tmp, name, None).await;
        let _ = std::fs::remove_file(tmp);
        f
    }

    /// A fresh path in our sealed files' folder.
    fn scratch(&self, ext: &str) -> Result<PathBuf> {
        let dir = self.dir.join("sealed");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join(format!("{}.{ext}", Id(rand()))))
    }

    /// This user's storage nodes fetch `hash` from here. One that refuses
    /// it (past 5 GB, without its password) fails it; one offline does not.
    pub(super) async fn keep(
        self: &Arc<Self>,
        space: &Id,
        hash: &Id,
        password: Option<&str>,
    ) -> Result<()> {
        for (addr, token) in self.stores(space) {
            let ask = Ask::Keep(token, *hash, password.map(Into::into));
            let wait = Duration::from_secs(30);
            match tokio::time::timeout(wait, self.ask(addr, &ask)).await {
                Ok(Ok(no)) if !no.is_empty() => {
                    bail!("{}", String::from_utf8_lossy(&no))
                }
                Ok(Err(e)) => warn!("storage node: {e:#}"),
                _ => {}
            }
        }
        Ok(())
    }

    /// The space's storage nodes (not this user's: `keep`) fetch a small
    /// file from here, so it shows for everyone while we are away.
    pub(super) async fn hold(self: &Arc<Self>, space: &Id, hash: &Id) {
        let mine: Vec<_> =
            self.stores(space).into_iter().map(|s| s.0.id).collect();
        let hosts = self.view(space, |s| s.holds()).unwrap_or_default();
        for (addr, token) in
            hosts.into_iter().filter(|h| !mine.contains(&h.0.id))
        {
            {
                let mut a = lock(&self.asked);
                if a.len() >= 1024 {
                    a.pop_front();
                }
                a.push_back((addr.id, *hash));
            }
            let ask = Ask::Hold(*space, token, *hash);
            let ask = self.ask(addr, &ask);
            match tokio::time::timeout(Duration::from_secs(30), ask).await {
                Ok(Ok(no)) if !no.is_empty() => {
                    warn!("storage node: {}", String::from_utf8_lossy(&no))
                }
                Ok(Err(e)) => warn!("storage node: {e:#}"),
                _ => {}
            }
        }
    }

    /// One of this user's storage nodes.
    pub(super) fn mine(&self, d: &EndpointId) -> bool {
        let s =
            self.view(&self.own, |s| s.storage().iter().any(|a| a.0.id == *d));
        s == Some(true)
    }

    /// Served from here to members while online: any size, no copy, unsealed
    /// (QUIC encrypts), checked chunk by chunk. `name`: else the path's.
    /// Served to `space`'s members only, until we stop sharing it.
    pub async fn offer(
        &self,
        space: &Id,
        path: &Path,
        name: Option<&str>,
    ) -> Result<File> {
        use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
        self.get(space)?;
        let files = &self.files;
        let path = std::path::absolute(path)?;
        let size = std::fs::metadata(&path)?.len();
        // Too big to hash first: device to device, as it is.
        if size >= crate::send::BIG {
            return self.offer_direct(space, &path, name);
        }
        let (format, mode) = (Default::default(), ImportMode::TryReference);
        let opts = AddPathOptions { path: path.clone(), format, mode };
        let tag = files.add_path_with_opts(opts).with_tag().await?;
        let base = path.file_name().and_then(|n| n.to_str());
        let name = short(name.or(base).unwrap_or("file"));
        let hash = Id(*tag.hash.as_bytes());
        let space = Some(*space);
        let (tag, n) = (tag.name.0.to_vec(), name.clone());
        lock(&self.shared).insert(hash, Shared { space, tag, name: n, size });
        self.save_shared();
        Ok(File { name, size, hash, key: crate::op::OFFERED })
    }

    /// Passed on from here too, to that space's members alone.
    pub(super) fn passed(&self, space: &Id, f: &File) {
        let mut s = lock(&self.shared);
        if s.contains_key(&f.hash) {
            return;
        }
        let (space, name, size) = (Some(*space), f.name.clone(), f.size);
        s.insert(f.hash, Shared { space, tag: vec![], name, size });
        drop(s);
        self.save_shared();
    }

    fn save_shared(&self) {
        self.stash("shared", &*lock(&self.shared));
    }

    /// What this device serves, as (hash, space, name, size).
    pub fn sharing(&self) -> Vec<(Id, Id, String, u64)> {
        let s = lock(&self.shared);
        let on = s.iter().filter_map(|(h, f)| Some((*h, f.space?, f)));
        let on = on.map(|(h, sp, f)| (h, sp, f.name.clone(), f.size));
        on.chain(self.offered_direct()).collect()
    }

    /// Stop serving an offered file (or one sent device to device); its
    /// members can no longer fetch it.
    pub async fn unshare(self: &Arc<Self>, h: &Id) -> Result<()> {
        if self.unserve(h) {
            return Ok(());
        }
        let tag = {
            let mut s = lock(&self.shared);
            let f = s.get_mut(h).context("not shared from here")?;
            f.space = None;
            std::mem::take(&mut f.tag)
        };
        self.save_shared();
        if !tag.is_empty() {
            self.files.tags().delete(tag).await?;
        }
        Ok(())
    }

    /// A blob's real size, proven by its hash, and who gave it: all asked
    /// at once, the first answer taken (offline devices cost no turn).
    pub(super) async fn verified(
        &self,
        peers: &[EndpointId],
        h: &Id,
    ) -> Option<(u64, EndpointId)> {
        let hash = iroh_blobs::Hash::from_bytes(h.0);
        let mut asks: n0_future::FuturesUnordered<_> = peers
            .iter()
            .map(|p| async move {
                let size = async {
                    let c = self.connect(*p, iroh_blobs::ALPN).await.ok()?;
                    let got =
                        iroh_blobs::get::request::get_verified_size(&c, &hash);
                    Some((got.await.ok()?.0, *p))
                };
                let wait = Duration::from_secs(8);
                tokio::time::timeout(wait, size).await.ok().flatten()
            })
            .collect();
        while let Some(r) = asks.next().await {
            if r.is_some() {
                return r;
            }
        }
        None
    }

    /// Blob `h`, `len` bytes, from whoever has it (sender, members, storage
    /// nodes), resumed; served on.
    pub(super) async fn pull(
        self: &Arc<Self>,
        space: &Id,
        h: &Id,
        len: u64,
    ) -> Result<&FsStore> {
        let hash = iroh_blobs::Hash::from_bytes(h.0);
        if !self.files.has(hash).await? {
            let sp = self.get(space)?;
            // Storage nodes and those online first, then everyone known.
            let mut peers: Vec<EndpointId> =
                self.stores(space).into_iter().map(|s| s.0.id).collect();
            peers.extend(sp.nbrs());
            peers.extend(self.bootstrap(&sp));
            let mut seen = HashSet::new();
            peers.retain(|p| seen.insert(*p));
            self.prove(space, &peers).await;
            // Real size first: a post may claim 1 byte.
            let gone = "nobody who has it is reachable now: it saves once \
                its sender, or a storage node keeping it, is online";
            let (size, from) = self.verified(&peers, h).await.context(gone)?;
            ensure!(size == len, "not the file its post said");
            // Who answered first, then the rest; a link may drop mid-way
            // (relay losses): asked again, it resumes.
            if len > 1 << 28 && !self.direct(from).await {
                self.emit(Event::Relayed { size: len });
            }
            peers.retain(|p| *p != from);
            peers.insert(0, from);
            peers.iter().for_each(|p| self.knock(*p));
            let ep = self.up().await?;
            let known = peers.iter().filter(|p| self.known(&sp, p));
            let pull = self.downloader(&ep, known.copied().collect());
            pull.download(hash, peers.repeat(3)).await.ok().context(gone)?;
        }
        Ok(&self.files)
    }

    /// Who we are, proven to `peers` at once (a sync with each, 5 s at
    /// most): files go to members only.
    pub(crate) async fn prove(
        self: &Arc<Self>,
        space: &Id,
        peers: &[EndpointId],
    ) {
        let Ok(sp) = self.get(space) else { return };
        let proofs = peers.iter().take(8).map(|p| {
            let s = self.clone().sync_with(sp.clone(), (*p).into());
            tokio::time::timeout(Duration::from_secs(5), s)
        });
        n0_future::join_all(proofs).await;
    }

    /// Proven a member's device here (it signed for them) or one of the
    /// space's storage nodes.
    pub(super) fn known(&self, sp: &Space, d: &EndpointId) -> bool {
        let u = lock(&self.verified).get(d).map(|v| v.0);
        let ours = self.stores(&sp.id).iter().any(|s| s.0.id == *d);
        let st = &lock(&sp.book).st;
        ours || st.hosts().iter().any(|a| a.id == *d)
            || u.is_some_and(|u| st.profile(&u).is_some() && !st.out(&u))
    }

    /// iroh-blobs' downloader: its links to `known` (proven) peers may fill
    /// irnet's wide window; to anyone else, a stranger's 8 MiB.
    pub(super) fn downloader(
        &self,
        ep: &Endpoint,
        known: Vec<EndpointId>,
    ) -> Downloader {
        use iroh_blobs::util::connection_pool::Options;
        let wide = Options::default().with_on_connected(move |_, c| {
            if known.contains(&c.remote_id()) {
                irnet::wide(&c);
            }
            std::future::ready(Ok(()))
        });
        Downloader::builder(&self.files, ep).pool_options(wide).build()
    }

    /// A copy (a reflink where it can): referenced instead, what the user
    /// later writes there is what members asking for its hash would get.
    async fn export(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        to: &Path,
    ) -> Result<()> {
        let files = self.pull(space, &f.hash, f.size).await?;
        self.passed(space, f);
        std::fs::remove_file(to)?;
        files.export(iroh_blobs::Hash::from_bytes(f.hash.0), to).await?;
        Ok(())
    }

    pub(super) fn member_device(&self, d: &EndpointId) -> bool {
        let has =
            |id: &Id| self.view(id, |s| s.devices(usize::MAX).contains(d));
        self.spaces().iter().any(|id| has(id) == Some(true))
    }

    /// `d` is ours, or a device of a member of `sp`: named in their ops,
    /// or proven theirs (`may_get`). Just started, a space opens after we
    /// serve: a moment for it.
    pub(crate) async fn member_of(&self, sp: &Id, d: &EndpointId) -> bool {
        for _ in 0..50 {
            if self.get(sp).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let u = lock(&self.verified).get(d).map(|v| v.0);
        let member = |s: &State| {
            s.owner_of(d).is_some()
                || u.is_some_and(|u| s.profile(&u).is_some() && !s.out(&u))
        };
        self.devices().iter().any(|x| x.0 == *d)
            || self.view(sp, member) == Some(true)
    }

    /// An attachment's plaintext, in memory (a preview).
    pub async fn file(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
    ) -> Result<Vec<u8>> {
        ensure!(!f.direct(), "sent device to device: save it");
        ensure!(f.size <= PREVIEW as u64, "too big to open here: save it");
        let hash = iroh_blobs::Hash::from_bytes(f.hash.0);
        if f.offered() {
            let files = self.pull(space, &f.hash, f.size).await?;
            self.passed(space, f);
            return Ok(files.get_bytes(hash).await?.to_vec());
        }
        let len = crate::crypto::sealed_len(f.size).context("no such size")?;
        let files = self.pull(space, &f.hash, len).await?;
        let (n, h) = (self.clone(), (*space, f.hash));
        tokio::spawn(async move {
            let _ = n.keep(&h.0, &h.1, None).await;
            n.hold(&h.0, &h.1).await;
        });
        let ct = files.get_bytes(hash).await?.to_vec();
        let mut pt = vec![];
        crate::crypto::open_stream(&f.key.0, &ct[..], &mut pt)?;
        Ok(pt)
    }

    /// A sealed file too big to preview, opened straight into `to`.
    async fn unseal(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        to: &Path,
    ) -> Result<()> {
        let len = crate::crypto::sealed_len(f.size).context("no such size")?;
        let files = self.pull(space, &f.hash, len).await?;
        let tmp = self.scratch("out")?;
        files.export(iroh_blobs::Hash::from_bytes(f.hash.0), &tmp).await?;
        let (key, to, t) = (f.key.0, to.to_path_buf(), tmp.clone());
        let r = tokio::task::spawn_blocking(move || {
            let r = std::io::BufReader::new(std::fs::File::open(t)?);
            let w = std::io::BufWriter::new(std::fs::File::create(to)?);
            crate::crypto::open_stream(&key, r, w)
        })
        .await;
        let _ = std::fs::remove_file(tmp);
        Ok(r??)
    }

    /// An attachment written into `dir`, never over anything already there.
    pub async fn save(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        dir: &Path,
    ) -> Result<PathBuf> {
        use std::io::{ErrorKind::AlreadyExists, Write};
        if f.direct() {
            return self.save_direct(space, f, dir).await;
        }
        let small = !f.offered() && f.size <= PREVIEW as u64;
        let data = if small { self.file(space, f).await? } else { vec![] };
        std::fs::create_dir_all(dir)?;
        for p in names(dir, &f.clone().safe().name) {
            match std::fs::File::create_new(&p) {
                Ok(mut out) => {
                    out.write_all(&data)?;
                    if !small {
                        drop(out);
                        let got = match f.offered() {
                            true => self.export(space, f, &p).await,
                            false => self.unseal(space, f, &p).await,
                        };
                        got.inspect_err(|_| drop(std::fs::remove_file(&p)))?;
                    }
                    mark(&p);
                    return Ok(p);
                }
                Err(e) if e.kind() == AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }

    pub(super) async fn ask(
        &self,
        peer: EndpointAddr,
        a: &Ask,
    ) -> Result<Vec<u8>> {
        let c = self.connect(peer, BLOB).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let b = sync::rest(&mut rx, PREVIEW + 1024).await?;
        c.close(0u32.into(), b"done");
        Ok(b)
    }

    /// Blob `h`, fetched from `from` in the background, kept. Charged to
    /// a space's files when asked (`charged`), so two asks at once never
    /// both fit; none of it come, its share is given back.
    pub(super) fn fetch(
        &self,
        h: &Id,
        from: EndpointId,
        charged: Option<(Id, u64)>,
    ) {
        let (files, n) = (self.files.clone(), self.this.get().cloned());
        let hash = iroh_blobs::Hash::from_bytes(h.0);
        tokio::spawn(async move {
            let Some(n) = n.and_then(|n| n.upgrade()) else { return };
            n.knock(from);
            // From an owner (its token) or a device they vouched for.
            let got = match n.up().await {
                Ok(ep) => n.downloader(&ep, vec![from]),
                Err(e) => return warn!("keeping a file: {e:#}"),
            };
            let got = got.download(hash, vec![from; 3]);
            if let Err(e) = got.await {
                warn!("keeping a file: {e:#}");
                let none = files.observe(hash).await;
                if let Some((sp, size)) = charged
                    && none.is_ok_and(|b| b.ranges.is_empty())
                {
                    let mut b = lock(&n.budget);
                    b.entry((sp, true)).and_modify(|u| u.1 -= size.min(u.1));
                }
            }
        });
    }

    /// A device the space's owner vouched for (not banned since), or soon
    /// will: a new member's first photo may come first.
    pub(super) async fn vouched(&self, sp: &Id, d: &EndpointId) -> bool {
        for _ in 0..50 {
            if self.get(sp).is_ok_and(|s| self.takes(&s, d)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }

    /// What a storage node takes for a space each day: 4 GB of ops, and
    /// apart 4 GB of members' small files (25 MB each), so no member fills
    /// its disk, nor crowds out the ops with files that never come.
    pub(super) fn budget(
        &self,
        sp: &Id,
        size: u64,
        file: bool,
    ) -> Result<(), &'static str> {
        if Some(size) > crate::crypto::sealed_len(PREVIEW as u64) {
            return Err("over 25 MB: the sender's own storage node keeps it");
        }
        let (mut b, day) = (lock(&self.budget), now() / DAY);
        let used = b.entry((*sp, file)).or_insert((day, 0));
        if used.0 != day {
            *used = (day, 0);
        }
        if used.1 + size > 4 << 30 {
            return Err("this space's files for today are kept: try tomorrow");
        }
        used.1 += size;
        Ok(())
    }

    pub(super) async fn give(&self, c: &Connection) -> Result<()> {
        let ((mut tx, mut rx), from) = (c.accept_bi().await?, c.remote_id());
        // A file's worth is read only after the head (and token), by storage.
        let mut head = [0u8; 17];
        rx.read_exact(&mut head).await?;
        // The ask's kind, then its token: read before anything longer.
        let max = match (head[0], self.serve) {
            (0, Some(t)) if same(&head[1..], &t) => 1024,
            (0, _) => bail!("not a storage node of yours"),
            (1, Some(_)) => 128,
            _ => bail!("not a storage node"),
        };
        let rest = sync::rest(&mut rx, max).await?;
        match postcard::from_bytes(&[&head[..], &rest].concat())? {
            Ask::Keep(t, h, pw) if self.serve.is_some_and(|s| same(&s, &t)) => {
                let size = self.verified(&[from], &h).await;
                let size = size.context("not reachable, or not there")?.0;
                let same = |a: &str, b: &str| {
                    blake3::hash(a.as_bytes()) == blake3::hash(b.as_bytes())
                };
                let open = self.big.as_deref().zip(pw.as_deref());
                if Some(size) > crate::crypto::sealed_len(limit())
                    && !open.is_some_and(|(a, b)| same(a, b))
                {
                    let no = "over 5 GB: storage nodes keep it only with their password";
                    tx.write_all(no.as_bytes()).await?;
                } else {
                    self.fetch(&h, from, None);
                }
            }
            Ask::Hold(sp, t, h)
                if self
                    .serve
                    .is_some_and(|s| same(&hold_token(&s, &sp), &t)) =>
            {
                ensure!(self.vouched(&sp, &from).await, "not vouched for");
                let hash = iroh_blobs::Hash::from_bytes(h.0);
                if !self.files.has(hash).await? {
                    let size = self.verified(&[from], &h).await;
                    let size = size.context("not reachable, or not there")?.0;
                    match self.budget(&sp, size, true) {
                        Ok(()) => self.fetch(&h, from, Some((sp, size))),
                        Err(no) => tx.write_all(no.as_bytes()).await?,
                    }
                }
            }
            Ask::Keep(..) => bail!("not a storage node of yours"),
            Ask::Hold(..) => bail!("not a space this storage node keeps"),
        }
        tx.finish()?;
        let _ = tokio::time::timeout(Duration::from_secs(10), c.closed()).await;
        Ok(())
    }

    /// May `d` have blob `h`? Sealed ones: any member's proven device, or
    /// a storage node we asked to hold it; offered ones: only members of
    /// the space they went to.
    pub(super) fn may_get(&self, d: &EndpointId, h: &Id) -> bool {
        let asked = lock(&self.asked).contains(&(*d, *h));
        if self.serve.is_some() || self.mine(d) || asked {
            return true;
        }
        let Some(u) = lock(&self.verified).get(d).map(|v| v.0) else {
            return false;
        };
        let member = |id: &Id| {
            self.view(id, |s| s.profile(&u).is_some() && !s.out(&u))
                == Some(true)
        };
        let shared = lock(&self.shared).get(h).map(|s| s.space);
        match shared {
            Some(sp) => sp.is_some_and(|sp| member(&sp)),
            None => self.spaces().iter().any(member),
        }
    }
}
