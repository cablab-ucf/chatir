//! One iroh endpoint: gossip (live) and sync (catch-up) for every space.
//! Every op, local or remote: open, check, append to the log, fold, notify.

mod files;
mod lanes;
mod live;
mod net;
mod pair;
mod peers;
mod space;
mod spaces;

pub use net::*;
use {lanes::*, live::*, pair::*, peers::*, space::*};

use crate::call::{self, Call};
use crate::crypto::{Key, Me, kem_open, kem_seal, rand, same};
use crate::op::{self, Env, File, Invite, Op, Role, Signed, code, tag, uncode};
use crate::state::State;
use crate::store::{self, Log};
use crate::sync::{self, Admit, Bucket, Hello};
use crate::{Ch, Id, now};
use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::{BindOpts, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey,
    TransportAddr,
};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::store::fs::FsStore;
use iroh_gossip::api::{Event as Gossiped, GossipSender};
use iroh_gossip::{Gossip, TopicId};
use n0_future::StreamExt;
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed,
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tracing::{debug, warn};

const DAY: u64 = 86_400_000;
/// How far ahead a clock may run: later ops wait (sync brings them again).
const SKEW: u64 = 600_000;
const GOSSIP_MAX: usize = 128 << 10;
// Local only (no packets): a late tick means we slept; storage budgets.
const BEAT: Duration = Duration::from_secs(60);
// Alone in a space, redial backing off to this, then wait: whoever comes
// online dials us, and a network change or chatir in sight redials (`resume`).
const ALONE: Duration = Duration::from_secs(64);
const QUIET: Duration = Duration::from_secs(365 * 86400);
/// Spaces up to this many ring for a call in any channel.
const RINGS: usize = 10;
/// No input this long: idle (`Idle`, once), still reachable.
pub const AWAY: Duration = Duration::from_secs(600);
/// In use, `Here` again at most this often; unheard twice as long (and a
/// bit), a device is taken for gone: its battery died, say. Idle, it says
/// nothing more, so it stands six hours.
const ALIVE: Duration = Duration::from_secs(1800);
const DEAD_IDLE: Duration = Duration::from_secs(6 * 3600);
// Typing, said again this often while it lasts; heard, it lasts 60 s.
const TYPING: Duration = Duration::from_secs(50);
// A callee says once that it rings; its caller hears ringback this long.
const RANG: Duration = Duration::from_secs(45);
pub use crate::op::{MAX_NAME, MAX_TEXT};
/// What storage nodes keep without their password.
pub const MAX_FILE: u64 = 5 << 30;
/// `MAX_FILE`, or in tests (debug builds) CHATIR_TEST_MAX_FILE.
fn limit() -> u64 {
    let test = std::env::var("CHATIR_TEST_MAX_FILE")
        .ok()
        .filter(|_| cfg!(debug_assertions));
    test.and_then(|v| v.parse().ok()).unwrap_or(MAX_FILE)
}

/// What opens in memory, to preview; larger files are saved.
const PREVIEW: usize = 25 << 20;
const BLOB: &[u8] = b"spaces/blob/1";
/// Ops dialed straight to a space's devices: `space ‖ sealed op` a stream.
const DIRECT: &[u8] = b"spaces/op/1";
/// A direct link quiet this long is closed: open links are pinged every 5 s.
const QUIET_LINK: Duration = Duration::from_secs(10);
/// Untouched this long with nothing open, iroh sleeps (`rest_iroh`).
const IDLE: Duration = Duration::from_secs(60);
/// Spaces up to this many devices are told directly; bigger, by gossip.
const FANOUT: usize = 64;

type Links = Arc<Mutex<HashMap<EndpointId, (Connection, Instant)>>>;
/// An op sent to a lane and not yet acked: its space, time and device,
/// whether it is live (never kept), and when it left.
type Wait = (Id, u64, EndpointId, bool, Instant);
/// An unacked op is forgotten after this long (`wake_who` retries to 58 s).
const WAITED: Duration = Duration::from_secs(120);
/// How long a text to one of our devices waits for it to wake.
const WOKEN: Duration = Duration::from_secs(70);
/// A text that came again within this long is a retry whose ack was lost
/// (the wire carries no id): taken, not told twice.
const AGAIN: Duration = Duration::from_secs(90);
/// A lane on another process's mailbox (pushir's daemon) is proved every
/// this many beats and trusted this long after; one held here, as each
/// connection begins, and trusted while it lasts (`wakes`).
const LANE_BEATS: u64 = 5;
const PROVED: Duration = Duration::from_secs(7 * 60);

/// Text handed to one of our own devices: one link, nothing kept elsewhere.
const HAND: &[u8] = b"spaces/hand/1";
const PARKED: usize = 32 << 20;
/// Refused ops a space remembers, so sync brings none again.
const REJECTED: usize = 4096;
const LOCKED: &str = "keys changed without you: ask for a new invite";

#[derive(Clone, Debug, Default)]
pub enum Net {
    /// The lab's relay, then n0's US East; n0's DNS.
    #[default]
    Default,
    /// Your own relays; nothing of n0's.
    Relay(Vec<RelayUrl>),
    Lan,
    /// Only addresses from codes and the address book (tests).
    Direct,
}

pub struct Config {
    pub dir: PathBuf,
    pub name: String,
    pub net: Net,
    /// Become another device of whoever made this link code.
    pub link: Option<String>,
    /// Be a storage node: keep others' ciphertext.
    pub serve: bool,
    /// A storage node's password, which keeps files past `MAX_FILE`.
    pub big: Option<String>,
    /// Where files our devices send go (`send.rs`); else the system's
    /// downloads, else DIR/received.
    pub inbox: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    Changed {
        space: Id,
    },
    Typing {
        space: Id,
        channel: Ch,
        user: Id,
        on: bool,
    },
    Presence {
        space: Id,
    },
    Synced {
        space: Id,
        count: usize,
    },
    Call {
        space: Id,
    },
    /// Text one of our devices handed this one.
    Handed {
        from: EndpointId,
        name: Option<String>,
        text: String,
    },
    /// `user` declined the call in `channel` (ours: on another device).
    Declined {
        space: Id,
        channel: Ch,
        user: Id,
    },
    /// A big file comes through a relay, not direct: at its rate limit.
    Relayed {
        size: u64,
    },
    /// A short code of ours was used up by someone who did not prove it.
    Spent,
    /// A device of ours, or of someone we share a space with, offers us a
    /// file (`accept`, `decline_file`).
    Incoming {
        id: Id,
        device: EndpointId,
        user: Id,
        name: String,
        size: u64,
    },
    /// A transfer (`transfers`) moved on: at most about four times a second.
    Transfer {
        id: Id,
    },
}

#[derive(Serialize, Deserialize, Default)]
struct Local {
    port: u16,
    invisible: bool,
    read: BTreeMap<(Id, Ch), u64>,
    joined: BTreeMap<Id, u64>,
    peers: BTreeMap<EndpointId, EndpointAddr>,
    token: Option<[u8; 16]>,
}

/// Apart from `Local`: postcard is positional, a new field resets it.
#[derive(Serialize, Deserialize, Default)]
struct Prefs {
    level: BTreeMap<Id, Level>,
    // Per space, notified (or seen) up to here.
    told: BTreeMap<Id, u64>,
}

/// A storage node's word for one space, from its owners: where members
/// meet, which epochs it keeps, and each owner's devices it takes from.
#[derive(Serialize, Deserialize, Default)]
struct Host {
    room: [u8; 32],
    tags: BTreeSet<[u8; 8]>,
    users: BTreeMap<Id, Vec<EndpointId>>,
}

/// A file served from here, to the members of the space it went to;
/// `space` none once its sender stopped sharing it.
#[derive(Serialize, Deserialize, Clone)]
struct Shared {
    space: Option<Id>,
    tag: Vec<u8>,
    name: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum Level {
    #[default]
    All,
    Mentions,
    Off,
}

#[derive(Serialize, Debug, Clone)]
pub struct Notice {
    pub space: Id,
    pub room: Ch,
    pub title: String,
    pub body: String,
    pub ring: bool,
    /// Its call rang, and ended: stop ringing.
    pub hush: bool,
    /// This device is on silent: shown, but no sound (set by the app).
    pub quiet: bool,
}

impl Notice {
    pub(crate) fn new(
        space: Id,
        room: Ch,
        title: String,
        body: String,
        ring: bool,
        hush: bool,
    ) -> Self {
        Self { space, room, title, body, ring, hush, quiet: false }
    }
}

/// A space as a ring needs it before any node starts: its current key,
/// whether its calls ring here, who is in it and its rooms' names.
#[derive(Serialize, Deserialize)]
struct Page {
    // Our own space: a text from one of our devices is under its key.
    #[serde(default)]
    own: bool,
    name: String,
    dm: bool,
    ring: bool,
    level: Level,
    // `@our name`, as messages mention us; and our newest posts, a reply
    // to which is to us.
    tag: String,
    #[serde(default)]
    ours: Vec<Id>,
    key: Key,
    names: BTreeMap<Id, String>,
    rooms: BTreeMap<Ch, String>,
    // Who we are: what we said from another device of ours tells us nothing.
    #[serde(default)]
    me: Option<Id>,
    // A small space's devices' lanes: what rang asleep is acked to its own.
    #[serde(default)]
    lanes: BTreeMap<EndpointId, String>,
}

/// A member's device here (said `Here`, or answered one) or `Gone`, when
/// it said so (older news changes nothing), and when we heard it.
type Seen = (bool, u64, Instant);

/// A member's call (or none), its device, and when it was said.
type Said = (Option<Ch>, EndpointId, u64);

pub struct Space {
    pub id: Id,
    inv: Option<Invite>,
    book: Mutex<Book>,
    tx: Mutex<Option<GossipSender>>,
    nbrs: Mutex<HashSet<EndpointId>>,
    seen: Mutex<HashMap<(Id, EndpointId), Seen>>,
    // Live ops already taken, so none is taken twice.
    heard: Mutex<HashMap<Id, Instant>>,
    calls: Mutex<HashMap<Id, Said>>,
    // Syncing with now; true: asked again meanwhile, so once more after.
    busy: Mutex<HashMap<EndpointId, bool>>,
    // Devices our storage nodes were last told they may take from.
    vouched: AtomicUsize,
    // The gossip topic we are in, and a nudge to move to a new one.
    room: Mutex<[u8; 32]>,
    moved: Arc<Notify>,
    // When we last rekeyed to mend a leak (`repair`).
    fixed: Mutex<Option<Instant>>,
    // Our ops written while nobody else was reachable.
    undelivered: Mutex<HashSet<Id>>,
    // Per member, our ops up to this time are on one of their devices:
    // they synced, gossiped, or a lane took them.
    reach: Mutex<HashMap<Id, u64>>,
    // The same per device: is it on the one its user used last?
    reach_dev: Mutex<HashMap<EndpointId, u64>>,
    // Devices open but untouched (`Idle`).
    idle: Mutex<HashSet<(Id, EndpointId)>>,
    // Members whose device says our call is ringing there, and when.
    ringing: Mutex<HashMap<(Id, Ch), Instant>>,
    task: OnceLock<AbortOnDropHandle<()>>,
}

struct Book {
    log: Log,
    idx: BTreeMap<u32, BTreeSet<Id>>,
    at: HashMap<Id, (u64, u32)>,
    keys: HashMap<[u8; 8], Key>,
    // Unopened yet; past `PARKED` bytes dropped (sync brings it again).
    pending: HashMap<Id, (Env, Src)>,
    parked: usize,
    // Per epoch, the older keys `Back` gives its holders.
    backs: HashMap<[u8; 8], Vec<[u8; 8]>>,
    // Learned once an admin's op stands (maybe after more history opens).
    offered: Vec<Offered>,
    st: State,
    knocks: Vec<Invite>,
    // Each rekey's envelope, by its op's id: shown to whoever it left out.
    rekeys: HashMap<Id, Id>,
    // Refused, oldest first: indexed as had, never served.
    rejected: VecDeque<(Id, u32)>,
    rejects: u64,
}

/// (author, (ts, id), epoch, key, via `Back` rather than `Rekey`).
type Offered = (Id, (u64, Id), [u8; 8], Key, bool);

/// In the log (0, or the version it checked as), or held till it opens
/// (ours: no check).
enum Src {
    Log(u8),
    Net(Vec<u8>, bool),
}

/// A file's name as `op::fits` takes it: 255 bytes, cut at a character.
pub(crate) fn short(n: &str) -> String {
    n[..n.floor_char_boundary(255)].into()
}

fn named(n: &str) -> Result<&str> {
    let n = n.trim();
    ensure!(!n.is_empty() && n.len() <= MAX_NAME, "names are 1 to 64 bytes");
    Ok(n)
}

/// One chatir to a folder: a second says so, rather than wait on the first.
async fn hold(dir: &Path) -> Result<std::fs::File> {
    let f = std::fs::File::create(dir.join("lock"))?;
    for _ in 0..50 {
        if f.try_lock().is_ok() {
            return Ok(f);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("another chatir is using {}", dir.display())
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

/// iroh, while needed: its endpoint, and the gossip and router on it.
#[derive(Clone)]
struct Iroh {
    ep: Endpoint,
    gossip: Gossip,
    router: Router,
    // The relay keys it was made with (`relay-keys` then).
    keys: Vec<(RelayUrl, String)>,
}

pub struct Node {
    /// Since when this node is up: its run, in the usage log.
    up: Instant,
    /// Storage node: devices admitted to sync, told our pushir relay.
    served: crate::push::Served,
    /// Asked a storage node for its relay this run.
    relayed: AtomicBool,
    // Ourselves, for tasks that outlive a call (acks on direct links).
    this: OnceLock<Weak<Node>>,
    // Per space, up to when one of its storage nodes has our ops.
    kept: Mutex<HashMap<Id, u64>>,
    // Where calls are heard.
    #[cfg(feature = "voice")]
    route: Mutex<crate::voice::Route>,
    // Direct links to devices, closed once quiet (`QUIET_LINK`).
    links: Links,
    // What each device and we moved this run.
    pub(crate) traffic: Arc<crate::traffic::Traffic>,
    // Ops sent as events (`push::event`), by the ack each awaits (space,
    // time, device, live?), and how many left that way and how many were
    // dialed after all.
    waits: Mutex<HashMap<Key, Wait>>,
    // Our lane, proved: the nonce last sent to it through its relay, and
    // when one last came back (`check_lane`).
    echo: Mutex<(Key, Option<Instant>)>,
    // When a bare wake or a mailbox's return last had us catch up.
    caught: Mutex<Option<Instant>>,
    evs: (AtomicUsize, AtomicUsize),
    // Our events address, as last told (`tell_push`).
    told_lane: Mutex<Option<String>>,
    // Read receipts waiting to go, the latest per channel (`mark_read`).
    reading: Mutex<HashMap<(Id, Ch), u64>>,
    me: Me,
    own: Id,
    dir: PathBuf,
    net: Net,
    // Our device's key; iroh while needed (`up`), closed once idle.
    key: SecretKey,
    iroh: RwLock<Option<Iroh>>,
    binding: tokio::sync::Mutex<()>,
    // Shut down: iroh never binds again.
    down: AtomicBool,
    used: Mutex<Instant>,
    server: Option<crate::push::Server>,
    lookup: MemoryLookup,
    spaces: RwLock<HashMap<Id, Arc<Space>>>,
    local: Mutex<Local>,
    prefs: Mutex<Prefs>,
    // Closed conversations, and when: hidden until something newer.
    closed: Mutex<BTreeMap<Id, u64>>,
    rang: Mutex<HashSet<(Id, Id, Ch)>>,
    // Texts from our devices not yet told: a notice each, as a message is.
    texts: Mutex<Vec<Notice>>,
    // Texts of ours sent by lane, by the ack each awaits.
    told: Mutex<HashMap<Key, tokio::sync::oneshot::Sender<()>>>,
    name: Mutex<String>,
    events: broadcast::Sender<Event>,
    clock: AtomicU64,
    invisible: AtomicBool,
    // Last input, last `Here` said, and away (said `Gone` for idling).
    input: Mutex<Instant>,
    said: Mutex<Instant>,
    away: AtomicBool,
    // Where we said we type, and when.
    typing: Mutex<Option<(Id, Ch, Instant)>>,
    serve: Option<[u8; 16]>,
    big: Option<String>,
    pub(crate) inbox: Option<PathBuf>,
    files: FsStore,
    held: Mutex<Option<std::fs::File>>,
    // Per space, of ops and (true) of files apart, the day and what it took
    // then (storage nodes).
    budget: Mutex<BTreeMap<(Id, bool), (u64, u64)>>,
    hosts: Mutex<BTreeMap<Id, Host>>,
    // Devices whose users proved them theirs (sync, calls).
    verified: Mutex<HashMap<EndpointId, (Id, Instant)>>,
    // Small files we asked a space's storage node to hold, newest last.
    asked: Mutex<VecDeque<(EndpointId, Id)>>,
    // Sync sessions answered now, by whom and for which space.
    pub(crate) talks:
        (tokio::sync::Semaphore, Mutex<HashSet<(EndpointId, Id)>>),
    shared: Mutex<BTreeMap<Id, Shared>>,
    // Files sent device to device, either way (`send.rs`).
    pub(crate) send: crate::send::Sends,
    // When each peer last synced with us, either way.
    synced: Mutex<HashMap<EndpointId, u64>>,
    call: Mutex<Option<Call>>,
    media: call::Media,
}

#[derive(Debug, Clone)]
struct Proto(Weak<Node>);

#[derive(Serialize, Deserialize)]
enum Ask {
    // Fetch a sealed file from the asker and keep it (a password past 5 GB).
    Keep([u8; 16], Id, Option<String>),
    // A member's small sealed file, for a space this storage node keeps.
    Hold(Id, [u8; 16], Id),
}

/// A storage node's key for one space's small files: members have it.
pub fn hold_token(token: &[u8; 16], space: &Id) -> [u8; 16] {
    let k = crate::crypto::kdf("hold", &[token, &space.0]);
    k[..16].try_into().expect("16 of 32")
}

/// Made only where none is: unreadable, a key is never replaced.
fn secret(path: &Path, link: Option<Key>) -> Result<Key> {
    match std::fs::read(path) {
        Ok(b) => {
            b.try_into().map_err(|_| anyhow!("{} is damaged", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let k = link.unwrap_or_else(rand);
            store::write(path, &k)?;
            Ok(k)
        }
        Err(e) => Err(e).context(path.display().to_string()),
    }
}

fn own_invite(me: &Me, peers: Vec<EndpointAddr>) -> Invite {
    let k = crate::crypto::kdf("own space", &[&me.seed]);
    let nonce = k[..16].try_into().expect("16");
    let key = crate::crypto::kdf("own key", &[&me.seed]);
    Invite { owner: me.id(), nonce, key, name: String::new(), dm: None, peers }
}

impl Node {
    pub async fn start(cfg: Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.dir)?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &cfg.dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )?;
        let held = hold(&cfg.dir).await?;
        let link: Option<(Key, Vec<EndpointAddr>)> =
            cfg.link.as_deref().map(|c| uncode("link", c)).transpose()?;
        let me =
            Me::new(secret(&cfg.dir.join("seed"), link.as_ref().map(|l| l.0))?);
        ensure!(
            link.as_ref().is_none_or(|l| l.0 == me.seed),
            "already someone else"
        );
        let dev =
            SecretKey::from_bytes(&secret(&cfg.dir.join("device"), None)?);
        let mut local: Local = store::load(&cfg.dir.join("local"));
        let mut prefs: Prefs = store::load(&cfg.dir.join("prefs"));
        // Told while asleep (`peek`): not again.
        let shown = cfg.dir.join("shown");
        for (id, ts) in store::load::<BTreeMap<Id, u64>>(&shown) {
            let t = prefs.told.entry(id).or_default();
            *t = (*t).max(ts.min(now()));
        }
        let _ = std::fs::remove_file(shown);
        let closed = store::load(&cfg.dir.join("closed"));
        // Direct (tests): one address book for the process, as DNS is for
        // the rest, so any device is found by its id.
        static BOOK: OnceLock<MemoryLookup> = OnceLock::new();
        let lookup = match cfg.net {
            Net::Direct => BOOK.get_or_init(MemoryLookup::new).clone(),
            _ => MemoryLookup::new(),
        };
        local.peers.values().for_each(|a| lookup.add_endpoint_info(a.clone()));
        let serve = cfg.serve.then(|| *local.token.get_or_insert_with(rand));
        let files = FsStore::load(cfg.dir.join("files")).await?;
        let own_inv = own_invite(&me, link.map(|l| l.1).unwrap_or_default());
        let hosts: BTreeMap<Id, Host> = store::load(&cfg.dir.join("hosts"));
        let budget = store::load(&cfg.dir.join("budget"));
        let shared = store::load(&cfg.dir.join("shared"));
        let send = crate::send::Sends::load(&cfg.dir);
        // Where our devices' files go: kept for a start that names none
        // (a phone woken without its app).
        let at = cfg.dir.join("downloads");
        let inbox = match cfg.inbox {
            Some(i) => {
                let _ = store::write(&at, i.to_string_lossy().as_bytes());
                Some(i)
            }
            None => std::fs::read_to_string(&at).ok().map(PathBuf::from),
        };
        // Owners' devices are served across restarts: they pair, register.
        let users = hosts.values().flat_map(|h| h.users.values().flatten());
        let served: crate::push::Served =
            Arc::new(Mutex::new(users.copied().collect()));
        let server = serve.and_then(|_| {
            crate::push::Server::open(&cfg.dir, &cfg.net, served.clone())
        });
        let n = Arc::new(Self {
            up: Instant::now(),
            served,
            relayed: AtomicBool::new(false),
            links: Links::default(),
            traffic: Arc::default(),
            waits: Mutex::default(),
            echo: Mutex::default(),
            caught: Mutex::default(),
            evs: Default::default(),
            told_lane: Mutex::default(),
            reading: Mutex::default(),
            this: OnceLock::new(),
            kept: Mutex::default(),
            #[cfg(feature = "voice")]
            route: Mutex::default(),
            own: own_inv.id(),
            me,
            dir: cfg.dir,
            net: cfg.net,
            key: dev,
            iroh: RwLock::new(None),
            binding: tokio::sync::Mutex::new(()),
            down: AtomicBool::new(false),
            used: Mutex::new(Instant::now()),
            server,
            lookup,
            spaces: RwLock::default(),
            invisible: AtomicBool::new(local.invisible),
            input: Mutex::new(Instant::now()),
            said: Mutex::new(Instant::now()),
            away: AtomicBool::new(false),
            typing: Mutex::default(),
            local: Mutex::new(local),
            prefs: Mutex::new(prefs),
            closed: Mutex::new(closed),
            rang: Mutex::default(),
            texts: Mutex::default(),
            told: Mutex::default(),
            name: Mutex::new(cfg.name),
            events: broadcast::channel(1024).0,
            clock: AtomicU64::new(0),
            serve,
            big: cfg.big,
            inbox,
            files,
            held: Mutex::new(Some(held)),
            budget: Mutex::new(budget),
            hosts: Mutex::new(hosts),
            verified: Mutex::default(),
            asked: Mutex::default(),
            talks: (tokio::sync::Semaphore::new(32), Mutex::default()),
            shared: Mutex::new(shared),
            send,
            synced: Mutex::default(),
            call: Mutex::new(None),
            media: broadcast::channel(128).0,
        });
        let _ = n.this.set(Arc::downgrade(&n));
        n.up().await?;
        n.persist();
        n.save_hosts();
        if serve.is_some() {
            let ids: Vec<Id> = lock(&n.hosts).keys().copied().collect();
            for id in ids {
                let peers = n.owners(&id).into_iter().take(16);
                n.open(None, id, peers.map(EndpointAddr::new).collect())?;
            }
        } else {
            let own = n.open(Some(own_inv), n.own, vec![])?;
            let named = n.view(&n.own, |s| s.profile(&n.id()).is_some());
            if cfg.link.is_none() && named == Some(false) {
                let (name, kem) = (n.name(), n.me.pk());
                n.post(&own, Op::Profile { name, kem }).await?;
            }
            // Named for us at once (renamed in My Account › Your devices).
            let device = n.device();
            if n.label(&device).is_none() {
                let name = host_name();
                n.post(&own, Op::Label { device, name }).await?;
            }
            n.reconcile().await;
        }
        // Our lane (a phone's is the mailbox its platform holds), heard
        // here unless the app hears it for us; spaces told where.
        let phone = cfg!(any(target_os = "android", target_os = "ios"))
            || crate::push::shared();
        if !phone {
            let r = crate::push::ours(&n.dir, &n.net, serve.is_some()).await;
            let _ = r.inspect_err(|e| warn!("lane: {e:#}"));
        }
        if !crate::push::heard(&n.dir) {
            let rx = crate::push::listen(&n.dir);
            tokio::spawn(hear(Arc::downgrade(&n), rx));
        }
        tokio::spawn(lane_up(Arc::downgrade(&n)));
        tokio::spawn(heartbeat(Arc::downgrade(&n)));
        n.resume_sends();
        crate::usage::note(&n.dir, "up", serde_json::json!({}));
        Ok(n)
    }

    /// Our device's id, iroh up or not.
    pub fn device(&self) -> EndpointId {
        self.key.public()
    }

    /// Who you are, on every device.
    pub fn id(&self) -> Id {
        self.me.id()
    }

    pub fn name(&self) -> String {
        lock(&self.name).clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn seal(
        &self,
        sp: &Space,
        key: &Key,
        ts: u64,
        op: Op,
    ) -> (Signed, Vec<u8>) {
        let (me, dev) = (&self.me.user, self.device());
        op::seal(&sp.id, key, me, dev, ts, op)
    }

    /// Ourselves, while we run.
    pub(crate) fn this(&self) -> Option<Arc<Node>> {
        self.this.get()?.upgrade()
    }

    /// `name` in our folder, as `v` is now; a failure only logged.
    pub(crate) fn stash<T: Serialize>(&self, name: &str, v: &T) {
        let r = store::save(&self.dir.join(name), v);
        let _ = r.inspect_err(|e| warn!("saving {name}: {e}"));
    }

    fn persist(&self) {
        self.stash("local", &*lock(&self.local));
    }

    /// Out of sight, kept: back with its next message, or when opened.
    pub fn hide(&self, id: &Id, on: bool) {
        let last = self.last(id).unwrap_or_default();
        let mut c = lock(&self.closed);
        let _ = if on { c.insert(*id, last) } else { c.remove(id) };
        self.stash("closed", &*c);
        drop(c);
        self.emit(Event::Changed { space: *id });
    }

    pub fn hidden(&self, id: &Id) -> bool {
        let t = lock(&self.closed).get(id).copied();
        t.is_some_and(|t| self.last(id).unwrap_or_default() <= t)
    }

    fn last(&self, id: &Id) -> Option<u64> {
        let last = |s: &State| {
            s.channels().iter().filter_map(|(c, _)| s.latest(c)).max()
        };
        self.view(id, last).flatten()
    }

    fn save_prefs(&self) {
        self.stash("prefs", &*lock(&self.prefs));
    }

    /// How loud `id` is: as set on any device of ours (our own space, kept
    /// by our storage nodes), else as this device had it before.
    pub fn level(&self, id: &Id) -> Level {
        let set = self.view(&self.own, |s| s.level(id)).flatten();
        set.or_else(|| lock(&self.prefs).level.get(id).copied())
            .unwrap_or_default()
    }

    /// For every device of ours: said in our own space; members ring us
    /// only as it says, so our push is told again.
    pub fn set_level(&self, id: &Id, l: Level) {
        lock(&self.prefs).level.insert(*id, l);
        self.save_prefs();
        if let Some(n) = self.this.get().and_then(Weak::upgrade) {
            let id = *id;
            tokio::spawn(async move {
                if let Ok(own) = n.get(&n.own) {
                    let _ =
                        n.post(&own, Op::Notify { space: id, level: l }).await;
                }
                drop(n.tell_push(&id).await);
            });
        }
    }

    /// Per channel or thread, the newest unread (by level), once across wakes.
    pub fn notices(&self) -> Vec<Notice> {
        let (me, mut out, mut live) = (self.id(), vec![], HashSet::new());
        let was = lock(&self.prefs).told.clone();
        for id in self.spaces() {
            let (level, mark) = {
                let mut p = lock(&self.prefs);
                let l = p.level.get(&id).copied().unwrap_or_default();
                (l, *p.told.entry(id).or_insert_with(now))
            };
            let Some((name, dm, rooms, newest)) = self.view(&id, |s| {
                let rooms = s.rooms();
                let newest = rooms.iter().filter_map(|r| s.latest(&r.0)).max();
                (s.name().to_string(), s.dm.is_some(), rooms, newest)
            }) else {
                continue;
            };
            for (ch, room) in rooms.iter().filter(|_| level != Level::Off) {
                let since = self.since(&id, ch).max(mark);
                let m = self.view(&id, |s| match level {
                    Level::All => s.last_from(ch, since, &me),
                    _ => s.last_mention(ch, since, &me),
                });
                let Some(m) = m.flatten() else { continue };
                let title = match dm {
                    true => m.name.clone(),
                    false => format!("{} in #{room} ({name})", m.name),
                };
                let body =
                    if m.text.is_empty() { "(file)".into() } else { m.text };
                out.push(Notice::new(id, *ch, title, body, false, false));
            }
            // Others' calls, each once while it lasts; not the one we are in
            // (its callee answering is no call to us).
            let calls = self.get(&id).map(|sp| self.callers(&sp, None));
            let calls = calls.unwrap_or_default();
            let joined = self.in_call().filter(|c| c.0 == id).map(|c| c.1);
            // Taken on another of ours: it rings here no more.
            let ours: HashSet<Ch> =
                calls.iter().filter(|c| c.2 == me).map(|c| c.1).collect();
            for (_, ch, u) in calls {
                let fresh = u != me && lock(&self.rang).insert((id, u, ch));
                live.insert((id, u, ch));
                let tell = level != Level::Off && (dm || level == Level::All);
                if !fresh || !tell || ours.contains(&ch) || joined == Some(ch) {
                    continue;
                }
                let who = self.name_of(&u, Some(id));
                let who = who.unwrap_or_else(|| u.to_string()[..8].into());
                let room = rooms.iter().find(|r| r.0 == ch);
                let room = room.map_or("call".into(), |r| r.1.to_string());
                let (title, body) = match dm {
                    true => (who, "is calling you".into()),
                    false => (
                        format!("{who} in #{room} ({name})"),
                        "is calling the channel".into(),
                    ),
                };
                let ring = self.loud(&id);
                out.push(Notice::new(id, ch, title, body, ring, false));
            }
            let p = &mut lock(&self.prefs).told;
            p.insert(id, mark.max(newest.unwrap_or(0).min(now())));
        }
        lock(&self.rang).retain(|c| live.contains(c));
        out.extend(self.offer_notices());
        out.append(&mut lock(&self.texts));
        // On disk only when it moved: this runs on every change.
        if lock(&self.prefs).told != was {
            self.save_prefs();
        }
        out
    }

    /// `u`'s name in `first`, else any space we share: a new DM may lack
    /// their profile.
    pub fn name_of(&self, u: &Id, first: Option<Id>) -> Option<String> {
        let mut all = first.into_iter().chain(self.spaces());
        all.find_map(|sp| self.view(&sp, |s| s.profile(u).map(Into::into))?)
    }

    /// Our lane, as this space's members should have it, and how loud we
    /// are here: its events only members can label.
    async fn tell_push(self: &Arc<Self>, id: &Id) -> Result<()> {
        let Some(url) = crate::push::lane_at(&self.dir) else {
            return Ok(());
        };
        let sp = self.get(id)?;
        if sp.inv.is_none() {
            return Ok(());
        }
        // How loud we are here: members ring us only for what we would show.
        let url = match self.level(id) {
            Level::All => url,
            Level::Mentions => format!("{url} level:mentions"),
            Level::Off => format!("{url} level:off"),
        };
        let (me, dev) = (self.id(), self.device());
        let has = self.view(id, |s| s.push(&me, &dev).map(str::to_string));
        if has.flatten().as_deref() == Some(&url) {
            return Ok(());
        }
        self.post(&sp, Op::Push { url: Some(url) }).await?;
        Ok(())
    }

    fn get(&self, id: &Id) -> Result<Arc<Space>> {
        let m = read(&self.spaces);
        m.get(id).cloned().ok_or_else(|| anyhow!("not in that space"))
    }

    fn all(&self) -> Vec<Arc<Space>> {
        let m = read(&self.spaces);
        m.values().cloned().collect()
    }

    /// Our own space: every device of ours and our storage nodes, nobody
    /// else. Its posts are notes to self.
    pub fn own(&self) -> Id {
        self.own
    }

    pub fn view<R>(&self, id: &Id, f: impl FnOnce(&State) -> R) -> Option<R> {
        self.get(id).ok().map(|s| f(&lock(&s.book).st))
    }

    /// Strictly increasing wall-clock ms.
    fn tick(&self) -> u64 {
        let t = now();
        let prev = self.clock.fetch_max(t, Relaxed);
        if prev >= t { self.clock.fetch_add(1, Relaxed) + 1 } else { t }
    }

    pub async fn shutdown(&self) {
        self.phonebook();
        let call = lock(&self.call).take();
        if let Some(sp) = call.and_then(|c| self.get(&c.space).ok()) {
            self.signal(&sp, Op::Call { channel: None }).await;
        }
        // Out of every space first: nothing says `Here` after `Gone`.
        let all = self.all();
        write(&self.spaces).clear();
        for sp in all {
            self.signal(&sp, Op::Gone).await;
        }
        // Gossip sends after `broadcast` returns: a moment for `Gone` to go.
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.stop_sends().await;
        self.down.store(true, Relaxed);
        let moved = self.usage();
        let _one = self.binding.lock().await;
        let i = write(&self.iroh).take();
        if let Some(i) = i {
            let _ = i.router.shutdown().await;
            i.ep.close().await;
        }
        // Closed with the node alone: every router it had kept it open.
        let _ = self.files.shutdown().await;
        lock(&self.held).take();
        crate::usage::note(&self.dir, "down", moved);
    }

    /// This run so far: seconds up, and what iroh moved.
    pub fn usage(&self) -> serde_json::Value {
        let ep = self.ep();
        let mut v = ep.map_or_else(
            || serde_json::json!({}),
            |e| crate::usage::counted(&e),
        );
        v["s"] = self.up.elapsed().as_secs().into();
        v
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// `name` in `dir`, else `stem (1).ext`, `stem (2).ext`… as browsers save.
pub(crate) fn names(dir: &Path, name: &str) -> impl Iterator<Item = PathBuf> {
    let (dir, name) = (dir.to_path_buf(), name.to_string());
    (0..).map(move |i| {
        let (stem, ext) = name.rsplit_once('.').unwrap_or((&name, ""));
        let dot = if ext.is_empty() { "" } else { "." };
        dir.join(match i {
            0 => name.clone(),
            i => format!("{stem} ({i}){dot}{ext}"),
        })
    })
}

/// From the internet, like a browser download: Windows and macOS check it
/// before it runs.
pub(crate) fn mark(p: &Path) {
    #[cfg(windows)]
    let _ = std::fs::write(
        format!("{}:Zone.Identifier", p.display()),
        "[ZoneTransfer]\r\nZoneId=3\r\n",
    );
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/xattr")
        .args(["-w", "com.apple.quarantine", "0081;0;chatir;"])
        .arg(p)
        .status();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = p;
}

/// The blob store, served by each router iroh has (it rests and wakes):
/// a router's end leaves it open (iroh-blobs' own handler closes it, and
/// every image and file after the first rest then failed: "send update").
/// The node's end closes it.
#[derive(Debug, Clone)]
struct Kept(BlobsProtocol);

/// An event's label, for space `id` under key `k`: a relay sees no space.
fn ev_label(id: &Id, k: &Key) -> Key {
    crate::crypto::kdf("event", &[&id.0, k])
}

/// A text by lane: from which of our devices, when, that device's lane
/// (for the ack), its name, and the text.
type Text = (EndpointId, u64, String, String, String);

/// The ack device `d` owes for an event at `ts`: none but a member makes
/// it, so no relay drops an op and acks it.
fn ev_ack(id: &Id, k: &Key, ts: u64, d: &EndpointId) -> Key {
    crate::crypto::kdf(
        "event ack",
        &[&id.0, k, &ts.to_le_bytes(), d.as_bytes()],
    )
}

/// How loud a device asked to be in a space (its push's `level:mentions`
/// or `level:off`; all, if it says neither).
fn level_of(push: &str) -> Level {
    let has = |t| push.split_whitespace().any(|x| x == t);
    match (has("level:off"), has("level:mentions")) {
        (true, _) => Level::Off,
        (_, true) => Level::Mentions,
        _ => Level::All,
    }
}

struct Public;

/// A storage node's name at first: its computer's, marked as one.
pub fn storage_name() -> String {
    format!("{}-storage", host_name())
}

/// This computer's name, or what kind of device it is.
pub fn host_name() -> String {
    let file = std::fs::read_to_string("/etc/hostname").ok();
    let cmd = || {
        let o = std::process::Command::new("hostname").output().ok()?;
        Some(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let h = std::env::var("COMPUTERNAME").ok().or(file).or_else(cmd);
    let h = h.map(|h| h.trim().trim_end_matches(".local").to_string());
    let h = h.filter(|h| !h.is_empty() && h != "localhost");
    h.unwrap_or_else(|| {
        match std::env::consts::OS {
            "android" => "Android phone",
            "ios" => "iPhone",
            "macos" => "Mac",
            "windows" => "Windows PC",
            _ => "Linux computer",
        }
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn relay() -> (iroh_relay::server::Server, String) {
        use iroh_relay::server::{RelayConfig, Server, ServerConfig};
        let mut cfg = ServerConfig::default();
        cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
        let s = Server::spawn(cfg).await.unwrap();
        let at = format!("http://{}/", s.http_addr().unwrap());
        (s, at)
    }

    /// Homed on one relay, a device is reached by one homed on another
    /// (our own, the lab's, n0's while that is down): iroh dials whichever relay
    /// the address names.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_is_reached_on_any_relay() {
        let ((_x, one), (_y, two)) = (relay().await, relay().await);
        let ep = async |l: &str| {
            let b = endpoint(&Net::Relay(vec![l.parse().unwrap()]));
            b.alpns(vec![b"t".to_vec()]).bind().await.unwrap()
        };
        let (a, b) = (ep(&one).await, ep(&two).await);
        let secs = Duration::from_secs(10);
        tokio::time::timeout(secs, a.online()).await.expect("a home");
        let ours = a.addr().addrs.into_iter().filter(|x| x.is_relay());
        let to = EndpointAddr::from_parts(a.id(), ours);
        tokio::spawn(async move { a.accept().await?.await.ok() });
        let c = tokio::time::timeout(secs, b.connect(to, b"t")).await;
        assert!(c.expect("in time").is_ok(), "b reached a");
    }

    async fn start(net: &Net, name: &str) -> Arc<Node> {
        let dir =
            std::env::temp_dir().join(format!("chatir-test-{name}-{}", now()));
        let (name, net) = (name.into(), net.clone());
        let cfg = Config {
            dir,
            name,
            net,
            link: None,
            serve: false,
            big: None,
            inbox: None,
        };
        Node::start(cfg).await.unwrap()
    }

    async fn until(f: impl Fn() -> bool) -> bool {
        for _ in 0..400 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn stop(n: Arc<Node>) {
        n.shutdown().await;
        let _ = std::fs::remove_dir_all(&n.dir);
    }

    pub(super) fn post(ch: Ch, text: &str) -> Op {
        Op::Post { channel: ch, text: text.into(), reply: None, files: vec![] }
    }

    /// Our own relays (`--relay`), no DNS, no address given: a member is
    /// found on them, and a post and a call's ring reach it.
    #[tokio::test(flavor = "multi_thread")]
    async fn our_own_relays_find_each_device() {
        let (_r, url) = relay().await;
        let net = Net::Relay(vec![url.parse().unwrap()]);
        let (a, b) = (start(&net, "a").await, start(&net, "b").await);
        let id = a.create("duo").await.unwrap();
        let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
        b.join(&a.invite(&id).await.unwrap()).await.unwrap();
        let knows =
            || a.view(&id, |s| s.owner_of(&b.device()).is_some()) == Some(true);
        assert!(until(knows).await, "a knows b's device");
        // c comes through a alone, and has never reached b.
        let c = start(&net, "c").await;
        c.join(&a.invite(&id).await.unwrap()).await.unwrap();
        let c_knows =
            || c.view(&id, |s| s.owner_of(&b.device()).is_some()) == Some(true);
        assert!(until(c_knows).await, "c knows b's device");
        c.publish(&id, post(ch, "found you")).await.unwrap();
        let got = || {
            let m =
                b.view(&id, |s| s.messages(&ch, None, 10)).unwrap_or_default();
            m.iter().any(|m| m.text == "found you")
        };
        assert!(until(got).await, "b has it, from c");
        c.call(Some((id, ch))).await.unwrap();
        assert!(until(|| !b.calls(&id).is_empty()).await, "b hears the call");
        for n in [a, b, c] {
            stop(n).await;
        }
    }

    /// Who knows only a space's id is neither heard nor met, so never
    /// synced with and never told its room: only an op that opens, signed
    /// on the device that sends it, by someone not cut off, is.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_checked_op_makes_a_neighbor() {
        let net = Net::Direct;
        let a = start(&net, "gate").await;
        let id = a.create("s").await.unwrap();
        let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
        let key =
            async || Invite::decode(&a.invite(&id).await.unwrap()).unwrap().key;
        let (sp, to, k) =
            (a.get(&id).unwrap(), a.addr().await.unwrap(), key().await);
        // From a fresh device, `raw(its id)`; whether we met it.
        let from = async |raw: &dyn Fn(EndpointId) -> Vec<u8>| {
            let x = endpoint(&net).bind().await.unwrap();
            let c = x.connect(to.clone(), DIRECT).await.unwrap();
            let mut s = c.open_uni().await.unwrap();
            s.write_all(&[&id.0[..], &raw(x.id())].concat()).await.unwrap();
            s.finish().unwrap();
            (x, c) // open till read
        };
        let met = |x: &(Endpoint, Connection)| sp.nbrs().contains(&x.0.id());
        let (u, other) =
            (Me::new(rand()), SecretKey::from_bytes(&rand()).public());
        let by =
            |k: Key, d| op::seal(&id, &k, &u.user, d, now(), post(ch, "hi")).1;
        let junk =
            Env { epoch: tag(&k), ts: now(), live: false, ct: vec![7; 64] };
        let bad = [
            from(&|_| postcard::to_allocvec(&junk).unwrap()).await,
            from(&|_| by(k, other)).await, // signed for another device
            from(&|d| by(rand(), d)).await, // a key nobody here holds
        ];
        let good = from(&|d| by(k, d)).await;
        assert!(until(|| met(&good)).await, "a newcomer's op, checked");
        // Rekeyed: an old invite's holder is cut off.
        a.rekey(&id).await.unwrap();
        let old = from(&|d| by(k, d)).await;
        let k2 = key().await;
        let fresh = from(&|d| by(k2, d)).await;
        assert!(until(|| met(&fresh)).await, "under the new key");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!bad.iter().chain([&old]).any(met), "none of these");
        // Refused a sync, it binds no user to its device either.
        let (x, room) = (SecretKey::from_bytes(&rand()).public(), [9; 32]);
        let who = Some(sync::sign(&u.user, &id, &room, &a.device()));
        let (token, root, devices, tags) = (None, [0; 32], vec![], vec![]);
        let h = Hello { space: id, root, room, who, token, devices, tags };
        assert!(matches!(a.find(&h, x).1, Admit::No));
        assert!(!lock(&a.verified).contains_key(&x));
        stop(a).await;
    }

    /// A storage node charges a member's file to the space's files, never
    /// its ops, when asked; and gives it back if none of it came.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_never_comes_is_given_back() {
        let s = start(&Net::Direct, "keeper").await;
        let (sp, size) = (Id(rand()), 25 << 20);
        let spent = |file| lock(&s.budget).get(&(sp, file)).map_or(0, |u| u.1);
        s.budget(&sp, size, true).unwrap();
        assert_eq!((spent(true), spent(false)), (size, 0));
        let nobody = SecretKey::from_bytes(&rand()).public();
        s.fetch(&Id(rand()), nobody, Some((sp, size)));
        assert!(until(|| spent(true) == 0).await, "none came");
        stop(s).await;
    }

    /// Events name no space; a relay can neither ack them nor (with a
    /// device two members claim) take them in its place.
    #[tokio::test(flavor = "multi_thread")]
    async fn events_name_no_space_and_only_members_ack() {
        let a = start(&Net::Direct, "events").await;
        let id = a.create("s").await.unwrap();
        let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
        let sp = a.get(&id).unwrap();
        let k = lock(&sp.book).key(sp.inv.as_ref().unwrap()).0;
        let dev = |n: u8| SecretKey::from_bytes(&[n; 32]).public();
        let (u, v) = (Me::new(rand()), Me::new(rand()));
        let push = |who: &Me, d, lane: EndpointId| {
            let relay = "https://relay.invalid./".parse().unwrap();
            let l = pushir::Lane { relay, id: lane, secret: [0; 32] };
            let url = Some(l.to_string());
            op::seal(&id, &k, &who.user, d, now(), Op::Push { url }).1
        };
        let claims = vec![
            push(&u, dev(1), dev(11)),
            push(&v, dev(1), dev(12)),
            push(&u, dev(2), dev(13)),
        ];
        a.ingest(&sp, claims, false);
        let lanes = a.lanes(&id);
        assert!(!lanes.contains_key(&dev(1)), "claimed twice: neither's");
        assert!(lanes[&dev(2)].id == dev(13));
        // u's op from device 2's lane: under its label, not the space's id.
        let raw = op::seal(&id, &k, &u.user, dev(2), now(), post(ch, "ev")).1;
        a.hear(&[&[0u8][..], &id.0, &raw].concat());
        assert!(!sp.nbrs().contains(&dev(2)), "the old, open form");
        a.hear(&[&[0u8][..], &ev_label(&id, &k), &raw].concat());
        assert!(sp.nbrs().contains(&dev(2)));
        // Acked only with what a member alone can make.
        let want = ev_ack(&id, &k, 7, &dev(2));
        lock(&a.waits).insert(want, (id, 7, dev(2), false, Instant::now()));
        a.hear(&[&[1u8][..], &id.0, &7u64.to_le_bytes()].concat());
        a.hear(&[&[1u8][..], &ev_ack(&id, &rand(), 7, &dev(2))].concat());
        assert!(lock(&a.waits).contains_key(&want), "forged");
        a.hear(&[&[1u8][..], &want].concat());
        assert!(lock(&a.waits).is_empty());
        stop(a).await;
    }

    /// While a DM someone knocked with waits for them, knocks of theirs
    /// open no more: a card's holder fills no disk with DMs.
    #[tokio::test(flavor = "multi_thread")]
    async fn knocks_open_one_waiting_dm_each() {
        let a = start(&Net::Direct, "knocks").await;
        let id = a.create("s").await.unwrap();
        let sp = a.get(&id).unwrap();
        let k = lock(&sp.book).key(sp.inv.as_ref().unwrap()).0;
        let d = SecretKey::from_bytes(&rand()).public();
        let knock = |who: &Me| {
            let dm =
                postcard::to_allocvec(&Invite::new(who.id(), "", Some(a.id())));
            let sealed = kem_seal(&a.me.pk(), &id.0, &dm.unwrap()).unwrap();
            op::seal(&id, &k, &who.user, d, now(), Op::Knock { sealed }).1
        };
        let (x, y) = (Me::new(rand()), Me::new(rand()));
        a.ingest(&sp, vec![knock(&x), knock(&x), knock(&y)], false);
        let by = |u: Id| {
            let theirs =
                |s: &Arc<Space>| s.inv.as_ref().is_some_and(|i| i.owner == u);
            a.all().iter().filter(|s| theirs(s)).count()
        };
        assert!(until(|| by(x.id()) + by(y.id()) == 2).await);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!((by(x.id()), by(y.id()), a.waiting()), (1, 1, 2));
        a.ingest(&sp, vec![knock(&x)], false);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(by(x.id()), 1, "still one");
        stop(a).await;
    }
}
