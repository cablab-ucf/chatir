//! Our push frames not yet acked, on disk (`DIR/outbox`): a message, a
//! knock, a lane moved or a device text outlives this process (killed,
//! napped, offline) and goes again, less often each time, until the
//! device it is for acks it, syncs with us, or a month passes (said).

use super::*;

/// Unacked this long, a frame is given up (said in the log).
const MONTH: u64 = 30 * 24 * 3600 * 1000;
/// First kept retry: `wake_who`'s own cover the first minute.
const FIRST: u64 = 120_000;
/// Then between tries, in seconds; the last repeats.
const BACKOFF: [u64; 4] = [600, 3600, 6 * 3600, 24 * 3600];
/// Frames kept at most (devices gone for good): the oldest go first.
const MOST: usize = 4096;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(super) struct Out {
    /// The ack it awaits; zero for a bare frame (catch up), never acked.
    pub ack: Key,
    pub space: Id,
    pub device: [u8; 32],
    /// The op's time (a direct link's ack names it), or the text's.
    pub ts: u64,
    /// Its lane as sent; looked up again as it goes (it may have moved).
    pub url: String,
    pub frame: Vec<u8>,
    pub first: u64,
    pub tries: u8,
    pub next: u64,
    /// Its last try reached a relay (this run; a new run tries them all).
    #[serde(skip)]
    pub sent: bool,
}

impl Out {
    pub(super) fn new(
        ack: Option<Key>,
        space: Id,
        device: EndpointId,
        ts: u64,
        url: String,
        frame: Vec<u8>,
    ) -> Self {
        let (ack, device, t) =
            (ack.unwrap_or_default(), *device.as_bytes(), now());
        let next = t + FIRST;
        Out {
            ack,
            space,
            device,
            ts,
            url,
            frame,
            first: t,
            tries: 0,
            next,
            sent: false,
        }
    }
}

impl Node {
    /// Whether frames of ours wait to go: with no node running, a mailbox
    /// back on its relay (the network is back) starts one for them.
    pub fn outbox_waiting(dir: &Path) -> bool {
        !store::load::<Vec<Out>>(&dir.join("outbox")).is_empty()
    }

    /// Kept before they first go: a process killed at once still sends.
    pub(super) fn keep_out(&self, add: Vec<Out>) {
        if add.is_empty() {
            return;
        }
        let mut o = lock(&self.outbox);
        for a in add {
            o.retain(|x| (x.device, &x.frame) != (a.device, &a.frame));
            o.push(a);
        }
        if let Some(over) = o.len().checked_sub(MOST).filter(|n| *n > 0) {
            warn!("outbox full: the {over} oldest unacked frames given up");
            o.drain(..over);
        }
        self.save_out(&o);
    }

    fn save_out(&self, o: &[Out]) {
        if let Err(e) = store::save(&self.dir.join("outbox"), &o) {
            warn!("saving the outbox: {e}");
        }
    }

    /// Delivered: what `done` names leaves the outbox; a big one is saved
    /// with the next change or beat (lost, it goes again, shown once).
    pub(super) fn out_done(&self, done: impl Fn(&Out) -> bool) {
        let mut o = lock(&self.outbox);
        let n = o.len();
        o.retain(|x| !done(x));
        match o.len() {
            m if m == n => {}
            m if m < 256 => self.save_out(&o),
            _ => self.out_dirty.store(true, Relaxed),
        }
    }

    /// The outbox on disk, if it changed since.
    pub(super) fn save_out_now(&self) {
        if self.out_dirty.swap(false, Relaxed) {
            self.save_out(&lock(&self.outbox));
        }
    }

    /// What is due goes again, once each, by its device's lane as it is
    /// now. `all`: everything (we start); `stuck`: those whose last try
    /// reached no relay too (the network may be back).
    pub(super) fn flush_out(self: &Arc<Self>, all: bool, stuck: bool) {
        let t = now();
        let due: Vec<Out> = {
            let mut o = lock(&self.outbox);
            let n = o.len();
            o.retain(|x| {
                let keep = t.saturating_sub(x.first) < MONTH;
                if !keep {
                    warn!("unacked a month, given up: a frame for a device");
                }
                keep
            });
            let go = |x: &&mut Out| all || x.next <= t || stuck && !x.sent;
            let due: Vec<Out> = (o.iter_mut().filter(go))
                .map(|x| {
                    let wait =
                        BACKOFF[(x.tries as usize).min(BACKOFF.len() - 1)];
                    (x.tries, x.next) =
                        (x.tries.saturating_add(1), t + wait * 1000);
                    x.clone()
                })
                .collect();
            if !due.is_empty()
                || o.len() != n
                || self.out_dirty.swap(false, Relaxed)
            {
                self.save_out(&o);
            }
            due
        };
        if due.is_empty() {
            return;
        }
        // Each device's lane as it is now, once: it may have moved.
        let mut lanes = HashMap::new();
        for sp in self.all() {
            for (d, l) in self.lanes(&sp.id) {
                lanes.entry(d).or_insert(l);
            }
        }
        for x in due {
            let Ok(d) = EndpointId::from_bytes(&x.device) else { continue };
            let now_at = lanes.get(&d).cloned();
            let Some(to) = now_at.or_else(|| crate::push::lane_of(&x.url))
            else {
                continue;
            };
            let acked = x.ack != Key::default();
            if acked {
                lock(&self.waits)
                    .insert(x.ack, (x.space, x.ts, d, false, Instant::now()));
            }
            let n = Arc::downgrade(self);
            tokio::spawn(async move {
                let sent = crate::push::send(&to, &x.frame).await;
                if let Some(n) = n.upgrade() {
                    let mut o = lock(&n.outbox);
                    let same = |o: &&mut Out| {
                        (o.device, &o.frame) == (x.device, &x.frame)
                    };
                    o.iter_mut().filter(same).for_each(|o| o.sent = sent);
                }
                // Bare (too big to carry): done once on the relay while a
                // storage node keeps the op, which the device catches up
                // from; else until it syncs with us.
                if let Some(n) = n
                    .upgrade()
                    .filter(|n| sent && !acked && n.kept(&x.space) >= x.ts)
                {
                    n.out_done(|o| {
                        (o.device, &o.frame) == (x.device, &x.frame)
                    });
                }
            });
        }
    }
}
