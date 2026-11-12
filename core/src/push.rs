//! Push through irpush: each device holds one relay mailbox, shared by its
//! apps (irpush's daemon or Android library, else our own), and irch hears
//! at its slot there. Members send a device's lane small frames (`split`);
//! storage nodes ring the phones that registered, sealed to each phone. No
//! Google: no Firebase. irpush queues nothing: a device away misses a
//! frame, and catches up as it opens (it syncs with its storage nodes).

use crate::crypto::{self, Key};
use crate::wake::{HEAD, MAX};
use crate::{Net, node::lock, store};
use anyhow::{Result, ensure};
use data_encoding::HEXLOWER as HEX;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey};
use irpush::Slot;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub const ALPN: &[u8] = b"irch/push/5";
/// Ring tokens a phone may have here: one per space it is in.
const MOST: usize = 4096;
/// Phones kept here, and ring tokens in all: past either, the one longest
/// unregistered goes.
const PHONES: usize = 1024;
const RINGS: usize = 1 << 16;
const SLOW: Duration = Duration::from_secs(15);
/// What a ring is sealed for: one phone, by a storage node.
const RING: &[u8] = b"irch ring";

#[derive(Serialize, Deserialize)]
enum Ask {
    /// Our `route` and ring tokens.
    Register(String, Vec<[u8; 16]>),
    /// The storage node's relays, `url,…`, which its iroh relays through.
    Relays,
}

async fn ask(ep: &Endpoint, to: EndpointAddr, a: &Ask) -> Result<Vec<u8>> {
    let go = async {
        let c = ep.connect(to, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let got = crate::sync::rest(&mut rx, 4096).await?;
        c.close(0u8.into(), b"");
        Ok(got)
    };
    tokio::time::timeout(SLOW, go).await?
}

/// Our best relay, and whether it is quiet (irpush's, pinged hours apart):
/// a storage node's, or irch's first; not a stock one.
pub fn relay(dir: &Path) -> (String, bool) {
    let r = std::fs::read_to_string(dir.join("relays")).ok();
    let first = r.as_deref().and_then(|r| r.split(',').next());
    let url = first.and_then(|f| f.split('#').next());
    let url = url.filter(|u| u.starts_with("http"));
    let url = url.unwrap_or(crate::node::RELAYS[0]);
    let stock = crate::node::RELAYS[1..]
        .iter()
        .any(|s| s.parse::<iroh::RelayUrl>().ok() == url.parse().ok());
    (url.split('#').next().unwrap_or(url).into(), !stock)
}

/// The first relay of `url[#key],…`.
fn first(l: &str) -> Option<RelayUrl> {
    let url = |r: &str| r.split('#').next()?.trim().parse::<RelayUrl>().ok();
    l.split(',').filter_map(url).find(|u| u.scheme().starts_with("http"))
}

/// A storage node's irpush relay, checked, kept in DIR/push-relay.
pub fn set_relay(dir: &Path, l: &str) -> Result<()> {
    ensure!(first(l).is_some(), "no relay given");
    std::fs::create_dir_all(dir)?;
    store::write(&dir.join("push-relay"), l.trim().as_bytes())
}

/// Our relays: a storage node's from the systemd credential `push-relay`
/// (kept out of the Nix store), DIR/push-relay (`irch --install
/// --push-relay`) or `IRCH_PUSH_RELAY`; else our best (`relay`), or our
/// own. LAN only, none.
fn relays(dir: &Path, net: &Net, serve: bool) -> Option<String> {
    let read = |d: &Path| std::fs::read_to_string(d.join("push-relay")).ok();
    let cred = std::env::var_os("CREDENTIALS_DIRECTORY")
        .and_then(|d| read(Path::new(&d)))
        .or_else(|| read(dir));
    let given = cred.or_else(|| std::env::var("IRCH_PUSH_RELAY").ok());
    match (given.filter(|_| serve), net) {
        (Some(l), _) => Some(l.trim().into()),
        (_, Net::Default) => Some(relay(dir).0),
        (_, Net::Relay(u)) => Some(format!("{},!n0", u.first()?)),
        _ => None,
    }
}

/// The relays of storage node `to`.
pub async fn relays_of(ep: &Endpoint, to: EndpointAddr) -> Result<String> {
    let l = String::from_utf8(ask(ep, to, &Ask::Relays).await?)?;
    ensure!(first(&l).is_some(), "no relays there");
    Ok(l)
}

pub async fn register(
    ep: &Endpoint,
    to: EndpointAddr,
    route: &str,
    rings: &[[u8; 16]],
) -> Result<()> {
    let a = Ask::Register(route.into(), rings.to_vec());
    ensure!(ask(ep, to, &a).await? == [1], "not taken there");
    Ok(())
}

type Phone = (String, Vec<[u8; 16]>);

/// Each phone's route and tokens (kept on disk), whose each token is, and
/// when each last registered (since we started; before, oldest).
#[derive(Default)]
struct Phones {
    by: BTreeMap<EndpointId, Phone>,
    rings: HashMap<[u8; 16], HashSet<EndpointId>>,
    at: HashMap<EndpointId, Instant>,
}

impl Phones {
    /// A route (its key aside) another device registered first.
    fn taken(&self, d: &EndpointId, to: &str) -> bool {
        let at = |r: &str| r.split('#').next().map(str::to_string);
        self.by.iter().any(|(x, p)| x != d && at(&p.0) == at(to))
    }

    fn set(&mut self, d: EndpointId, p: Option<Phone>) {
        for t in self.by.remove(&d).into_iter().flat_map(|p| p.1) {
            if let Some(s) = self.rings.get_mut(&t)
                && s.remove(&d)
                && s.is_empty()
            {
                self.rings.remove(&t);
            }
        }
        self.at.remove(&d);
        let Some(p) = p else { return };
        for t in &p.1 {
            self.rings.entry(*t).or_default().insert(d);
        }
        self.at.insert(d, Instant::now());
        self.by.insert(d, p);
    }
}

/// Devices this storage node admitted to sync: members it keeps for.
pub type Served = Arc<Mutex<HashSet<EndpointId>>>;

/// A storage node's push server: its file, phones, devices served, relays.
#[derive(Clone)]
pub struct Server(Arc<(PathBuf, Mutex<Phones>, Served, String)>);

impl Server {
    pub fn open(dir: &Path, net: &Net, s: Served) -> Option<Self> {
        let relays = relays(dir, net, true)?;
        let file = dir.join("phones");
        let mut phones = Phones::default();
        let kept: BTreeMap<EndpointId, Phone> = store::load(&file);
        kept.into_iter().for_each(|(d, p)| phones.set(d, Some(p)));
        phones.at.clear();
        Some(Self(Arc::new((file, Mutex::new(phones), s, relays))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST + 64).await?;
        let (file, phones, served, relays) = &*self.0;
        let d = c.remote_id();
        // Members' devices we sync with only: a stranger rings nobody.
        let ours = lock(served).contains(&d);
        let reply = match postcard::from_bytes(&b)? {
            Ask::Relays if ours => relays.clone().into_bytes(),
            Ask::Relays => vec![],
            Ask::Register(to, rings) => {
                ensure!(to.len() <= 512 && rings.len() <= MOST, "too big");
                // A route is the device's that registered it first: no
                // member rings another's phone with tokens of their own.
                let ok = ours
                    && route_of(&to).is_some()
                    && !lock(phones).taken(&d, &to);
                if ok {
                    // A second apart each, so none rewrites the file flat out.
                    let second = Duration::from_secs(1);
                    let last = lock(phones).at.get(&d).copied();
                    let wait = last.map(|t| second.saturating_sub(t.elapsed()));
                    tokio::time::sleep(wait.unwrap_or_default()).await;
                    let mut p = lock(phones);
                    p.set(d, Some((to, rings)));
                    while p.by.len() > PHONES || p.rings.len() > RINGS {
                        let old = p.by.keys().min_by_key(|d| p.at.get(*d));
                        let old = *old.expect("some");
                        p.set(old, None);
                    }
                    store::save(file, &p.by)?;
                }
                vec![ok as u8]
            }
        };
        tx.write_all(&reply).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }

    /// A member's ring, sealed to whichever phones hold its token: the
    /// space and the op, or too big for one frame, the space alone, for the
    /// phone to catch up.
    async fn ring(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let mut head = [0u8; HEAD];
        rx.read_exact(&mut head).await?;
        let raw = crate::sync::rest(&mut rx, MAX).await?;
        let (_, phones, _, _) = &*self.0;
        let t: [u8; 16] = head[..16].try_into()?;
        let to: Vec<String> = {
            let p = lock(phones);
            let to = p.rings.get(&t).into_iter().flatten();
            to.filter_map(|d| Some(p.by.get(d)?.0.clone())).collect()
        };
        let ring = [&head[16..48], &raw[..]].concat();
        let fits = ring.len() + 40 <= irpush::MAX;
        let r = if fits { &ring[..] } else { &ring[..32] };
        let mut ok = false;
        for (id, slot, relay, k) in to.iter().filter_map(|t| route_of(t)) {
            let sealed = crypto::seal(&k, RING, r);
            let sent = irpush::send(&relay, id, slot, &sealed).await;
            ok |= sent.inspect_err(|e| tracing::warn!("push: {e:#}")).is_ok();
        }
        // 2: sent; lost if the phone is away (it catches up as it opens).
        tx.write_all(&[if ok { 2 } else { 0 }]).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }
}

// ── Lanes: a device's mailbox and irch's slot there ──
//
// Members send a device's lane `kind ‖ 32 bytes ‖ rest` (`split`): an op
// under its space's label (sealed and signed as ever: a relay sees no
// space), its ack (a MAC only a member makes), or a dial (start iroh, a
// link is coming). Rings from a storage node come the same way, sealed.

/// What a frame carries: an op (empty: catch up), an ack (its ts), a dial.
pub const OP: u8 = 0;
pub const ACK: u8 = 1;
pub const DIAL: u8 = 2;
/// A frame for irch (irpush's senders are nameless).
pub type Heard = Vec<u8>;
static EARS: Mutex<BTreeMap<PathBuf, UnboundedSender<Heard>>> =
    Mutex::new(BTreeMap::new());
/// Each profile's own lane, once known ("" while it attaches).
static LANES: Mutex<BTreeMap<PathBuf, String>> = Mutex::new(BTreeMap::new());
/// Android: irpush's library holds the mailbox; its id ("" until told).
static SHARED: Mutex<Option<String>> = Mutex::new(None);

/// `N` random bytes kept in DIR/`name`.
fn kept<const N: usize>(dir: &Path, name: &str) -> Result<[u8; N]> {
    let f = dir.join(name);
    if let Some(k) = std::fs::read(&f).ok().and_then(|k| k.try_into().ok()) {
        return Ok(k);
    }
    std::fs::create_dir_all(dir)?;
    let k = crypto::rand();
    store::write(&f, &k)?;
    Ok(k)
}

/// irch's slot on this device's mailbox.
pub fn slot(dir: &Path) -> Result<Slot> {
    kept(dir, "irpush.slot")
}

fn at(id: EndpointId, slot: Slot, relay: &RelayUrl) -> String {
    format!("irch:ev:{id}:{}@{relay}", HEX.encode(&slot))
}

/// A lane, `irch:ev:ID:SLOT@RELAY`, or a route (`irpush:…#KEY`).
fn addr(t: &str) -> Option<(EndpointId, Slot, RelayUrl)> {
    let t = t.strip_prefix("irch:ev:").or_else(|| t.strip_prefix("irpush:"))?;
    let (a, relay) = t.split_once('@')?;
    let (id, slot) = a.split_once(':')?;
    let slot = HEX.decode(slot.as_bytes()).ok()?.try_into().ok()?;
    let relay = relay.split('#').next()?.parse().ok()?;
    Some((id.parse().ok()?, slot, relay))
}

fn route_of(r: &str) -> Option<(EndpointId, Slot, RelayUrl, Key)> {
    let (id, slot, relay) = addr(r).filter(|_| r.starts_with("irpush:"))?;
    let k = HEX.decode(r.rsplit_once('#')?.1.as_bytes()).ok()?;
    Some((id, slot, relay, k.try_into().ok()?))
}

/// What reaches `dir`'s lane from now on, here alone.
pub fn listen(dir: &Path) -> UnboundedReceiver<Heard> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    lock(&EARS).insert(dir.into(), tx);
    rx
}

/// Whether someone hears `dir`'s lane.
pub fn heard(dir: &Path) -> bool {
    lock(&EARS).get(dir).is_some_and(|e| !e.is_closed())
}

fn hand(dir: &Path, m: Vec<u8>) {
    let mut e = lock(&EARS);
    if e.get(dir).is_some_and(|e| e.send(m).is_err()) {
        e.remove(dir);
    }
}

/// A lane frame, `kind ‖ 32 bytes ‖ rest`: an event (its space's label,
/// then the op), an ack (its MAC) or a dial (a label).
pub fn split(b: &[u8]) -> Option<(u8, [u8; 32], &[u8])> {
    let (k, r) = b.split_first().filter(|(k, _)| **k <= DIAL)?;
    let (x, rest) = r.split_first_chunk::<32>()?;
    Some((*k, *x, rest))
}

/// A ring a storage node sealed to us: `space ‖ op`.
pub fn ring(dir: &Path, b: &[u8]) -> Option<Vec<u8>> {
    crypto::open(&kept(dir, "irpush.seal").ok()?, RING, b)
}

/// A desktop's or storage node's lane, if it has none: the device's
/// mailbox (irpush's daemon) if on the relay we would use, else our own
/// there, pinged every 25 min.
pub async fn ours(dir: &Path, net: &Net, serve: bool) -> Result<()> {
    let Some(on) = relays(dir, net, serve).as_deref().and_then(first) else {
        return Ok(());
    };
    let (slot, sk) =
        (slot(dir)?, SecretKey::from_bytes(&kept(dir, "irpush.key")?));
    {
        let mut l = lock(&LANES);
        if l.contains_key(dir) {
            return Ok(());
        }
        l.insert(dir.into(), String::new());
    }
    let daemon = match on == irpush::relay() {
        true => daemon(slot).await,
        false => None,
    };
    let (id, rx) = match daemon {
        Some((id, rx)) => (id, Some(rx)),
        None => (sk.public(), None),
    };
    lock(&LANES).insert(dir.into(), at(id, slot, &on));
    let d = dir.to_path_buf();
    tokio::spawn(async move {
        if let Some(mut rx) = rx {
            while let Some(m) = rx.recv().await {
                hand(&d, m);
            }
            lock(&LANES).insert(d.clone(), at(sk.public(), slot, &on));
        }
        let hear = |s: Slot, m: &[u8]| {
            if s == slot {
                hand(&d, m.to_vec())
            }
        };
        irpush::listen(&on, &sk, Duration::from_secs(25 * 60), hear).await
    });
    Ok(())
}

/// The device's mailbox, held by irpush's daemon, if one answers.
async fn daemon(
    slot: Slot,
) -> Option<(EndpointId, UnboundedReceiver<Vec<u8>>)> {
    #[cfg(unix)]
    return irpush::local::attach(&irpush::local::sock(), slot).await.ok();
    #[cfg(not(unix))]
    {
        let _ = slot;
        None
    }
}

/// Android: irpush's library holds our lane, mailbox `id` ("" until told).
pub fn share(id: &str) {
    *lock(&SHARED) = Some(id.into());
}

pub fn shared() -> bool {
    lock(&SHARED).is_some()
}

/// Ours, as members get it (`irch:ev:ID:SLOT@RELAY`), once known.
pub fn lane_at(dir: &Path) -> Option<String> {
    let l = lock(&LANES).get(dir).filter(|l| !l.is_empty()).cloned();
    l.or_else(|| {
        let id = lock(&SHARED).clone()?.parse().ok()?;
        Some(at(id, slot(dir).ok()?, &irpush::relay()))
    })
}

/// Our route, for the storage nodes we register with alone: our lane, and
/// the key a ring to us is sealed under.
pub fn route(dir: &Path) -> Option<String> {
    let l = lane_at(dir)?;
    let k = kept::<32>(dir, "irpush.seal").ok()?;
    Some(format!("irpush:{}#{}", l.strip_prefix("irch:ev:")?, HEX.encode(&k)))
}

/// A device's lane, from its push.
pub fn lane_of(push: &str) -> Option<String> {
    let lane = |t: &&str| t.starts_with("irch:ev:") && addr(t).is_some();
    push.split_whitespace().find(lane).map(Into::into)
}

/// One frame to lane `to`, while we have one for its ack: whether it left.
/// Lost if `to` is away (irpush queues nothing).
pub async fn event(dir: &Path, to: &str, b: &[u8]) -> bool {
    let Some((id, slot, relay)) = addr(to).filter(|_| lane_at(dir).is_some())
    else {
        return false;
    };
    let r = irpush::send(&relay, id, slot, b).await;
    r.inspect_err(|e| tracing::debug!("event: {e:#}")).is_ok()
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("push server")
    }
}

impl ProtocolHandler for Server {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let r = match c.alpn() {
            ALPN => self.asked(&c).await,
            _ => self.ring(&c).await,
        };
        r.map_err(|e| {
            AcceptError::from_err(std::io::Error::other(e.to_string()))
        })
    }
}

#[test]
fn a_storage_nodes_relay_is_checked_kept_and_told() {
    let n = u64::from_le_bytes(crate::crypto::rand());
    let d = std::env::temp_dir().join(format!("irch-relay-{n}"));
    assert!(set_relay(&d, "not a url").is_err());
    let r = "https://irpush.example./".to_string();
    set_relay(&d, &r).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("push-relay")).unwrap(), r);
    assert_eq!(relay(&d), (crate::node::RELAYS[0].into(), true), "irch's own");
    let stock = crate::node::RELAYS[1];
    std::fs::write(d.join("relays"), stock).unwrap();
    assert_eq!(relay(&d), (stock.into(), false), "a stock one");
    std::fs::write(d.join("relays"), format!("{r},https://b.example./"))
        .unwrap();
    assert_eq!(relay(&d), ("https://irpush.example./".into(), true));
    let _ = std::fs::remove_dir_all(d);
}

/// A stranger gets no relays or registration; a device we sync with does,
/// and only the route it registered first is its.
#[tokio::test(flavor = "multi_thread")]
async fn only_served_devices_register_here() {
    let n = u64::from_le_bytes(crate::crypto::rand());
    let dir = std::env::temp_dir().join(format!("irch-served-{n}"));
    set_relay(&dir, "https://irpush.invalid./").unwrap();
    let served = Served::default();
    let s = Server::open(&dir, &Net::Direct, served.clone()).unwrap();
    let ep = || crate::node::endpoint(&Net::Direct, None);
    let host = ep().alpns(vec![ALPN.to_vec()]).bind().await.unwrap();
    let at = host.addr();
    tokio::spawn(async move {
        while let Some(i) = host.accept().await {
            let s = s.clone();
            tokio::spawn(async move {
                if let Ok(c) = i.await {
                    let _ = s.asked(&c).await;
                }
            });
        }
    });
    let (x, y) = (ep().bind().await.unwrap(), ep().bind().await.unwrap());
    let route = |k: u8| {
        let (s, k) = (HEX.encode(&[1; 16]), HEX.encode(&[k; 32]));
        format!("irpush:{}:{s}@https://r.invalid./#{k}", x.id())
    };
    lock(&served).insert(x.id());
    assert!(relays_of(&y, at.clone()).await.is_err(), "a stranger");
    assert!(register(&y, at.clone(), &route(1), &[]).await.is_err());
    assert!(relays_of(&x, at.clone()).await.is_ok());
    assert!(register(&x, at.clone(), "irpush:x", &[]).await.is_err());
    assert!(register(&x, at.clone(), &route(1), &[]).await.is_ok());
    lock(&served).insert(y.id());
    let theirs = register(&y, at, &route(2), &[]).await;
    assert!(theirs.is_err(), "x's route, whatever the key");
    let _ = std::fs::remove_dir_all(dir);
}
