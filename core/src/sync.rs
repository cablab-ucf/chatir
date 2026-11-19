//! Set reconciliation over one QUIC stream; gossip's safety net.
//! ```text
//! A → B  Hello { space, root, room, who, token, devices, tags }
//!        root: hash of day buckets; room: proof of a key; who: signed
//! B → A  Same | Days(B's buckets) | Out (a key since replaced without
//!        A), then the rekeys since, then an empty frame: A folds them and
//!        judges for itself (unknown space, or not admitted: closed)
//! A → B  A's ids for every day that differs
//! B → A  ids B wants, then the items A lacks, then an empty frame
//! A → B  the items B wants, then an empty frame; B finishes, A closes
//! ```
//! Items are opaque, so storage nodes reconcile too. Fingerprints hash
//! sorted ids, never XOR them; lists are sets, so repeats count once.
//! A member answers only someone holding a key no later epoch cut them
//! off from; a storage node, only devices its owners vouched for.

use crate::Id;
use crate::crypto::{Key, kdf};
use crate::node::{Node, Space, lock};
use anyhow::{Result, anyhow};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{EndpointId, PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALPN: &[u8] = b"spaces/sync/1";
pub type Bucket = (u32, u32, [u8; 16]);
/// A session's whole time: a slow one resumes next time, a stalled one ends.
pub const LIMIT: Duration = Duration::from_secs(300);

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub space: Id,
    pub root: [u8; 32],
    /// The space's gossip topic under the key this side writes with.
    pub room: [u8; 32],
    /// A member, signed for the device asked (storage nodes: none).
    pub who: Option<(Id, Signature)>,
    pub token: Option<[u8; 16]>,
    // With a token: the devices a storage node may take this space from,
    // and the epochs it may keep.
    pub devices: Vec<EndpointId>,
    pub tags: Vec<[u8; 8]>,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Same,
    Out,
    Days(Vec<Bucket>),
}

/// Whom a sync is answered for.
pub enum Admit {
    Yes,
    No,
    /// They hold a key a later epoch left them out of: these rekeys say so.
    Out(Vec<Id>),
}

/// Where a space's members meet under `key`: unknown to anyone without it.
pub fn room(space: &Id, key: &Key) -> [u8; 32] {
    kdf("room", &[&space.0, key])
}

fn said(space: &Id, room: &[u8; 32], to: &EndpointId) -> Vec<u8> {
    [ALPN, &space.0, room, to.as_bytes()].concat()
}

pub fn sign(
    me: &SecretKey,
    space: &Id,
    room: &[u8; 32],
    to: &EndpointId,
) -> (Id, Signature) {
    (Id(*me.public().as_bytes()), me.sign(&said(space, room, to)))
}

/// Who asked, if they signed it for us.
pub fn who(h: &Hello, us: &EndpointId) -> Option<Id> {
    let (u, sig) = h.who.as_ref()?;
    let pk = PublicKey::from_bytes(&u.0).ok()?;
    pk.verify(&said(&h.space, &h.room, us), sig).ok().map(|_| *u)
}

pub fn root(b: &[Bucket]) -> [u8; 32] {
    blake3::hash(&postcard::to_allocvec(b).expect("buckets")).into()
}

pub async fn initiate(
    n: &Arc<Node>,
    sp: &Arc<Space>,
    c: &Connection,
    token: Option<[u8; 16]>,
) -> Result<usize> {
    let (mut tx, mut rx) = c.open_bi().await?;
    let mine = sp.buckets();
    let hello = n.hello(sp, &c.remote_id(), root(&mine), token);
    write(&mut tx, &hello).await?;
    // A bucket a day: a megabyte is a century.
    let theirs = match read(&mut rx, 1 << 20).await? {
        Reply::Same => return Ok(0),
        // Not a word to take: the rekeys, which `locked` judges once folded.
        Reply::Out => {
            take(n, sp, &mut rx).await?;
            anyhow::bail!("told our key was replaced without us");
        }
        Reply::Days(d) => d,
    };
    let mut days: BTreeMap<u32, (Option<Bucket>, Option<Bucket>)> =
        BTreeMap::new();
    mine.into_iter().for_each(|b| days.entry(b.0).or_default().0 = Some(b));
    theirs.into_iter().for_each(|b| days.entry(b.0).or_default().1 = Some(b));
    let diff = days.into_iter().filter(|(_, (a, b))| a != b).map(|(d, _)| d);
    // A peer reads days as u16 (to 2149): one past that would fail it all.
    let diff = diff.filter(|d| *d <= u16::MAX.into());
    let ids: BTreeMap<u32, Vec<Id>> = diff.map(|d| (d, sp.ids(d))).collect();
    write(&mut tx, &ids).await?;
    let want: BTreeSet<Id> = read(&mut rx, 16 << 20).await?;
    let got = take(n, sp, &mut rx).await?;
    give(sp, &mut tx, &want.into_iter().collect::<Vec<_>>()).await?;
    tx.finish()?;
    // Closing now could drop what is in flight: wait for B's end.
    let _ = rx.read_to_end(0).await;
    Ok(got)
}

/// An admitted remote's turn: one session a space each, 32 in all; past
/// that, refused (peers sync again on their next event).
struct Turn<'a>(&'a Mutex<HashSet<(EndpointId, Id)>>, (EndpointId, Id));

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        lock(self.0).remove(&self.1);
    }
}

/// A user's sync turns, counted while they last.
struct Turns<'a>(&'a Mutex<HashMap<Id, usize>>, Id);

impl<'a> Turns<'a> {
    fn take(m: &'a Mutex<HashMap<Id, usize>>, u: Id) -> Option<Self> {
        let mut t = lock(m);
        let k = t.entry(u).or_default();
        (*k < 4).then(|| *k += 1)?;
        Some(Self(m, u))
    }
}

impl Drop for Turns<'_> {
    fn drop(&mut self) {
        let mut t = lock(self.0);
        if let Some(k) = t.get_mut(&self.1) {
            *k -= 1;
            if *k == 0 {
                t.remove(&self.1);
            }
        }
    }
}

pub async fn respond(n: &Arc<Node>, c: &Connection) -> Result<()> {
    // From anyone, before we know the space: small (2,000 devices), and
    // soon. A turn only once admitted: the slow hold up nobody.
    let hello = async {
        let (tx, mut rx) = c.accept_bi().await?;
        let b = unframe(&mut rx, 64 << 10).await?;
        anyhow::Ok((tx, rx, postcard::from_bytes::<Hello>(&b)?))
    };
    let ten = Duration::from_secs(10);
    let (tx, rx, hello) = tokio::time::timeout(ten, hello).await??;
    tokio::time::timeout(LIMIT, session(n, c, tx, rx, hello)).await??;
    let _ = tokio::time::timeout(Duration::from_secs(5), c.closed()).await;
    Ok(())
}

async fn session(
    n: &Arc<Node>,
    c: &Connection,
    mut tx: SendStream,
    mut rx: RecvStream,
    hello: Hello,
) -> Result<()> {
    let (sp, out) = match n.find(&hello, c.remote_id()) {
        (Some(sp), Admit::Yes) => (sp, None),
        (Some(sp), Admit::Out(ids)) => (sp, Some(ids)),
        _ => anyhow::bail!("unknown space"),
    };
    let ((all, each), who) = (&n.talks, (c.remote_id(), sp.id));
    let _all = all.try_acquire().map_err(|_| anyhow!("busy"))?;
    anyhow::ensure!(lock(each).insert(who), "busy");
    let _each = Turn(each, who);
    // 4 at once a user (or a device that says no user): one signer with
    // many endpoint ids holds no more of the 32.
    let user =
        self::who(&hello, &n.device()).unwrap_or(Id(*c.remote_id().as_bytes()));
    let _mine = Turns::take(&n.turns, user).ok_or_else(|| anyhow!("busy"))?;
    if let Some(ids) = out {
        // Told they are out (its rekeys, up to 16 MB) once a minute a
        // space: a stale key from any number of new ids costs no more.
        let fresh = {
            let mut o = lock(&n.outs);
            o.retain(|_, t| t.elapsed() < Duration::from_secs(60));
            o.insert(sp.id, Instant::now()).is_none()
        };
        anyhow::ensure!(fresh, "busy");
        write(&mut tx, &Reply::Out).await?;
        give(&sp, &mut tx, &ids).await?;
        return Ok(tx.finish()?);
    }
    let mine = sp.buckets();
    if root(&mine) == hello.root {
        write(&mut tx, &Reply::Same).await?;
        n.reached(&sp, c.remote_id());
        return Ok(tx.finish()?);
    }
    write(&mut tx, &Reply::Days(mine)).await?;
    let (mut want, mut have): (Vec<Id>, Vec<Id>) = (vec![], vec![]);
    // 500,000 ids at most; days as u16 (to 2149), so however many a peer
    // names, they are 65,536 at most.
    let days: BTreeMap<u16, Vec<Id>> =
        postcard::from_bytes(&unframe(&mut rx, 16 << 20).await?)?;
    for (day, mut theirs) in days {
        theirs.sort_unstable();
        theirs.dedup();
        let ours = sp.ids(day.into());
        let new = theirs.iter().filter(|i| ours.binary_search(i).is_err());
        want.extend(new);
        let lack = |i: &Id| theirs.binary_search(i).is_err();
        have.extend(ours.into_iter().filter(lack));
    }
    write(&mut tx, &want).await?;
    give(&sp, &mut tx, &have).await?;
    take(n, &sp, &mut rx).await?;
    n.reached(&sp, c.remote_id());
    Ok(tx.finish()?)
}

async fn give(sp: &Space, tx: &mut SendStream, ids: &[Id]) -> Result<()> {
    // By bytes, as `take` takes them: 256 of the largest would be 1 GiB.
    let mut rest = ids;
    while !rest.is_empty() {
        let (got, took) = sp.get_some(rest, 16 << 20);
        for b in got {
            frame(tx, &b).await?;
        }
        rest = &rest[took..];
    }
    frame(tx, &[]).await
}

async fn take(
    n: &Arc<Node>,
    sp: &Arc<Space>,
    rx: &mut RecvStream,
) -> Result<usize> {
    let (mut got, mut batch, mut held) = (0, vec![], 0);
    loop {
        let b = unframe(rx, crate::op::MAX_ENV).await?;
        let done = b.is_empty();
        held += b.len();
        if !done {
            batch.push(b);
        }
        // Bytes, not count: 512 of the largest would be 2 GiB.
        if done || held >= 16 << 20 {
            got += n.ingest(sp, std::mem::take(&mut batch), false);
            held = 0;
        }
        if done {
            return Ok(got);
        }
    }
}

async fn write<T: Serialize>(tx: &mut SendStream, v: &T) -> Result<()> {
    frame(tx, &postcard::to_allocvec(v)?).await
}

async fn read<T: DeserializeOwned>(
    rx: &mut RecvStream,
    max: usize,
) -> Result<T> {
    Ok(postcard::from_bytes(&unframe(rx, max).await?)?)
}

pub(crate) async fn frame(tx: &mut SendStream, b: &[u8]) -> Result<()> {
    tx.write_all(&(b.len() as u32).to_be_bytes()).await?;
    Ok(tx.write_all(b).await?)
}

/// The rest of a stream, in order, to `max` bytes: noq 1.4's `read_to_end`
/// reads unordered, which can lose data (n0-computer/noq#867).
pub(crate) async fn rest(rx: &mut RecvStream, max: usize) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut b = Vec::new();
    rx.take(max as u64 + 1).read_to_end(&mut b).await?;
    anyhow::ensure!(b.len() <= max, "over {max} bytes");
    Ok(b)
}

pub(crate) async fn unframe(
    rx: &mut RecvStream,
    max: usize,
) -> Result<Vec<u8>> {
    let mut len = [0; 4];
    rx.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(len <= max, "frame of {len} bytes");
    let mut b = vec![0; len];
    rx.read_exact(&mut b).await?;
    Ok(b)
}
