//! Push through pushir: each device holds one relay mailbox, shared by its
//! apps (pushir's daemon or Android library, else our own), and chatir hears
//! on its lane there: a secret its senders hold, under which every frame is
//! sealed, so the relay routes a frame and reads nothing of it. Members send
//! a device's lane small frames (`split`). No Google: no Firebase. A frame is a hint: the lab's
//! relay keeps one until its mailbox has it, another relay may lose it, so
//! whatever a frame carries is acked, and dialed or synced if it is not.

use crate::{Net, node::lock, store};
use anyhow::{Result, ensure};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey};
use pushir::{Key, Lane};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver, UnboundedSender};

pub const ALPN: &[u8] = b"spaces/push/1";
const SLOW: Duration = Duration::from_secs(15);

#[derive(Serialize, Deserialize)]
enum Ask {
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

/// The first relay of `url[#key],…`.
fn first(l: &str) -> Option<RelayUrl> {
    let url = |r: &str| r.split('#').next()?.trim().parse::<RelayUrl>().ok();
    l.split(',').filter_map(url).find(|u| u.scheme().starts_with("http"))
}

/// A storage node's pushir relay, checked, kept in DIR/push-relay.
pub fn set_relay(dir: &Path, l: &str) -> Result<()> {
    ensure!(first(l).is_some(), "no relay given");
    std::fs::create_dir_all(dir)?;
    store::write(&dir.join("push-relay"), l.trim().as_bytes())
}

/// The relay our lane is on: a storage node's own, from the systemd
/// credential `push-relay` (kept out of the Nix store), DIR/push-relay
/// (`chatir --install --push-relay`) or `CHATIR_PUSH_RELAY`; else the one a
/// storage node of ours told us (DIR/relays), else pushir's
/// (irpush.cxn.center, or `PUSHIR_RELAY`). Our own relays (`--relay`): the
/// first. LAN only, none.
pub fn relay(dir: &Path, net: &Net, serve: bool) -> Option<RelayUrl> {
    let read = |d: &Path, f: &str| std::fs::read_to_string(d.join(f)).ok();
    let cred = std::env::var_os("CREDENTIALS_DIRECTORY")
        .and_then(|d| read(Path::new(&d), "push-relay"))
        .or_else(|| read(dir, "push-relay"));
    let given = cred.or_else(|| std::env::var("CHATIR_PUSH_RELAY").ok());
    match (given.filter(|_| serve), net) {
        (Some(l), _) => first(&l),
        (_, Net::Default) => read(dir, "relays")
            .and_then(|l| first(&l))
            .or_else(|| pushir::relay().ok()),
        (_, Net::Relay(u)) => u.first().cloned(),
        _ => None,
    }
}

/// The relays of storage node `to`.
pub async fn relays_of(ep: &Endpoint, to: EndpointAddr) -> Result<String> {
    let l = String::from_utf8(ask(ep, to, &Ask::Relays).await?)?;
    ensure!(first(&l).is_some(), "no relays there");
    Ok(l)
}

/// Devices this storage node admitted to sync: members it keeps for.
pub type Served = Arc<Mutex<HashSet<EndpointId>>>;

/// A storage node, asked by the devices it serves for the relay their
/// lanes go on (its own, if it runs one).
#[derive(Clone)]
pub struct Server(Arc<(Served, String)>);

impl Server {
    pub fn open(dir: &Path, net: &Net, s: Served) -> Option<Self> {
        Some(Self(Arc::new((s, relay(dir, net, true)?.to_string()))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 64).await?;
        let (served, relays) = &*self.0;
        // Members' devices we sync with only.
        let ours = lock(served).contains(&c.remote_id());
        let reply = match postcard::from_bytes(&b)? {
            Ask::Relays if ours => relays.clone().into_bytes(),
            Ask::Relays => vec![],
        };
        tx.write_all(&reply).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }
}

// ── Lanes: a device's mailbox and chatir's lane there ──
//
// Whoever holds a lane (`pushir:ID:SECRET@RELAY`: the members of a device's
// spaces) sends it `kind ‖ 32 bytes ‖ rest` (`split`): an op under its
// space's label (sealed and signed as ever), its ack (a MAC only a member
// makes), or a dial (start iroh, a link is coming).
// pushir seals each under the lane's secret, padded to one size: the relay
// learns a mailbox and a time. An empty message says sync: a bare wake, or
// the mailbox back from a gap in which frames may have been missed.

/// What a frame carries: an op (empty: catch up), an ack (its ts), a dial,
/// or a text from one of our devices (since 0.4.9: an older one ignores
/// it, and is reached directly as before).
pub const OP: u8 = 0;
pub const ACK: u8 = 1;
pub const DIAL: u8 = 2;
pub const TEXT: u8 = 3;
/// A space, and one sealed op of it: what a lane brought.
pub type News = (crate::Id, Vec<u8>);
/// A message for chatir, opened (pushir's senders are nameless).
pub type Heard = Vec<u8>;
static EARS: Mutex<BTreeMap<PathBuf, UnboundedSender<Heard>>> =
    Mutex::new(BTreeMap::new());
/// Each profile's own lane, once known ("" while it attaches).
static LANES: Mutex<BTreeMap<PathBuf, String>> = Mutex::new(BTreeMap::new());
/// Android: pushir's library holds the mailbox; its id ("" until told).
static SHARED: Mutex<Option<String>> = Mutex::new(None);

/// chatir's lane secret on this device's mailbox: random, kept.
pub fn secret(dir: &Path) -> Result<Key> {
    std::fs::create_dir_all(dir)?;
    pushir::kept(&dir.join("lane"))
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

/// A lane message, `kind ‖ 32 bytes ‖ rest`: an event (its space's label,
/// then the op), an ack (its MAC), a dial (a label) or a text (our own
/// space's label, then the text).
pub fn split(b: &[u8]) -> Option<(u8, [u8; 32], &[u8])> {
    let (k, r) = b.split_first().filter(|(k, _)| **k <= TEXT)?;
    let (x, rest) = r.split_first_chunk::<32>()?;
    Some((*k, *x, rest))
}

/// A desktop's or storage node's lane, if it has none: on the device's
/// mailbox (pushir's daemon) if that is on the relay we would use, else on
/// a mailbox of our own there, pinged as rarely as its NAT allows.
pub async fn ours(dir: &Path, net: &Net, serve: bool) -> Result<()> {
    let Some(on) = relay(dir, net, serve) else { return Ok(()) };
    let secret = secret(dir)?;
    let sk = SecretKey::from_bytes(&pushir::kept(&dir.join("mailbox"))?);
    {
        let mut l = lock(&LANES);
        if l.contains_key(dir) {
            return Ok(());
        }
        l.insert(dir.into(), String::new());
    }
    let daemon = match pushir::relay().is_ok_and(|r| r == on) {
        true => daemon(&secret).await,
        false => None,
    };
    let (id, rx) = match daemon {
        Some((id, rx)) => (id, Some(rx)),
        None => (sk.public(), None),
    };
    let at = move |id| Lane { relay: on.clone(), id, secret }.to_string();
    lock(&LANES).insert(dir.into(), at(id));
    let d = dir.to_path_buf();
    tokio::spawn(async move {
        if let Some(mut rx) = rx {
            while let Some(m) = rx.recv().await {
                hand(&d, m);
            }
            // The daemon left: our own mailbox, told by the heartbeat.
            lock(&LANES).insert(d.clone(), at(sk.public()));
        }
        let Ok(l) = at(sk.public()).parse::<Lane>() else { return };
        let hear = |f: &[u8]| match f.is_empty() {
            true => hand(&d, vec![]),
            false => {
                pushir::open(&secret, f).into_iter().for_each(|m| hand(&d, m))
            }
        };
        pushir::listen(&l.relay, &sk, hear).await
    });
    Ok(())
}

/// The device's mailbox, held by pushir's daemon, if one answers.
async fn daemon(secret: &Key) -> Option<(EndpointId, Receiver<Vec<u8>>)> {
    #[cfg(unix)]
    return pushir::local::attach(&pushir::local::sock(), secret).await.ok();
    #[cfg(not(unix))]
    {
        let _ = secret;
        None
    }
}

/// Android: pushir's library holds our lane, mailbox `id` ("" until told).
pub fn share(id: &str) {
    *lock(&SHARED) = Some(id.into());
}

pub fn shared() -> bool {
    lock(&SHARED).is_some()
}

/// Ours, as its senders get it (`pushir:ID:SECRET@RELAY`), once known.
pub fn lane_at(dir: &Path) -> Option<String> {
    let l = lock(&LANES).get(dir).filter(|l| !l.is_empty()).cloned();
    l.or_else(|| {
        let id = lock(&SHARED).clone()?.parse().ok()?;
        let (relay, secret) = (pushir::relay().ok()?, secret(dir).ok()?);
        Some(Lane { relay, id, secret }.to_string())
    })
}

/// A device's lane, from its push.
pub fn lane_of(push: &str) -> Option<Lane> {
    push.split_whitespace().find_map(|t| t.parse().ok())
}

/// How long a connection to a relay is kept after its last frame.
const KEPT: Duration = Duration::from_secs(20);

type Out = Arc<tokio::sync::Mutex<Option<(pushir::Sender, Instant)>>>;
/// One nameless connection per relay we send through, kept `KEPT` after
/// its last frame: a burst (an op to every device of a space, their acks)
/// costs one handshake, not one each; idle, nothing stays open.
static OUT: LazyLock<Mutex<HashMap<RelayUrl, Out>>> =
    LazyLock::new(Mutex::default);

/// `msg` to lane `to`: whether it left. Not delivery: only its ack is.
pub async fn send(to: &Lane, msg: &[u8]) -> bool {
    if msg.len() > pushir::MAX {
        return false;
    }
    let out = lock(&OUT).entry(to.relay.clone()).or_default().clone();
    let mut o = out.lock().await;
    // Twice at most: a connection gone stale, then a fresh one.
    for _ in 0..2 {
        let (mut s, fresh) = match o.take().filter(|s| s.1.elapsed() < KEPT) {
            Some((s, _)) => (s, false),
            None => match pushir::Sender::connect(&to.relay).await {
                Ok(s) => (s, true),
                Err(e) => {
                    tracing::debug!("lane: {e:#}");
                    return false;
                }
            },
        };
        match s.send(to, msg).await {
            Ok(()) => {
                *o = Some((s, Instant::now()));
                if fresh {
                    tokio::spawn(idle(out.clone()));
                }
                return true;
            }
            Err(e) => tracing::debug!("lane: {e:#}"),
        }
    }
    false
}

/// Closes a relay connection `KEPT` after its last frame.
async fn idle(out: Out) {
    loop {
        tokio::time::sleep(KEPT).await;
        let mut o = out.lock().await;
        match o.take() {
            Some(s) if s.1.elapsed() < KEPT => *o = Some(s),
            Some((s, _)) => return drop(s.close().await),
            None => return,
        }
    }
}

/// Another network: what was open to a relay is gone with the old one.
pub async fn reset() {
    let all: Vec<Out> = lock(&OUT).values().cloned().collect();
    for o in all {
        o.lock().await.take();
    }
}

/// One message to lane `to`, while we have a lane for its ack: whether it
/// left.
pub async fn event(dir: &Path, to: &Lane, b: &[u8]) -> bool {
    lane_at(dir).is_some() && send(to, b).await
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("push server")
    }
}

impl ProtocolHandler for Server {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        self.asked(&c).await.map_err(|e| {
            AcceptError::from_err(std::io::Error::other(e.to_string()))
        })
    }
}

#[test]
fn a_storage_nodes_relay_is_checked_kept_and_told() {
    let n = u64::from_le_bytes(crate::crypto::rand());
    let d = std::env::temp_dir().join(format!("chatir-relay-{n}"));
    assert!(set_relay(&d, "not a url").is_err());
    let r = "https://pushir.example./".to_string();
    set_relay(&d, &r).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("push-relay")).unwrap(), r);
    let url = |u: &str| u.parse::<RelayUrl>().ok();
    let ours = pushir::relay().ok();
    assert_eq!(relay(&d, &Net::Default, false), ours, "pushir's own");
    assert_eq!(relay(&d, &Net::Default, true), url(&r), "a storage node's");
    std::fs::write(d.join("relays"), format!("{r}#key,https://b.example./"))
        .unwrap();
    assert_eq!(relay(&d, &Net::Default, false), url(&r), "as it told us");
    let own = Net::Relay(vec![url("https://own.example./").unwrap()]);
    assert_eq!(relay(&d, &own, false), url("https://own.example./"));
    assert_eq!(relay(&d, &Net::Lan, false), None);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn frames_are_told_apart() {
    let f = [&[ACK][..], &[7; 32], b"op"].concat();
    assert_eq!(split(&f), Some((ACK, [7; 32], &b"op"[..])));
    assert_eq!(split(&[DIAL; 33]).map(|x| x.2.len()), Some(0));
    assert_eq!(split(&[TEXT; 33]).map(|x| x.0), Some(TEXT));
    assert!(split(&[TEXT + 1; 40]).is_none() && split(&[OP; 32]).is_none());
    let l = Lane {
        relay: "https://r.example./".parse().unwrap(),
        id: SecretKey::from_bytes(&[1; 32]).public(),
        secret: [2; 32],
    };
    let push = format!("{l} level:mentions");
    assert!(lane_of(&push) == Some(l));
    assert!(lane_of("level:off").is_none());
}

/// A stranger is told no relay; a device we sync with is.
#[tokio::test(flavor = "multi_thread")]
async fn only_served_devices_are_told_our_relay() {
    let n = u64::from_le_bytes(crate::crypto::rand());
    let dir = std::env::temp_dir().join(format!("chatir-served-{n}"));
    set_relay(&dir, "https://pushir.invalid./").unwrap();
    let served = Served::default();
    let s = Server::open(&dir, &Net::Direct, served.clone()).unwrap();
    let ep = || crate::node::endpoint(&Net::Direct);
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
    lock(&served).insert(x.id());
    assert!(relays_of(&y, at.clone()).await.is_err(), "a stranger");
    assert!(relays_of(&x, at).await.is_ok());
    let _ = std::fs::remove_dir_all(dir);
}
