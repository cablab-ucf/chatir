//! Files sent device to device at the link's speed: served in place under a
//! random id (nothing hashed first), pulled in blocks over many streams on
//! several connections, each block checked, resumable.
//!
//! The receiver asks for block `i` of `id`; the sender answers with its
//! bytes, then their BLAKE3, hashed as it read them. The receiver writes
//! each block in place into `name.part`, hashing as it writes, asks again
//! for one whose hash differs, and keeps a bitmap of finished blocks beside
//! it (`name.part.map`): a transfer goes on where it stopped, after a
//! restart too.

use crate::node::{Event, Node, lock, mark, names, short};
use crate::op::{DIRECT, File};
use crate::{Id, now, store, sync};
use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use n0_future::task::AbortOnDropHandle;
use n0_future::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tracing::warn;

pub const ALPN: &[u8] = b"chatir/send/1";
/// Asked for, checked and noted as one: 4 to 16 MiB ran as fast on
/// loopback; smaller loses less when a transfer stops (streams × this is
/// in flight).
const BLOCK: u64 = 4 << 20;
/// What a stream reads or writes at once: memory is streams × this a side.
const SLICE: usize = 1 << 20;
/// Connections × streams on each that a receiver pulls with: the best
/// measured on loopback (a connection seals on one core). 64 streams of
/// 2 MiB a round trip, each idle one between blocks: 7 Gbit/s at 100 ms.
/// `CHATIR_SEND` (`4x16`) tries another.
const SHAPE: (usize, usize) = (8, 8);
/// Blocks a peer may have us send at once, over all its connections.
const PER_PEER: usize = 512;
/// A block whose hash differs is asked again this many times a round.
const TRIES: u8 = 3;
/// Offers in a space this big go device to device, not through iroh-blobs.
pub const BIG: u64 = 1 << 30;
/// Progress is told every tick; finished blocks noted on disk every
/// `FLUSH` ticks (5 s), data synced first.
const TICK: Duration = Duration::from_millis(250);
const FLUSH: u32 = 20;
/// The longest wait between tries to reach a sender (a network change
/// tries at once).
const PATIENCE: Duration = Duration::from_secs(600);
/// A stream silent this long mid-block is given up.
const STALL: Duration = Duration::from_secs(60);
/// Transfers listed at most, and offers waiting from one device: offers
/// past either are refused.
const ROWS: usize = 1024;
const WAITING: usize = 16;

#[derive(Serialize, Deserialize)]
enum Ask {
    /// Block `i` of `id`: `OK`, its bytes, their BLAKE3.
    Get(Id, u64),
    /// Whether we may have `id`: `OK` and its size (8 bytes, LE).
    Head(Id),
    /// A file for the asking device: its id, name and size, from `user`.
    Offer(Id, String, u64, Id),
    /// No more of `id`.
    End(Id, End),
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
enum End {
    Declined,
    Cancelled,
    Done,
}

const OK: u8 = 0;
/// Not here, or not for you: the same answer.
const NO: u8 = 1;
const BUSY: u8 = 2;
/// Changed or unreadable at its sender since it was offered.
const GONE: u8 = 3;

/// Who may fetch a file served from here.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
enum To {
    Device(EndpointId),
    /// The members' devices of a space (a chat offer).
    Space(Id),
}

/// A file served in place, as it was when offered.
#[derive(Serialize, Deserialize, Clone)]
struct Served {
    path: PathBuf,
    name: String,
    size: u64,
    modified: u64,
    to: To,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Stage {
    Offered,
    Waiting,
    Running,
    Done,
    Failed,
    Declined,
    Cancelled,
}

impl Stage {
    fn over(self) -> bool {
        use Stage::*;
        matches!(self, Done | Failed | Declined | Cancelled)
    }
}

impl Row {
    /// Offered, just now.
    fn new(device: EndpointId, user: Id, name: &str, size: u64) -> Self {
        let (name, stage, at) = (name.into(), Stage::Offered, now());
        let (space, out, path, error) = (None, false, None, None);
        Self { device, user, space, name, size, out, stage, path, error, at }
    }
}

/// A transfer either way: `device` sends to us, or we to it; `space`,
/// a chat offer's.
#[derive(Serialize, Deserialize, Clone)]
struct Row {
    device: EndpointId,
    user: Id,
    space: Option<Id>,
    name: String,
    size: u64,
    out: bool,
    stage: Stage,
    path: Option<PathBuf>,
    error: Option<String>,
    at: u64,
}

/// Kept in `sends`.
#[derive(Serialize, Deserialize, Default)]
struct Book {
    served: BTreeMap<Id, Served>,
    rows: BTreeMap<Id, Row>,
}

/// Bytes moved, and the rate as last read.
struct Meter {
    got: AtomicU64,
    last: Mutex<(Instant, u64, u64)>,
}

impl Meter {
    fn new(got: u64) -> Arc<Self> {
        let last = Mutex::new((Instant::now(), got, 0));
        Arc::new(Self { got: got.into(), last })
    }

    /// Bytes a second since it was last read, a second ago or more.
    fn rate(&self) -> u64 {
        let got = self.got.load(Relaxed);
        let mut l = lock(&self.last);
        let dt = l.0.elapsed().as_secs_f64();
        if dt >= 1.0 {
            *l = (
                Instant::now(),
                got,
                (got.saturating_sub(l.1) as f64 / dt) as u64,
            );
        }
        l.2
    }
}

/// A pull under way: dropped, it stops.
struct Run {
    part: Arc<Part>,
    _task: AbortOnDropHandle<()>,
}

/// The node's side of it all.
#[derive(Default)]
pub(crate) struct Sends {
    book: Mutex<Book>,
    meters: Mutex<HashMap<Id, Arc<Meter>>>,
    runs: Mutex<HashMap<Id, Run>>,
    // Blocks each peer has us sending now.
    busy: Mutex<HashMap<EndpointId, usize>>,
    // Changed since last told (`touch`).
    dirty: Mutex<HashSet<Id>>,
    // A network change: waiting pulls try again now.
    pub(crate) kick: Arc<tokio::sync::Notify>,
}

impl Sends {
    /// Over a week ago: forgotten.
    pub(crate) fn load(dir: &Path) -> Self {
        let mut book: Book = store::load(&dir.join("sends"));
        // Begun a week ago and not under way (over, or never taken):
        // forgotten, and what was sent no longer served.
        let week = now().saturating_sub(7 * 86_400_000);
        let on = |r: &Row| matches!(r.stage, Stage::Waiting | Stage::Running);
        let Book { served, rows } = &mut book;
        rows.retain(|_, r| on(r) || r.at > week);
        served.retain(|id, s| {
            matches!(s.to, To::Space(_)) || rows.contains_key(id)
        });
        Self { book: Mutex::new(book), ..Default::default() }
    }
}

fn with(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(ext);
    s.into()
}

#[cfg(unix)]
fn write_at(f: &std::fs::File, b: &[u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, b, at)
}

#[cfg(unix)]
fn read_at(f: &std::fs::File, b: &mut [u8], at: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, b, at)
}

#[cfg(windows)]
fn write_at(f: &std::fs::File, mut b: &[u8], mut at: u64) -> io::Result<()> {
    while !b.is_empty() {
        match std::os::windows::fs::FileExt::seek_write(f, b, at)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => (b, at) = (&b[n..], at + n as u64),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_at(f: &std::fs::File, mut b: &mut [u8], mut at: u64) -> io::Result<()> {
    while !b.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(f, b, at)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => (b, at) = (&mut b[n..], at + n as u64),
        }
    }
    Ok(())
}

/// When a file was last written: one rewritten since it was offered is
/// not served (nothing hashed it first).
fn stamp(m: &std::fs::Metadata) -> u64 {
    let t = m.modified().ok();
    let t = t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    t.map_or(0, |d| d.as_nanos() as u64)
}

fn opened(s: &Served) -> io::Result<std::fs::File> {
    let f = std::fs::File::open(&s.path)?;
    let m = f.metadata()?;
    if m.len() != s.size || stamp(&m) != s.modified {
        return Err(io::Error::other("changed since it was offered"));
    }
    Ok(f)
}

/// A file coming in: its `.part`, and which of its blocks are in.
struct Part {
    file: std::fs::File,
    path: PathBuf,
    // None once removed: no flush brings it back.
    map: Mutex<Option<PathBuf>>,
    bits: Mutex<Vec<u8>>,
    size: u64,
}

fn has(b: &[u8], i: u64) -> bool {
    b[(i / 8) as usize] >> (i % 8) & 1 == 1
}

impl Part {
    /// `to`'s `.part` (made by `claim`), and its bitmap unless `new`.
    fn open(to: &Path, size: u64, new: bool) -> io::Result<Self> {
        let path = with(to, ".part");
        let file =
            std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let fresh = new || file.metadata()?.len() != size;
        if fresh {
            file.set_len(size)?;
        }
        let map = with(to, ".part.map");
        let n = size.div_ceil(BLOCK).div_ceil(8) as usize;
        let old = std::fs::read(&map).ok().filter(|b| !fresh && b.len() == n);
        let bits = Mutex::new(old.unwrap_or_else(|| vec![0; n]));
        Ok(Self { file, path, map: Mutex::new(Some(map)), bits, size })
    }

    fn blocks(&self) -> u64 {
        self.size.div_ceil(BLOCK)
    }

    /// Where block `i` starts, and how long it is.
    fn span(&self, i: u64) -> (u64, u64) {
        let at = i * BLOCK;
        (at, (self.size - at).min(BLOCK))
    }

    fn left(&self) -> usize {
        let b = lock(&self.bits);
        (0..self.blocks()).filter(|i| !has(&b, *i)).count()
    }

    /// Bytes of the blocks in.
    fn got(&self) -> u64 {
        let b = lock(&self.bits);
        let i = (0..self.blocks()).filter(|i| has(&b, *i));
        i.map(|i| self.span(i).1).sum()
    }

    fn set(&self, i: u64) {
        lock(&self.bits)[(i / 8) as usize] |= 1 << (i % 8);
    }

    /// Its data on disk, then which blocks it holds: a crash loses none.
    fn flush(&self) -> Result<()> {
        let bits = lock(&self.bits).clone();
        self.file.sync_data()?;
        match &*lock(&self.map) {
            Some(p) => store::write(p, &bits),
            None => Ok(()),
        }
    }

    fn remove(&self) {
        if let Some(p) = lock(&self.map).take() {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_file(&self.path);
    }

    /// Whole: on disk, under its name (held for it by `claim`).
    fn done(&self, to: &Path) -> io::Result<()> {
        self.file.sync_all()?;
        let mut m = lock(&self.map);
        std::fs::rename(&self.path, to)?;
        if let Some(p) = m.take() {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }
}

/// A free name for `name` in `dir`, as `save` picks one, held by an empty
/// file until it is whole; its `.part` beside it.
fn claim(dir: &Path, name: &str) -> Result<PathBuf> {
    use std::io::ErrorKind::AlreadyExists;
    std::fs::create_dir_all(dir)?;
    for p in names(dir, name).take(10_000) {
        let part = with(&p, ".part");
        if part.exists() {
            continue;
        }
        match std::fs::File::create_new(&p) {
            Err(e) if e.kind() == AlreadyExists => continue,
            r => drop(r?),
        }
        match std::fs::File::create_new(&part) {
            Ok(_) => return Ok(p),
            Err(e) => {
                let _ = std::fs::remove_file(&p);
                if e.kind() != AlreadyExists {
                    return Err(e.into());
                }
            }
        }
    }
    bail!("no free name for it there")
}

/// Block bytes `at..at + len` of `f` onto `tx`, then their BLAKE3.
async fn give(
    tx: &mut (impl AsyncWrite + Unpin),
    f: Arc<std::fs::File>,
    at: u64,
    len: u64,
) -> Result<()> {
    tx.write_all(&[OK]).await?;
    let (mut h, mut buf, mut off) = (blake3::Hasher::new(), vec![0; SLICE], 0);
    while off < len {
        let k = (len - off).min(SLICE as u64) as usize;
        let f = f.clone();
        (buf, h) = spawn_blocking(move || {
            read_at(&f, &mut buf[..k], at + off)?;
            h.update(&buf[..k]);
            io::Result::Ok((buf, h))
        })
        .await??;
        timeout(STALL, tx.write_all(&buf[..k])).await??;
        off += k as u64;
    }
    tx.write_all(h.finalize().as_bytes()).await?;
    Ok(())
}

/// Why a pull stopped: try again later, or never.
enum Pull {
    Retry,
    Fatal(String),
}

fn retry<E>(_: E) -> Pull {
    Pull::Retry
}

/// Counted bytes of a block, given back unless it came whole.
struct Undo<'a>(&'a AtomicU64, u64);

impl Drop for Undo<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Relaxed);
    }
}

/// Block `i` from `rx` into `part`, hashed as it is written: whether it
/// matches the hash its sender read. `got` counts it as it comes.
async fn take(
    rx: &mut (impl AsyncRead + Unpin),
    part: &Arc<Part>,
    i: u64,
    got: &AtomicU64,
    buf: &mut Vec<u8>,
) -> Result<bool, Pull> {
    let (at, len) = part.span(i);
    let (mut h, mut undo) = (blake3::Hasher::new(), Undo(got, 0));
    while undo.1 < len {
        let off = undo.1;
        let k = (len - off).min(SLICE as u64) as usize;
        buf.resize(SLICE, 0);
        timeout(STALL, rx.read_exact(&mut buf[..k]))
            .await
            .map_err(retry)?
            .map_err(retry)?;
        let (p, b) = (part.clone(), std::mem::take(buf));
        let w = spawn_blocking(move || {
            let r = write_at(&p.file, &b[..k], at + off);
            h.update(&b[..k]);
            (r, b, h)
        });
        let (r, b, hh) = w.await.map_err(|e| Pull::Fatal(e.to_string()))?;
        (*buf, h) = (b, hh);
        r.map_err(|e| Pull::Fatal(format!("writing it: {e}")))?;
        got.fetch_add(k as u64, Relaxed);
        undo.1 += k as u64;
    }
    let mut sum = [0; 32];
    timeout(STALL, rx.read_exact(&mut sum))
        .await
        .map_err(retry)?
        .map_err(retry)?;
    let ok = h.finalize() == blake3::Hash::from(sum);
    if ok {
        undo.1 = 0;
    }
    Ok(ok)
}

enum Got {
    Whole,
    Damaged,
    Busy,
}

/// Block `i`, asked on `c`.
async fn get(
    c: &Connection,
    w: &Work,
    i: u64,
    buf: &mut Vec<u8>,
) -> Result<Got, Pull> {
    let (mut tx, mut rx) = c.open_bi().await.map_err(retry)?;
    let ask = postcard::to_allocvec(&Ask::Get(w.id, i)).expect("ask");
    tx.write_all(&ask).await.map_err(retry)?;
    tx.finish().map_err(retry)?;
    let mut st = [0];
    timeout(STALL, rx.read_exact(&mut st))
        .await
        .map_err(retry)?
        .map_err(retry)?;
    match st[0] {
        OK => match take(&mut rx, &w.part, i, &w.meter.got, buf).await? {
            true => Ok(Got::Whole),
            false => Ok(Got::Damaged),
        },
        BUSY => Ok(Got::Busy),
        GONE => Err(Pull::Fatal("it changed or is gone at its sender".into())),
        _ => Err(Pull::Fatal("its sender no longer offers it".into())),
    }
}

/// A round's blocks to ask for: the next not in yet, or one given back;
/// and how often each came damaged.
struct Work {
    id: Id,
    part: Arc<Part>,
    meter: Arc<Meter>,
    next: Mutex<(u64, Vec<u64>)>,
    damaged: Mutex<HashMap<u64, u8>>,
}

impl Work {
    fn pop(&self) -> Option<u64> {
        let mut n = lock(&self.next);
        if let Some(i) = n.1.pop() {
            return Some(i);
        }
        let b = lock(&self.part.bits);
        let i = (n.0..self.part.blocks()).find(|i| !has(&b, *i))?;
        n.0 = i + 1;
        Some(i)
    }
}

/// One stream's worth: blocks asked on `c` one after another until none
/// is left or `c` fails (its block left for the others).
async fn worker(w: Arc<Work>, c: Connection) -> Result<(), Pull> {
    let mut buf = vec![0; SLICE];
    loop {
        let Some(i) = w.pop() else { return Ok(()) };
        let got = get(&c, &w, i, &mut buf).await;
        let again = || lock(&w.next).1.push(i);
        match got {
            Ok(Got::Whole) => w.part.set(i),
            Ok(Got::Damaged) => {
                let mut d = lock(&w.damaged);
                let n = d.entry(i).or_default();
                *n += 1;
                if *n > TRIES {
                    return Err(Pull::Fatal(
                        "its blocks keep coming damaged".into(),
                    ));
                }
                drop(d);
                warn!("block {i} came damaged: asked again");
                again();
            }
            Ok(Got::Busy) => {
                again();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                again();
                return Err(e);
            }
        }
    }
}

/// `SHAPE`, or `CHATIR_SEND` (`CONNSxSTREAMS`) to measure another.
fn shape() -> (usize, usize) {
    let v = std::env::var("CHATIR_SEND").ok();
    let v = v.as_deref().and_then(|v| v.split_once('x'));
    let v = v.and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    let ok = |&(a, b): &(usize, usize)| {
        (1..=8).contains(&a) && (1..=64).contains(&b)
    };
    v.filter(ok).unwrap_or(SHAPE)
}

/// Pulls `id` until it is whole, fails or is stopped; between tries, a
/// wait that doubles to `PATIENCE`, or less if the network changes.
async fn run(n: Weak<Node>, id: Id, from: EndpointId, part: Arc<Part>) {
    let mut wait = Duration::from_secs(1);
    loop {
        let Some(node) = n.upgrade() else { return };
        let left = part.left();
        match node.round(&id, from, &part).await {
            Ok(()) => return node.finish(&id, &part).await,
            Err(Pull::Fatal(e)) => return node.fail(&id, &e),
            Err(Pull::Retry) if part.left() < left => {
                wait = Duration::from_secs(1)
            }
            Err(Pull::Retry) => {}
        }
        node.stage(&id, Stage::Waiting);
        let kick = node.send.kick.clone();
        drop(node);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = kick.notified() => {}
        }
        wait = (wait * 2).min(PATIENCE);
    }
}

/// One of a peer's blocks being sent; `PER_PEER` at once at most.
struct Slot<'a>(&'a Mutex<HashMap<EndpointId, usize>>, EndpointId);

impl<'a> Slot<'a> {
    fn take(
        m: &'a Mutex<HashMap<EndpointId, usize>>,
        d: EndpointId,
    ) -> Option<Self> {
        let mut b = lock(m);
        let k = b.entry(d).or_default();
        if *k >= PER_PEER {
            return None;
        }
        *k += 1;
        Some(Self(m, d))
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut b = lock(self.0);
        if let Some(k) = b.get_mut(&self.1) {
            *k -= 1;
            if *k == 0 {
                b.remove(&self.1);
            }
        }
    }
}

impl Node {
    fn row(&self, id: &Id) -> Option<Row> {
        lock(&self.send.book).rows.get(id).cloned()
    }

    fn save_book(&self) {
        self.stash("sends", &*lock(&self.send.book));
    }

    /// `id` changed: saved, and told (`touch`).
    fn set(self: &Arc<Self>, id: &Id, f: impl FnOnce(&mut Row)) {
        if let Some(r) = lock(&self.send.book).rows.get_mut(id) {
            f(r);
        }
        self.save_book();
        self.touch(id);
    }

    /// Never out of an end: a late round does not undo a cancel.
    fn stage(self: &Arc<Self>, id: &Id, s: Stage) {
        let now = self.row(id).map(|r| r.stage);
        if now.is_some_and(|n| n != s && !n.over()) {
            self.set(id, |r| r.stage = s);
        }
    }

    fn meter(&self, id: &Id) -> Arc<Meter> {
        let mut m = lock(&self.send.meters);
        m.entry(*id).or_insert_with(|| Meter::new(0)).clone()
    }

    /// `Event::Transfer` for `id`, at most four times a second, the last
    /// change always.
    fn touch(self: &Arc<Self>, id: &Id) {
        let first = {
            let mut d = lock(&self.send.dirty);
            let first = d.is_empty();
            d.insert(*id);
            first
        };
        if first {
            let n = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(TICK).await;
                let Some(n) = n.upgrade() else { return };
                let ids = std::mem::take(&mut *lock(&n.send.dirty));
                ids.into_iter().for_each(|id| n.emit(Event::Transfer { id }));
            });
        }
    }

    /// Whose device `d` is: ours, or a member's in a space we share.
    fn whose(&self, d: &EndpointId) -> Option<Id> {
        if self.devices().iter().any(|x| x.0 == *d) {
            return Some(self.id());
        }
        let of = |s: &Id| self.view(s, |st| st.owner_of(d)).flatten();
        self.spaces().iter().find_map(of)
    }

    /// `user`'s devices we know (in spaces we share), newest first: each
    /// with our name for it and when it last wrote. Not this one.
    pub fn devices_of(
        &self,
        user: &Id,
    ) -> Vec<(EndpointId, Option<String>, u64)> {
        let mut all: BTreeMap<EndpointId, u64> = BTreeMap::new();
        for sp in std::iter::once(self.own()).chain(self.spaces()) {
            self.view(&sp, |s| {
                let theirs = s.devices_of(user).into_iter();
                for (d, t) in theirs.filter(|x| s.owner_of(&x.0) == Some(*user))
                {
                    let at = all.entry(d).or_insert(t);
                    *at = (*at).max(t);
                }
            });
        }
        all.remove(&self.device());
        let mut v: Vec<_> = all.into_iter().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        v.into_iter().map(|(d, t)| (d, self.label(&d), t)).collect()
    }

    /// `path` served from here under a new id, to `to`.
    fn register(
        &self,
        path: &Path,
        name: Option<&str>,
        to: To,
    ) -> Result<(Id, Served)> {
        let path = std::path::absolute(path)?;
        let m = std::fs::metadata(&path)?;
        ensure!(m.is_file(), "not a file");
        let base = path.file_name().and_then(|n| n.to_str());
        let name = short(name.or(base).unwrap_or("file"));
        let s = Served { path, name, size: m.len(), modified: stamp(&m), to };
        let id = Id(crate::crypto::rand());
        lock(&self.send.book).served.insert(id, s.clone());
        self.save_book();
        Ok((id, s))
    }

    /// A chat offer of a big file: served from here, unhashed, to the
    /// space's members (`BIG`).
    pub(crate) fn offer_direct(
        &self,
        space: &Id,
        path: &Path,
        name: Option<&str>,
    ) -> Result<File> {
        let (hash, s) = self.register(path, name, To::Space(*space))?;
        Ok(File { name: s.name, size: s.size, hash, key: DIRECT })
    }

    /// Chat offers served device to device, as `sharing` lists them.
    pub(crate) fn offered_direct(&self) -> Vec<(Id, Id, String, u64)> {
        let b = lock(&self.send.book);
        let on = b.served.iter().filter_map(|(id, s)| match s.to {
            To::Space(sp) => Some((*id, sp, s.name.clone(), s.size)),
            To::Device(_) => None,
        });
        on.collect()
    }

    /// Served no more (`unshare`, `cancel`): whether it was. A device it
    /// was sent to is told.
    pub(crate) fn unserve(self: &Arc<Self>, id: &Id) -> bool {
        let was = lock(&self.send.book).served.remove(id).is_some();
        let r = self.row(id).filter(|r| r.out && !r.stage.over());
        if let Some(r) = r {
            self.stage(id, Stage::Cancelled);
            self.say_later(r.device, Ask::End(*id, End::Cancelled));
        }
        if was {
            self.save_book();
        }
        was
    }

    /// `path` to `device` alone (ours, or a member's we know): served from
    /// here, offered there, if it is reachable now. Its id.
    pub async fn send_to(
        self: &Arc<Self>,
        device: EndpointId,
        path: &Path,
    ) -> Result<Id> {
        ensure!(device != self.device(), "that is this device");
        let none = "not a device of yours or of anyone you share a space with";
        let user = self.whose(&device).context(none)?;
        let (id, s) = self.register(path, None, To::Device(device))?;
        let (out, path) = (true, Some(s.path));
        let row = Row { out, path, ..Row::new(device, user, &s.name, s.size) };
        lock(&self.send.book).rows.insert(id, row);
        self.save_book();
        let offer = Ask::Offer(id, s.name, s.size, self.id());
        let said =
            timeout(Duration::from_secs(15), self.say(device, &offer)).await;
        if let Ok(Ok(r)) = &said
            && r[..] == [OK]
        {
            self.touch(&id);
            return Ok(id);
        }
        let mut b = lock(&self.send.book);
        b.served.remove(&id);
        b.rows.remove(&id);
        drop(b);
        self.save_book();
        match said {
            Ok(Ok(_)) => bail!("that device did not take it"),
            _ => {
                bail!("that device is not reachable now (asleep, or offline?)")
            }
        }
    }

    /// Pull `id`, offered to us, into `dir`: where it will be once whole.
    pub async fn accept(
        self: &Arc<Self>,
        id: &Id,
        dir: &Path,
    ) -> Result<PathBuf> {
        let r = self.row(id).filter(|r| !r.out).context("no such offer")?;
        ensure!(r.stage == Stage::Offered, "already {:?}", r.stage);
        self.begin(id, &r.name, dir)
    }

    pub async fn decline_file(self: &Arc<Self>, id: &Id) -> Result<()> {
        let r = self.row(id).filter(|r| !r.out && r.stage == Stage::Offered);
        let r = r.context("no such offer")?;
        self.stage(id, Stage::Declined);
        self.say_later(r.device, Ask::End(*id, End::Declined));
        Ok(())
    }

    /// Stop `id`, either way; the other side is told, if it can be.
    pub async fn cancel(self: &Arc<Self>, id: &Id) -> Result<()> {
        let r = self.row(id).context("no such transfer")?;
        ensure!(!r.stage.over(), "it is over");
        if r.out {
            self.unserve(id);
            return Ok(());
        }
        self.stop(id);
        self.stage(id, Stage::Cancelled);
        self.say_later(r.device, Ask::End(*id, End::Cancelled));
        Ok(())
    }

    /// Every transfer either way, newest first.
    pub fn transfers(&self) -> Vec<Value> {
        let mut v: Vec<(Id, Row)> = {
            let b = lock(&self.send.book);
            b.rows.iter().map(|(id, r)| (*id, r.clone())).collect()
        };
        v.sort_by_key(|r| std::cmp::Reverse(r.1.at));
        let row = |(id, r): (Id, Row)| {
            let m = self.meter(&id);
            let done = match r.stage {
                Stage::Done => r.size,
                _ => m.got.load(Relaxed).min(r.size),
            };
            let rate = if r.stage == Stage::Running { m.rate() } else { 0 };
            let path = r.path.map(|p| p.display().to_string());
            json!({
                "id": id, "device": r.device, "user": r.user, "name": r.name,
                "size": r.size, "done": done, "rate": rate, "state": r.stage,
                "out": r.out, "path": path, "error": r.error,
            })
        };
        v.into_iter().map(row).collect()
    }

    /// A file sent device to device in `space`, pulled from its poster's
    /// devices (the first that serves it) into `dir`, as `transfers`
    /// shows: where it is once whole.
    pub(crate) async fn save_direct(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        dir: &Path,
    ) -> Result<PathBuf> {
        let id = f.hash;
        if let Some(s) = lock(&self.send.book).served.get(&id) {
            bail!("it is on this device: {}", s.path.display());
        }
        let mut rx = self.subscribe();
        let going = self.row(&id).is_some_and(|r| !r.out && !r.stage.over());
        if !going {
            let who = self.view(space, |s| s.sender_of(&id)).flatten();
            let who = who.context("no such file here")?;
            let ds = self.view(space, |s| s.devices_of(&who));
            let me = self.device();
            let ds: Vec<_> = ds.into_iter().flatten().map(|d| d.0).collect();
            let ds: Vec<_> = ds.into_iter().filter(|d| *d != me).collect();
            self.prove(space, &ds).await;
            let head = &Ask::Head(id);
            let mut asks: FuturesUnordered<_> = (ds.into_iter())
                .map(|d| async move {
                    let r = timeout(Duration::from_secs(8), self.say(d, head));
                    let r = r.await.ok()?.ok()?;
                    let size = [&[OK][..], &f.size.to_le_bytes()].concat();
                    (r == size).then_some(d)
                })
                .collect();
            let mut from = None;
            while let Some(r) = asks.next().await {
                if r.is_some() {
                    from = r;
                    break;
                }
            }
            drop(asks);
            let gone = "nobody who has it is reachable now: it saves once its \
                sender is online";
            let from = from.context(gone)?;
            let name = f.clone().safe().name;
            let space = Some(*space);
            let row = Row { space, ..Row::new(from, who, &name, f.size) };
            lock(&self.send.book).rows.insert(id, row);
            self.begin(&id, &name, dir)?;
        }
        loop {
            let r = self.row(&id).context("cancelled")?;
            match r.stage {
                Stage::Done => return r.path.context("saved nowhere"),
                Stage::Failed => bail!("{}", r.error.unwrap_or_default()),
                Stage::Declined | Stage::Cancelled => bail!("cancelled"),
                _ => {}
            }
            use tokio::sync::broadcast::error::RecvError::Closed;
            if let Err(Closed) = rx.recv().await {
                bail!("shut down");
            }
        }
    }

    /// `name` claimed in `dir` and pulled there from now on.
    fn begin(
        self: &Arc<Self>,
        id: &Id,
        name: &str,
        dir: &Path,
    ) -> Result<PathBuf> {
        let p = claim(dir, name)?;
        self.set(id, |r| (r.stage, r.path) = (Stage::Waiting, Some(p.clone())));
        self.spawn_pull(id, true);
        Ok(p)
    }

    /// `id`'s pull started (or, after a restart, resumed).
    fn spawn_pull(self: &Arc<Self>, id: &Id, new: bool) {
        let Some(r) = self.row(id) else { return };
        let Some(to) = r.path else { return };
        let part = match Part::open(&to, r.size, new) {
            Ok(p) => Arc::new(p),
            Err(e) => return self.fail(id, &format!("its partial file: {e}")),
        };
        lock(&self.send.meters).insert(*id, Meter::new(part.got()));
        // Held while it starts: a pull that ends at once finds its run.
        let mut runs = lock(&self.send.runs);
        let task = tokio::spawn(run(
            Arc::downgrade(self),
            *id,
            r.device,
            part.clone(),
        ));
        runs.insert(*id, Run { part, _task: AbortOnDropHandle::new(task) });
    }

    /// Pulls accepted before a restart go on.
    pub(crate) fn resume_sends(self: &Arc<Self>) {
        let on = |r: &Row| {
            !r.out && matches!(r.stage, Stage::Waiting | Stage::Running)
        };
        let ids: Vec<Id> = {
            let b = lock(&self.send.book);
            b.rows.iter().filter(|r| on(r.1)).map(|r| *r.0).collect()
        };
        ids.iter().for_each(|id| self.spawn_pull(id, false));
    }

    /// Every pull stopped, where it got to noted.
    pub(crate) async fn stop_sends(&self) {
        let runs: Vec<Run> =
            lock(&self.send.runs).drain().map(|r| r.1).collect();
        for Run { part, _task } in runs {
            drop(_task);
            let _ = spawn_blocking(move || part.flush()).await;
        }
    }

    /// One go at pulling `id` from `from`: whole, or why not.
    async fn round(
        self: &Arc<Self>,
        id: &Id,
        from: EndpointId,
        part: &Arc<Part>,
    ) -> Result<(), Pull> {
        if part.left() == 0 {
            return Ok(());
        }
        // A chat offer's sender knows us (again, after a restart).
        if let Some(sp) = self.row(id).and_then(|r| r.space) {
            self.prove(&sp, &[from]).await;
        }
        let (conns, streams) = shape();
        let first =
            timeout(Duration::from_secs(15), self.connect(from, ALPN)).await;
        let first = first.map_err(retry)?.map_err(retry)?;
        // More links to it, now that it is up and found.
        let ep = self.up().await.map_err(retry)?;
        let more = (1..conns).map(|_| async {
            let c =
                timeout(Duration::from_secs(10), ep.connect(from, ALPN)).await;
            c.ok()?.ok()
        });
        let mut cs = vec![first];
        cs.extend(n0_future::join_all(more).await.into_iter().flatten());
        // The device we pull from is the one we chose.
        cs.iter().for_each(irnet::wide);
        self.stage(id, Stage::Running);
        let meter = self.meter(id);
        let w = Arc::new(Work {
            id: *id,
            part: part.clone(),
            meter: meter.clone(),
            next: Mutex::default(),
            damaged: Mutex::default(),
        });
        let mut all = tokio::task::JoinSet::new();
        for c in &cs {
            for _ in 0..streams {
                all.spawn(worker(w.clone(), c.clone()));
            }
        }
        let (mut fatal, mut ticks) = (None, 0u32);
        let mut seen = meter.got.load(Relaxed);
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                r = all.join_next() => match r {
                    None => break,
                    Some(Ok(Err(Pull::Fatal(e)))) => {
                        fatal = Some(e);
                        all.abort_all();
                    }
                    _ => {}
                },
                _ = tick.tick() => {
                    let got = meter.got.load(Relaxed);
                    if got != seen {
                        seen = got;
                        self.touch(id);
                    }
                    ticks += 1;
                    if ticks % FLUSH == 0 {
                        let p = part.clone();
                        let _ = spawn_blocking(move || p.flush()).await;
                    }
                }
            }
        }
        cs.iter().for_each(|c| c.close(0u32.into(), b""));
        let p = part.clone();
        let _ = spawn_blocking(move || p.flush()).await;
        match (part.left(), fatal) {
            (0, _) => Ok(()),
            (_, Some(e)) => Err(Pull::Fatal(e)),
            _ => Err(Pull::Retry),
        }
    }

    async fn finish(self: &Arc<Self>, id: &Id, part: &Arc<Part>) {
        let Some(r) = self.row(id) else { return };
        let Some(to) = r.path else { return };
        let (p, t) = (part.clone(), to.clone());
        match spawn_blocking(move || p.done(&t)).await {
            Ok(Ok(())) => {
                mark(&to);
                self.meter(id).got.store(r.size, Relaxed);
                self.stage(id, Stage::Done);
                self.say_later(r.device, Ask::End(*id, End::Done));
                // Last: it may be this very task.
                lock(&self.send.runs).remove(id);
            }
            Ok(Err(e)) => self.fail(id, &format!("saving it: {e}")),
            Err(e) => self.fail(id, &e.to_string()),
        }
    }

    fn fail(self: &Arc<Self>, id: &Id, e: &str) {
        warn!("receiving a file: {e}");
        self.set(id, |r| {
            if !r.stage.over() {
                (r.stage, r.error) = (Stage::Failed, Some(e.into()));
            }
        });
        self.stop(id);
    }

    /// `id`'s pull stopped; its partial file, bitmap and name gone.
    fn stop(&self, id: &Id) {
        let run = lock(&self.send.runs).remove(id);
        let to = self.row(id).and_then(|r| r.path);
        match (&run, &to) {
            (Some(r), _) => r.part.remove(),
            (None, Some(to)) => {
                let _ = std::fs::remove_file(with(to, ".part.map"));
                let _ = std::fs::remove_file(with(to, ".part"));
            }
            _ => {}
        }
        if let Some(to) =
            to.filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() == 0))
        {
            let _ = std::fs::remove_file(to);
        }
        // Last: it may be this very task.
        drop(run);
    }

    /// One ask to `d`, on a connection of its own: the answer.
    async fn say(&self, d: EndpointId, a: &Ask) -> Result<Vec<u8>> {
        let c = self.connect(d, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let b = sync::rest(&mut rx, 64).await;
        c.close(0u32.into(), b"");
        b
    }

    /// `a` to `d`, in the background, a few seconds at most: one offline
    /// learns it when it next asks.
    fn say_later(&self, d: EndpointId, a: Ask) {
        let Some(n) = self.this() else { return };
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(5), n.say(d, &a)).await;
        });
    }

    /// A connection on `ALPN`: each stream one ask, answered apart.
    pub(crate) async fn lend(self: &Arc<Self>, c: Connection) {
        while let Ok((tx, rx)) = c.accept_bi().await {
            let (n, c) = (self.clone(), c.clone());
            tokio::spawn(async move {
                if let Err(e) = n.asked(&c, tx, rx).await {
                    tracing::debug!("sending a file: {e:#}");
                }
            });
        }
    }

    /// What `from` may have of `id`, checked on every ask.
    async fn lent(&self, id: &Id, from: &EndpointId) -> Option<Served> {
        let s = lock(&self.send.book).served.get(id).cloned()?;
        let ok = match s.to {
            To::Device(d) => d == *from,
            To::Space(sp) => self.member_of(&sp, from).await,
        };
        ok.then_some(s)
    }

    async fn asked(
        self: &Arc<Self>,
        c: &Connection,
        mut tx: SendStream,
        mut rx: RecvStream,
    ) -> Result<()> {
        let from = c.remote_id();
        let ask = timeout(Duration::from_secs(10), sync::rest(&mut rx, 1024))
            .await??;
        let reply = match postcard::from_bytes(&ask)? {
            Ask::Get(id, i) => return self.send_block(c, tx, id, i).await,
            Ask::Head(id) => match self.lent(&id, &from).await {
                Some(s) => {
                    irnet::wide(c);
                    [&[OK][..], &s.size.to_le_bytes()].concat()
                }
                None => vec![NO],
            },
            Ask::Offer(id, name, size, user) => {
                vec![if self.offered(from, id, &name, size, user) {
                    OK
                } else {
                    NO
                }]
            }
            Ask::End(id, e) => {
                self.ended(from, id, e);
                vec![OK]
            }
        };
        tx.write_all(&reply).await?;
        tx.finish()?;
        Ok(())
    }

    /// Block `i` of `id` to `c`'s device, if it may have it.
    async fn send_block(
        self: &Arc<Self>,
        c: &Connection,
        mut tx: SendStream,
        id: Id,
        i: u64,
    ) -> Result<()> {
        let from = c.remote_id();
        let s = self.lent(&id, &from).await;
        let s = s.filter(|s| i < s.size.div_ceil(BLOCK));
        let Some(s) = s else {
            tx.write_all(&[NO]).await?;
            return Ok(tx.finish()?);
        };
        irnet::wide(c);
        let Some(_slot) = Slot::take(&self.send.busy, from) else {
            tx.write_all(&[BUSY]).await?;
            return Ok(tx.finish()?);
        };
        let size = s.size;
        let Ok(f) = spawn_blocking(move || opened(&s)).await? else {
            tx.write_all(&[GONE]).await?;
            return Ok(tx.finish()?);
        };
        // Sent to one device, which has begun.
        let r = self.row(&id);
        if r.is_some_and(|r| {
            r.out && r.device == from && r.stage == Stage::Offered
        }) {
            self.stage(&id, Stage::Running);
        }
        let (at, len) = (i * BLOCK, (size - i * BLOCK).min(BLOCK));
        give(&mut tx, Arc::new(f), at, len).await?;
        tx.finish()?;
        self.meter(&id).got.fetch_add(len, Relaxed);
        self.touch(&id);
        Ok(())
    }

    /// A file offered to us by `from`, a device of `user` (as we know it).
    fn offered(
        self: &Arc<Self>,
        from: EndpointId,
        id: Id,
        name: &str,
        size: u64,
        user: Id,
    ) -> bool {
        let of = |s: &Id| self.view(s, |st| st.owner_of(&from) == Some(user));
        let ours =
            user == self.id() && self.devices().iter().any(|d| d.0 == from);
        if !ours && !self.spaces().iter().any(|s| of(s) == Some(true)) {
            return false;
        }
        let f = File { name: short(name), size, hash: id, key: DIRECT };
        let name = f.safe().name;
        {
            let mut b = lock(&self.send.book);
            let waiting = b
                .rows
                .values()
                .filter(|r| r.device == from && r.stage == Stage::Offered);
            if b.rows.len() >= ROWS
                || waiting.count() >= WAITING
                || b.rows.contains_key(&id)
                || b.served.contains_key(&id)
            {
                return false;
            }
            b.rows.insert(id, Row::new(from, user, &name, size));
        }
        self.save_book();
        self.emit(Event::Incoming { id, device: from, user, name, size });
        self.touch(&id);
        true
    }

    /// `from` says `id` ended: ours to it declined, cancelled or whole;
    /// or theirs to us, cancelled.
    fn ended(self: &Arc<Self>, from: EndpointId, id: Id, e: End) {
        let mut b = lock(&self.send.book);
        let ours = b.served.get(&id).is_some_and(|s| s.to == To::Device(from));
        if ours {
            b.served.remove(&id);
            drop(b);
            if e == End::Done {
                let size = self.row(&id).map_or(0, |r| r.size);
                self.meter(&id).got.store(size, Relaxed);
            }
            let s = match e {
                End::Declined => Stage::Declined,
                End::Cancelled => Stage::Cancelled,
                End::Done => Stage::Done,
            };
            return self.stage(&id, s);
        }
        let r = b.rows.get(&id);
        let theirs =
            r.is_some_and(|r| !r.out && r.device == from && !r.stage.over());
        drop(b);
        if theirs && e == End::Cancelled {
            self.stop(&id);
            self.stage(&id, Stage::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let t = std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos();
        std::env::temp_dir().join(format!("chatir-send-{t}-{name}"))
    }

    /// A block damaged on the way is caught (and its bytes not counted);
    /// asked again, it comes whole.
    #[tokio::test]
    async fn a_damaged_block_is_caught() {
        let len = BLOCK + 12_345;
        let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        let src = tmp("src");
        std::fs::write(&src, &data).unwrap();
        let f = Arc::new(std::fs::File::open(&src).unwrap());
        let dir = tmp("dir");
        let to = claim(&dir, "x.bin").unwrap();
        let part = Arc::new(Part::open(&to, len, true).unwrap());
        assert_eq!((part.blocks(), part.span(1)), (2, (BLOCK, 12_345)));
        let (got, mut buf) = (AtomicU64::new(0), vec![]);
        for flip in [true, false] {
            let (mut a, mut b) = tokio::io::duplex(1 << 16);
            let f = f.clone();
            let sent =
                async move { give(&mut a, f, BLOCK, 12_345).await.unwrap() };
            let mut wire = vec![];
            tokio::join!(sent, b.read_to_end(&mut wire)).1.unwrap();
            if flip {
                wire[1 + 777] ^= 1;
            }
            assert_eq!(wire[0], OK);
            let ok = take(&mut &wire[1..], &part, 1, &got, &mut buf).await;
            assert_eq!(ok.ok(), Some(!flip));
            assert_eq!(got.load(Relaxed), if flip { 0 } else { 12_345 });
        }
        part.set(1);
        assert_eq!((part.left(), part.got()), (1, 12_345));
        let mut back = vec![0; 12_345];
        read_at(&part.file, &mut back, BLOCK).unwrap();
        assert!(back[..] == data[BLOCK as usize..]);
        part.remove();
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_file(src).unwrap();
    }
}
