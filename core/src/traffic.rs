//! What each device and we moved this run: QUIC's own count of every link
//! either way (direct or relayed), the closed ones' totals and the open
//! ones' as they stand. In memory only.

use crate::node::lock;
use iroh::EndpointId;
use iroh::endpoint::{Connection, WeakConnectionHandle};
use iroh::protocol::{AcceptError, ProtocolHandler};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Peer {
    sent: u64,
    got: u64,
    open: Vec<WeakConnectionHandle>,

    last: u64,
}

#[derive(Default)]
pub(crate) struct Traffic(Mutex<HashMap<EndpointId, Peer>>);

impl std::fmt::Debug for Traffic {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("traffic")
    }
}

/// One device's this run: bytes sent and received, links open now, and
/// when last linked (ms; now, while one is open).
pub struct Seen {
    pub sent: u64,
    pub got: u64,
    pub open: usize,
    pub last: u64,
}

/// Open still: not closed, not dropped.
fn live(w: &WeakConnectionHandle) -> Option<Connection> {
    w.upgrade().filter(|c| c.close_reason().is_none())
}

impl Traffic {
    /// `c`, counted till it closes (its final count then: the handle kept
    /// keeps nothing open).
    pub(crate) fn count(self: &Arc<Self>, c: &Connection) {
        let (d, w) = (c.remote_id(), c.weak_handle());
        let closed = w.closed();
        {
            let mut m = lock(&self.0);
            let p = m.entry(d).or_default();
            p.open.push(w);
            p.last = crate::now();
        }
        let t = Arc::downgrade(self);
        tokio::spawn(async move {
            let end = closed.await;
            let Some(t) = t.upgrade() else { return };
            let mut m = lock(&t.0);
            let Some(p) = m.get_mut(&d) else { return };
            if let Some(e) = end {
                (p.sent, p.got) = (
                    p.sent + e.stats.udp_tx.bytes,
                    p.got + e.stats.udp_rx.bytes,
                );
            }
            p.open.retain(|w| live(w).is_some());
            p.last = crate::now();
        });
    }

    /// Each device linked this run, its open links counted as they stand.
    pub(crate) fn seen(&self) -> HashMap<EndpointId, Seen> {
        let m = lock(&self.0);
        let one = |p: &Peer| {
            let open: Vec<_> = p.open.iter().filter_map(live).collect();
            let now = open.iter().map(Connection::stats);
            let (s, g) = now.fold((0, 0), |a, s| {
                (a.0 + s.udp_tx.bytes, a.1 + s.udp_rx.bytes)
            });
            let last = if open.is_empty() { p.last } else { crate::now() };
            Seen { sent: p.sent + s, got: p.got + g, open: open.len(), last }
        };
        m.iter().map(|(d, p)| (*d, one(p))).collect()
    }
}

/// A protocol whose links are counted.
#[derive(Debug, Clone)]
pub(crate) struct Counted<P>(pub P, pub Arc<Traffic>);

impl<P: ProtocolHandler> ProtocolHandler for Counted<P> {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        self.1.count(&c);
        self.0.accept(c).await
    }

    async fn shutdown(&self) {
        self.0.shutdown().await
    }
}
