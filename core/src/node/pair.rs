//! Our own devices: link and storage codes, labels, texts.

use super::*;

/// A text's MAC under our own space's key: only our devices make it.
fn text_mac(k: &Key, (from, ts, back, name, text): &Text) -> Key {
    let ts = ts.to_le_bytes();
    let parts =
        [&k[..], from.as_bytes(), &ts, back.as_bytes(), name.as_bytes()];
    crate::crypto::kdf(
        "device text",
        &[&parts[..], &[text.as_bytes()]].concat(),
    )
}

/// Its ack: only a device of ours that opened it makes it.
pub(super) fn text_ack(k: &Key, mac: &Key) -> Key {
    crate::crypto::kdf("device text ack", &[k, mac])
}

/// A text frame's body, if one of our devices made it under `k` (not
/// dated ahead past the skew).
pub(super) fn open_text(k: &Key, rest: &[u8]) -> Option<(Text, Key)> {
    let (t, mac): (Text, Key) = postcard::from_bytes(rest).ok()?;
    (t.1 <= now() + SKEW && same(&text_mac(k, &t), &mac)).then_some((t, mac))
}

/// Whether this text was taken already: by lane (`mac`, its own, with its
/// time) a retry whose ack was lost, for a month; and within `AGAIN` the
/// same text by its other way (lane or direct, which has no mac). The same
/// words sent twice are two texts. Noted either way; on disk, under the
/// shared lock: a sleeping device's ring and its node share it.
pub(super) fn taken_text(
    dir: &Path,
    from: &EndpointId,
    text: &str,
    mac: Option<&Key>,
) -> bool {
    let f = dir.join("handed");
    let words =
        *blake3::hash(&[from.as_bytes(), text.as_bytes()].concat()).as_bytes();
    let (at, again) = (now(), AGAIN.as_millis() as u64);
    shared(dir, || {
        // (key, when, by lane): a mac, or the words with the way they came.
        let mut seen: Vec<(Key, u64, bool)> = store::load(&f);
        seen.retain(|x| at.saturating_sub(x.1) < MONTH_MS);
        seen.drain(..seen.len().saturating_sub(HANDED));
        let lane = mac.is_some();
        let other_way = |x: &&(Key, u64, bool)| {
            x.0 == words && x.2 != lane && at.saturating_sub(x.1) < again
        };
        let taken = mac.is_some_and(|m| seen.iter().any(|x| x.0 == *m))
            || seen.iter().any(|x| other_way(&x));
        if !taken {
            seen.extend(mac.map(|m| (*m, at, true)));
            seen.push((words, at, lane));
            let _ = store::save(&f, &seen);
        }
        taken
    })
}

/// Texts remembered at most (`taken_text`), and for how long.
const HANDED: usize = 2048;
const MONTH_MS: u64 = 30 * 24 * 3600 * 1000;

impl Node {
    /// Hand `text` to one of our own devices: one link straight to it (no
    /// space, no sync, no other device), so fewer packets; it must be up.
    /// True once it took it; false if it went by its lane and that device
    /// is away (the relay keeps it: it shows there as it is back).
    pub async fn hand(&self, device: EndpointId, text: &str) -> Result<bool> {
        ensure!(self.devices().iter().any(|d| d.0 == device), "not ours");
        ensure!(text.len() <= MAX_TEXT, "too long");
        // By its lane: shown there at once, asleep or not, and acked back;
        // and directly too (a device before 0.4.9 hears only that). Either
        // way it is taken once (`taken_text`).
        let lane = self.text_by_lane(device, text);
        let ack = lane.as_ref().map(|l| l.0);
        let acked = async move {
            if let Some((_, rx)) = lane
                && rx.await.is_ok()
            {
                return;
            }
            std::future::pending::<()>().await
        };
        let r = tokio::select! {
            () = acked => Ok(true),
            r = self.hand_direct(device, text, ack.is_some()) => match r {
                Ok(()) => Ok(true),
                Err(_) if ack.is_some() => Ok(false),
                Err(e) => Err(e),
            },
        };
        ack.map(|a| lock(&self.told).remove(&a));
        r
    }

    /// `text` on `device`'s lane, to its mailbox (the relay keeps it while
    /// it is away), where it opens with no node running; again until acked,
    /// as an event is. None if it cannot go so: no lane, too long a text.
    pub(crate) fn text_by_lane(
        &self,
        device: EndpointId,
        text: &str,
    ) -> Option<(Key, tokio::sync::oneshot::Receiver<()>)> {
        let own = self.get(&self.own).ok()?;
        let k = lock(&own.book).key(own.inv.as_ref()?).0;
        let all = self.all().into_iter();
        let to = all
            .into_iter()
            .find_map(|sp| self.lanes(&sp.id).remove(&device))?;
        let back = crate::push::lane_at(&self.dir).unwrap_or_default();
        let name = self.label(&self.device()).unwrap_or_default();
        let t: Text = (self.device(), now(), back, name, text.into());
        let mac = text_mac(&k, &t);
        let body = postcard::to_allocvec(&(&t, mac)).ok()?;
        let label = ev_label(&self.own, &k);
        let f = [&[crate::push::TEXT][..], &label, &body].concat();
        if f.len() > pushir::MAX {
            return None;
        }
        let a = text_ack(&k, &mac);
        // On disk first: it goes again after a restart until acked.
        let url = to.to_string();
        let keep = Out::new(Some(a), self.own, device, t.1, url, f.clone());
        self.keep_out(vec![keep]);
        let (tx, rx) = tokio::sync::oneshot::channel();
        lock(&self.told).insert(a, tx);
        let n = self.this.get().cloned().unwrap_or_default();
        tokio::spawn(async move {
            for wait in [0, 3, 10, 45] {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                let Some(n) = n.upgrade() else { return };
                if !lock(&n.told).contains_key(&a) {
                    return;
                }
                drop(n);
                crate::push::send(&to, &f).await;
            }
        });
        Some((a, rx))
    }

    /// `text` straight to `device` (QUIC proves us), waking it through its
    /// lane if it is up in no space we share: at once, or (the text went by
    /// its lane too, which wakes nothing) once that had 5 s to be acked.
    async fn hand_direct(
        &self,
        device: EndpointId,
        text: &str,
        lane: bool,
    ) -> Result<()> {
        let up = self.all().iter().any(|sp| self.present(sp, &device));
        let wake = async {
            if lane {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if !up {
                self.rouse(device);
            }
            std::future::pending::<()>().await
        };
        tokio::select! {
            r = self.hand_tries(device, text, up) => r,
            () = wake => unreachable!(),
        }
    }

    async fn hand_tries(
        &self,
        device: EndpointId,
        text: &str,
        up: bool,
    ) -> Result<()> {
        let once = || async {
            let c = self.connect(device, HAND).await?;
            let (mut tx, mut rx) = c.open_bi().await?;
            tx.write_all(text.as_bytes()).await?;
            tx.finish()?;
            let ok = rx.read_to_end(1).await? == [1];
            c.close(0u32.into(), b"");
            anyhow::Ok(ok)
        };
        // Not taken (asleep, or its ack lost: then taken once, `handed`):
        // woken through its lane if not yet, and tried again for a minute.
        let (each, end) = (Duration::from_secs(15), Instant::now() + WOKEN);
        let mut woken = !up;
        loop {
            match tokio::time::timeout(each, once()).await {
                Ok(Ok(ok)) => {
                    ensure!(ok, "it did not take it");
                    return Ok(());
                }
                _ if Instant::now() < end => {
                    if !std::mem::replace(&mut woken, true) {
                        self.rouse(device);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                _ => bail!("that device did not wake: off, or no push lane"),
            }
        }
    }

    /// From one of our own devices only (QUIC proves which device).
    pub(super) async fn handed(&self, c: &Connection) -> Result<()> {
        let from = c.remote_id();
        let (mut tx, mut rx) = c.accept_bi().await?;
        let ours = self.devices().iter().any(|d| d.0 == from);
        let text = sync::rest(&mut rx, MAX_TEXT).await?;
        tx.write_all(&[ours as u8]).await?;
        tx.finish()?;
        if ours {
            let text = String::from_utf8(text)?;
            if !taken_text(&self.dir, &from, &text, None) {
                self.show_text(from, text);
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), c.closed()).await;
        Ok(())
    }

    /// A text from one of our devices, taken: a notice, as a message is
    /// (out of sight, or woken for it), and to the page.
    pub(super) fn show_text(&self, from: EndpointId, text: String) {
        {
            let name = self.label(&from);
            let who =
                name.clone().unwrap_or_else(|| from.fmt_short().to_string());
            let n = Notice::new(
                Id(*from.as_bytes()),
                Ch::default(),
                who,
                text.clone(),
                false,
                false,
            );
            lock(&self.texts).push(n);
            self.emit(Event::Handed { from, name, text });
        }
    }

    /// A short code (one minute, one try) for `code` (any code, or a
    /// device's key), else an invite to `space`, else our link code.
    pub async fn short_code(
        &self,
        space: Option<Id>,
        code: Option<String>,
        password: Option<String>,
    ) -> Result<String> {
        let long = match (code, space) {
            (Some(c), _) => {
                ensure!(crate::code::kind(&c).is_some(), "not a code");
                c
            }
            (None, Some(id)) => self.invite(&id).await?,
            (None, None) => self.link_code().await,
        };
        let (code, end) = crate::code::give(&self.net, long, password).await?;
        // Spent by someone who reached it without proving it: said, as
        // whoever was meant to take it now cannot.
        let n = self.this.get().cloned().unwrap_or_default();
        tokio::spawn(async move {
            if let (Ok(pairir::End::Spent), Some(n)) = (end.await, n.upgrade())
            {
                n.emit(Event::Spent);
            }
        });
        Ok(code)
    }

    /// The invite or link code a short code stands for.
    pub async fn take_code(
        &self,
        code: &str,
        password: Option<&str>,
    ) -> Result<String> {
        crate::code::take(&self.net, code, password).await
    }

    /// Everything another device needs to become you: keep it secret.
    pub async fn link_code(&self) -> String {
        let mut peers = vec![self.at().await];
        peers.extend(self.view(&self.own, |s| s.hosts()).unwrap_or_default());
        code("link", &(self.me.seed, peers))
    }

    pub async fn storage_code(&self) -> Option<String> {
        let token = self.serve?;
        Some(code("host", &(self.at().await, token, storage_name())))
    }

    pub async fn add_storage(self: &Arc<Self>, code: &str) -> Result<()> {
        type Named = (EndpointAddr, [u8; 16], String);
        let (addr, token, name) = uncode::<Named>("host", code)?;
        let name = Some(name);
        let own = self.get(&self.own)?;
        let op = Op::Storage { addr: addr.clone(), token, on: true };
        self.post(&own, op).await?;
        let named = self.label(&addr.id).is_some();
        if let Some(name) = name.filter(|n| !named && !n.trim().is_empty()) {
            let device = addr.id;
            self.post(&own, Op::Label { device, name }).await?;
        }
        self.host(&own, addr).await;
        Ok(())
    }

    /// What we call one of our devices or storage nodes.
    pub fn label(&self, d: &EndpointId) -> Option<String> {
        self.view(&self.own, |s| s.label(d).map(String::from)).flatten()
    }

    /// Name one of our devices or storage nodes, on every device of ours.
    pub async fn name_device(
        self: &Arc<Self>,
        device: EndpointId,
        name: &str,
    ) -> Result<()> {
        let name = named(name)?.to_string();
        self.post(&self.get(&self.own)?, Op::Label { device, name }).await?;
        Ok(())
    }
}
