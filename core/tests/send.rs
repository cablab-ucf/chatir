//! Files sent device to device: whole, only between devices allowed (as
//! ssh's keys), resumed.

use chatir_core::{Config, Event, Id, Net, Node, Op};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn dir(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos();
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("chatir-send-{t}-{n}-{name}"))
}

/// Files sent it land in DIR/inbox.
async fn start(dir: &Path, name: &str) -> Arc<Node> {
    let (net, link, notes, big) = (Net::Direct, None, None, None);
    let (inbox, dir, name) = (Some(dir.join("inbox")), dir.into(), name.into());
    let serve = false;
    let cfg = Config { dir, name, net, link, serve, notes, big, inbox };
    Node::start(cfg).await.unwrap()
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..2400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `n`'s transfer `id`, as the page sees it.
fn row(n: &Node, id: &Id) -> Value {
    let id = serde_json::json!(id);
    n.transfers().into_iter().find(|r| r["id"] == id).unwrap_or_default()
}

/// Sparse, `len` long, but each MiB different.
fn sparse(len: u64) -> PathBuf {
    use std::io::{Seek, SeekFrom, Write};
    let p = dir("src.bin");
    let mut f = std::fs::File::create(&p).unwrap();
    f.set_len(len).unwrap();
    for at in (0..len).step_by(1 << 20) {
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&at.to_le_bytes()[..(len - at).min(8) as usize]).unwrap();
    }
    p
}

fn hash(p: &Path) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(p).unwrap()).unwrap();
    h.finalize()
}

/// `a` and `b` in a space, each knowing and allowing the other's device.
async fn pair() -> (Arc<Node>, Arc<Node>, Id, PathBuf) {
    let bdir = dir("b");
    let (a, b) = (start(&dir("a"), "a").await, start(&bdir, "b").await);
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let knows = |x: &Node, y: &Node| {
        x.view(&id, |s| s.owner_of(&y.device()).is_some()) == Some(true)
    };
    until("each knows the other", || knows(&a, &b) && knows(&b, &a)).await;
    a.allow(b.device(), Some("b")).unwrap();
    b.allow(a.device(), Some("a")).unwrap();
    (a, b, id, bdir)
}

/// Where `id` is coming in, at `n`.
fn path(n: &Node, id: &Id) -> PathBuf {
    row(n, id)["path"].as_str().unwrap().into()
}

async fn offered(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Id {
    loop {
        if let Ok(Event::Incoming { id, .. }) = rx.recv().await {
            return id;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_sent_to_a_device_arrives_whole() {
    let (a, b, _, bdir) = pair().await;
    let src = sparse((40 << 20) + 12_345);
    let mut rx = b.subscribe();
    let id = a.send_to(b.device(), &src, None).await.unwrap();
    // Taken at once, as scp's: nobody asked.
    assert_eq!(offered(&mut rx).await, id);
    assert_eq!(row(&b, &id)["user"], serde_json::json!(a.id()));
    until("b has it", || row(&b, &id)["state"] == "done").await;
    let p = path(&b, &id);
    assert_eq!(p.parent(), Some(&*bdir.join("inbox")));
    assert_eq!(hash(&p), hash(&src), "whole");
    let left = std::fs::read_dir(bdir.join("inbox")).unwrap().count();
    assert_eq!(left, 1, "no .part left");
    until("a hears it is done", || row(&a, &id)["state"] == "done").await;
    assert!(a.sharing().is_empty(), "no longer served");

    // Cancelled by its sender on the way: b's side stops too.
    let big = sparse(512 << 20);
    let id = a.send_to(b.device(), &big, None).await.unwrap();
    assert_eq!(offered(&mut rx).await, id);
    a.cancel(&id).await.unwrap();
    until("b hears it", || row(&b, &id)["state"] == "cancelled").await;
    std::fs::remove_file(src).unwrap();
    std::fs::remove_file(big).unwrap();
}

/// Device to device, only between devices allowed: a space in common is
/// not enough, nor one side's word. A chat offer goes to members alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_not_allowed_is_refused() {
    let (a, b, id, _) = pair().await;
    let x = start(&dir("x"), "x").await;
    let src = sparse(1 << 20);
    let e = a.send_to(x.device(), &src, None).await.unwrap_err();
    assert!(e.to_string().contains("allowed"), "{e}");
    assert!(x.send_to(b.device(), &src, None).await.is_err(), "x knows nobody");
    // x joins, is shown an offer, and is banned: a refuses it, b saves it.
    x.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let knows = || a.view(&id, |s| s.owner_of(&x.device()).is_some());
    until("a knows x", || knows() == Some(true)).await;
    // x allows b; b has not allowed x: refused, so x's waits, untaken.
    let e = x.send_to(b.device(), &src, None).await.unwrap_err();
    assert!(e.to_string().contains("allowed"), "a space is not enough: {e}");
    // As ssh's file: comments kept, any blank between key and name, and
    // a name on one line.
    let f = x.dir().join("allowed");
    std::fs::write(&f, format!("# lab\n{}\tb's laptop\n", b.device())).unwrap();
    assert_eq!(x.allowed()[&b.device()], "b's laptop");
    x.allow(b.device(), Some("b\nevil")).unwrap();
    assert_eq!(x.allowed()[&b.device()], "b evil");
    let text = std::fs::read_to_string(&f).unwrap();
    assert!(text.starts_with("# lab\n") && text.lines().count() == 2, "{text}");
    let sent = x.send_to(b.device(), &src, None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(row(&x, &sent)["state"], "waiting");
    assert!(row(&b, &sent).is_null(), "b never took it");
    let big = sparse((1 << 30) + 1);
    let f = a.offer(&id, &big, None).await.unwrap();
    assert!(f.direct() && f.size == (1 << 30) + 1);
    let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
    let files = vec![f.clone()];
    let post =
        Op::Post { channel: ch, text: String::new(), reply: None, files };
    a.publish(&id, post).await.unwrap();
    let seen =
        |n: &Node| n.view(&id, |s| s.sender_of(&f.hash)) == Some(Some(a.id()));
    until("x sees the post", || seen(&x)).await;
    a.ban(&id, &x.id(), true).await.unwrap();
    let out = dir("x-out");
    let e = x.save(&id, &f, &out).await.unwrap_err();
    assert!(e.to_string().contains("nobody"), "{e}");
    assert_eq!(std::fs::read_dir(&out).map_or(0, |d| d.count()), 0);
    assert!(a.sharing().iter().any(|s| s.0 == f.hash && s.1 == id));
    std::fs::remove_file(src).unwrap();
    std::fs::remove_file(big).unwrap();
}

/// Stopped mid-way (its node shut down), a transfer goes on where it was
/// once that node starts again.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_transfer_resumes() {
    // Few streams: blocks finish early, so there is something to resume
    // (any other test here may take it too: it only slows them).
    unsafe { std::env::set_var("CHATIR_SEND", "2x4") };
    let (a, b, _, bdir) = pair().await;
    let src = sparse(512 << 20);
    let mut rx = b.subscribe();
    let id = a.send_to(b.device(), &src, None).await.unwrap();
    assert_eq!(offered(&mut rx).await, id);
    let p = path(&b, &id);
    let done = |n: &Node| row(n, &id)["done"].as_u64().unwrap_or(0);
    until("some of it", || done(&b) >= 192 << 20).await;
    b.shutdown().await;
    drop(b);
    let map = PathBuf::from(format!("{}.part.map", p.display()));
    let bits = std::fs::read(&map).unwrap();
    assert!(bits.iter().any(|b| *b != 0) && bits.iter().any(|b| *b != 0xff));
    let b = start(&bdir, "b").await;
    let r = row(&b, &id);
    assert!(r["done"].as_u64().unwrap() >= 64 << 20, "kept what it had: {r}");
    until("b has it all", || row(&b, &id)["state"] == "done").await;
    assert_eq!(hash(&p), hash(&src), "whole");
    assert!(!map.exists());
    std::fs::remove_file(src).unwrap();
}

/// A chat offer from 64 MiB is served device to device, nothing hashed
/// first: another member saves it, and it shows as a transfer.
#[tokio::test(flavor = "multi_thread")]
async fn a_big_chat_offer_is_saved_device_to_device() {
    let (a, b, id, _) = pair().await;
    let big = sparse((1 << 30) + 4321);
    let f = a.offer(&id, &big, Some("disk.img")).await.unwrap();
    assert!(f.direct() && f.name == "disk.img");
    let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
    let files = vec![f.clone()];
    let post =
        Op::Post { channel: ch, text: "here".into(), reply: None, files };
    a.publish(&id, post).await.unwrap();
    let seen = || b.view(&id, |s| s.sender_of(&f.hash)) == Some(Some(a.id()));
    until("b sees the post", seen).await;
    assert!(b.file(&id, &f).await.is_err(), "no preview");
    let out = dir("out");
    let p = b.save(&id, &f, &out).await.unwrap();
    assert_eq!(p, out.join("disk.img"));
    assert_eq!(hash(&p), hash(&big), "whole");
    let r = row(&b, &f.hash);
    assert_eq!(
        (r["state"].as_str(), r["out"].as_bool()),
        (Some("done"), Some(false))
    );
    a.unshare(&f.hash).await.unwrap();
    assert!(a.sharing().is_empty());
    std::fs::remove_file(big).unwrap();
    std::fs::remove_dir_all(out).unwrap();
}
