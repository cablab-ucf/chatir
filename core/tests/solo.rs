//! Android with no storage node: the phone's mailbox (held by pushir's
//! library, here as by another app) is its lane. Members send it labelled
//! events, and a sleeping phone shows what came from its phonebook, no node
//! started. Its relay is not theirs: every frame crosses relays. A process
//! of its own (`push::share` and `PUSHIR_RELAY` are process-wide): every
//! phone here shares one mailbox (each its own lane, all heard as one),
//! and each test takes the frames its spaces label.

use chatir_core::{Ch, Config, Id, Net, Node, Notice, Op, state::DM};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

type Frame = Vec<u8>;

/// What the mailbox heard for chatir, as `Push.kt` hands it to `Wake.ring`.
static RANG: Mutex<Vec<Frame>> = Mutex::new(vec![]);

fn dir(name: &str) -> PathBuf {
    let t = std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos();
    std::env::temp_dir().join(format!("chatir-solo-{t}-{name}"))
}

/// Members' relay (A: every iroh endpoint, their lanes), and the phone's
/// mailbox on another (B, pushir's). Up once for every test.
fn relay() -> Net {
    static AT: OnceLock<String> = OnceLock::new();
    let at = AT.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                use iroh_relay::server::*;
                let up = || {
                    let mut cfg = ServerConfig::default();
                    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
                    Server::spawn(cfg)
                };
                let (a, b) = (up().await.unwrap(), up().await.unwrap());
                let at = format!("http://{}/", a.http_addr().unwrap());
                let on = format!("http://{}/", b.http_addr().unwrap());
                // SAFETY: set once, before any node starts.
                unsafe { std::env::set_var("PUSHIR_RELAY", &on) };
                let key = iroh::SecretKey::generate();
                chatir_core::push::share(&key.public().to_string());
                tokio::spawn(async move {
                    let on = |d: &[u8]| RANG.lock().unwrap().push(d.to_vec());
                    pushir::listen(&pushir::relay().unwrap(), &key, on).await
                });
                tokio::time::sleep(Duration::from_millis(300)).await;
                tx.send(at).unwrap();
                std::future::pending::<()>().await
            })
        });
        rx.recv().unwrap()
    });
    Net::Relay(vec![at.parse().unwrap()])
}

/// Frames taken (`rung`): a ring comes again until acked.
static TAKEN: Mutex<Vec<Vec<u8>>> = Mutex::new(vec![]);

/// What frame `f` says to the phone at `db`: sealed for its lane alone.
fn read(db: &Path, f: &Frame) -> Option<Frame> {
    pushir::open(&chatir_core::push::secret(db).ok()?, f)
}

/// An op's frame for the phone at `db`, not taken yet: one its spaces label.
fn ours(db: &Path, f: &Frame) -> bool {
    read(db, f).is_some_and(|m| {
        matches!(Node::news(db, &m), Some(Some(_)))
            && !TAKEN.lock().unwrap().contains(&m)
    })
}

/// The next message the phone at `db` reads as news.
async fn rung(db: &Path) -> Frame {
    for _ in 0..400 {
        let got = {
            let mut r = RANG.lock().unwrap();
            r.iter().position(|f| ours(db, f)).map(|i| r.remove(i))
        };
        if let Some(f) = got.and_then(|f| read(db, &f)) {
            TAKEN.lock().unwrap().push(f.clone());
            return f;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("not rung");
}

/// Frames so far, gone: what comes next is news.
fn fresh(db: &Path) {
    RANG.lock().unwrap().retain(|f| !ours(db, f));
}

/// What a sleeping phone shows for a frame, and whether its node is needed.
fn woken(db: &Path, f: &Frame) -> (Option<Notice>, bool) {
    Node::woken(db, &Node::news(db, f).flatten().expect("news"))
}

/// A phone (its lane the shared device), or a member with a lane of its
/// own (a desktop's, on the same relay).
async fn node(dir: &Path, name: &str, phone: bool) -> Arc<Node> {
    let net = relay();
    if !phone {
        std::fs::create_dir_all(dir).unwrap();
        chatir_core::push::ours(dir, &net, false).await.unwrap();
    }
    let (dir, name) = (dir.into(), name.into());
    let (link, serve, notes, big, inbox) = (None, false, None, None, None);
    Node::start(Config { dir, name, net, link, serve, notes, big, inbox })
        .await
        .unwrap()
}

async fn pair() -> (Arc<Node>, PathBuf, Arc<Node>) {
    let db = dir("b");
    let a = node(&dir("a"), "a", false).await;
    (a, db.clone(), node(&db, "b", true).await)
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..800 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `a` has `b`'s lane in space `id` (no push server: the shared device),
/// as loud as `how` says, and a lane of its own to send from.
fn told(a: &Node, id: &Id, b: &Node, how: &str) -> bool {
    let p = a.view(id, |s| s.pushes_of(Some(&b.id()))).unwrap_or_default();
    let lane = |u: &String| u.contains("pushir:") && u.contains(how);
    chatir_core::push::lane_at(a.dir()).is_some()
        && p.iter().any(|p| lane(&p.1))
}

fn general(n: &Node, id: &Id) -> Ch {
    n.view(id, |s| s.channels()[0].0).unwrap()
}

fn texts(n: &Node, id: &Id, ch: &Ch) -> Vec<String> {
    let m = n.view(id, |s| s.messages(ch, None, 100)).unwrap_or_default();
    m.into_iter().map(|m| m.text).collect()
}

fn post(ch: Ch, text: &str, reply: Option<Id>) -> Op {
    Op::Post { channel: ch, text: text.into(), reply, files: vec![] }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ring_tells_a_sleeping_phone_what_rang() {
    let (a, db, b) = pair().await;
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    let both = || told(&a, &id, &b, "") && told(&a, &dm, &b, "");
    until("a has b's lane in both", both).await;
    b.notices(); // asked once: what comes next is news
    b.shutdown().await;
    drop(b);

    fresh(&db);
    a.publish(&dm, post(DM, "lunch?", None)).await.unwrap();
    let said = rung(&db).await;
    a.call(Some((dm, DM))).await.unwrap();
    let call = rung(&db).await;
    a.call(None).await.unwrap();
    let ended = rung(&db).await;

    // Up again with a away (else a tells b at once it is in no call), b
    // tells of both: they can only have come with the frames, heard as
    // `Wake.ring` hands them to a running node.
    a.shutdown().await;
    let b = node(&db, "b", true).await;
    let rings = |n: &Node| n.notices().into_iter().filter(|n| n.ring).count();
    assert_eq!(rings(&b), 0, "not yet");
    b.hear(&said);
    b.hear(&call);
    let n = b.notices();
    assert!(n.iter().any(|n| n.ring), "the call rings: {n:?}");
    b.hear(&ended);
    assert!(b.calls(&dm).is_empty(), "and stops when hung up");
    until("the message, told", || {
        texts(&b, &dm, &DM).contains(&"lunch?".to_string())
    })
    .await;
}

/// A call to a sleeping phone shows the moment its frame comes, from what
/// its node left at its nap: no node starts first. Hung up, it stops.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_shows_on_a_sleeping_phone_at_once() {
    let (a, db, b) = pair().await;
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    until("a has b's lane in the DM", || told(&a, &dm, &b, "")).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    fresh(&db);
    let t = std::time::Instant::now();
    a.call(Some((dm, DM))).await.unwrap();
    let (n, node_needed) = woken(&db, &rung(&db).await);
    let (n, ms) = (n.expect("shown at once"), t.elapsed().as_millis());
    assert!(!node_needed, "no node needed");
    assert!(ms < 2000, "{ms} ms");
    assert!(n.ring && !n.hush, "rings");
    assert_eq!((n.space, n.room), (dm, DM));
    assert_eq!((&n.title[..], &n.body[..]), ("a", "is calling you"));

    a.call(None).await.unwrap();
    assert!(woken(&db, &rung(&db).await).0.expect("hushed").hush);
}

/// Unacked (a sleeping phone acks nothing), a ring comes again for a
/// minute: one lost on the way, the next gets through. Shown once, and a
/// call rung once (declined, it would ring again).
#[tokio::test(flavor = "multi_thread")]
async fn a_ring_comes_again_and_shows_once() {
    let (a, db, b) = pair().await;
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    until("a has b's lane in the DM", || told(&a, &dm, &b, "")).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    fresh(&db);
    a.publish(&dm, post(DM, "lunch?", None)).await.unwrap();
    let said = rung(&db).await;
    a.call(Some((dm, DM))).await.unwrap();
    let call = rung(&db).await;
    for f in [&said, &call] {
        until("again", || {
            RANG.lock()
                .unwrap()
                .iter()
                .any(|g| read(&db, g).as_ref() == Some(f))
        })
        .await;
    }
    assert_eq!(woken(&db, &said).0.expect("shown").body, "lunch?");
    assert!(woken(&db, &call).0.expect("rings").ring);
    assert!(matches!(woken(&db, &said), (None, false)), "shown once");
    assert!(matches!(woken(&db, &call), (None, false)), "rung once");
}

/// A phone with no storage node, its device alone: a message reaches it as
/// one event, shows from it alone and is kept unopened; once b starts
/// again, it is there before any peer is, and not told twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_rings_a_solo_phone_by_event_and_keeps() {
    let (a, db, b) = pair().await;
    let id = a.create("hub").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    until("a has b's lane", || told(&a, &id, &b, "")).await;
    // Whom its phonebook names: else the node starts, to learn who.
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    fresh(&db);
    a.publish(&id, post(ch, "lunch?", None)).await.unwrap();
    let (n, node_needed) = woken(&db, &rung(&db).await);
    let n = n.expect("shown at once");
    assert_eq!((&n.title[..], &n.body[..]), ("a in #general (hub)", "lunch?"));
    assert!(!n.ring && n.room == ch && !node_needed, "kept, no node needed");

    a.shutdown().await;
    drop(a);
    let b = node(&db, "b", true).await;
    assert_eq!(texts(&b, &id, &ch), ["lunch?"], "from its own log");
    assert!(b.notices().is_empty(), "told once");
}

/// A DM made while b's phone sleeps: its lane takes the knock, it takes
/// the DM, sleeps again; opened later, b and a meet in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dm_taken_asleep_connects_later() {
    let (a, db, b) = pair().await;
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    until("a has b's lane", || told(&a, &id, &b, "")).await;
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let b_id = b.id();
    b.shutdown().await;
    drop(b);
    // Asleep: a opens a DM; b's lane takes the knock.
    fresh(&db);
    let dm = a.dm(&id, &b_id).await.unwrap();
    let knock = rung(&db).await;
    // Woken for a moment, as Android's service does, then asleep again.
    let b = node(&db, "b", true).await;
    b.hear(&knock);
    until("b takes the DM", || b.spaces().contains(&dm)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    b.shutdown().await;
    drop(b);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Opened: both in the DM.
    let b = node(&db, "b", true).await;
    until("b gossips in the DM", || b.peers(&dm) > 0).await;
    until("a gossips in the DM", || a.peers(&dm) > 0).await;
}

/// On "mentions only", a sleeping phone is sent a reply to one of its
/// messages, no @name needed, and nothing at all for the rest.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_to_you_is_a_mention_while_asleep() {
    let (a, db, b) = pair().await;
    let id = a.create("hub").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let mine = b.publish(&id, post(ch, "anyone up?", None)).await.unwrap();
    until("a has it", || texts(&a, &id, &ch) == ["anyone up?"]).await;
    b.set_level(&id, chatir_core::Level::Mentions);
    until("a has b's lane", || told(&a, &id, &b, "level:mentions")).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);
    fresh(&db);
    a.publish(&id, post(ch, "morning all", None)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let rang = RANG.lock().unwrap().iter().any(|f| ours(&db, f));
    assert!(!rang, "nothing sent for the rest");
    a.publish(&id, post(ch, "me!", Some(mine))).await.unwrap();
    let n = woken(&db, &rung(&db).await).0.expect("to b: told");
    assert_eq!(n.body, "me!");
}

/// A text from one of our devices reaches our phone with its node down at
/// once, by its lane: shown from the phonebook (our own space's key), no
/// node started, acked back, so the sender knows within seconds. Again (a
/// retry, or the same text directly too) it shows nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_text_reaches_our_sleeping_phone_at_once() {
    let a = node(&dir("ta"), "laptop", false).await;
    let db = dir("tb");
    let link = Some(a.link_code().await);
    let cfg = Config {
        dir: db.clone(),
        name: "phone".into(),
        net: relay(),
        link,
        serve: false,
        notes: None,
        big: None,
        inbox: None,
    };
    let b = Node::start(cfg).await.unwrap();
    let dev = b.device();
    let own = a.own();
    let lane = || {
        let p = a.view(&own, |s| s.pushes_of(None)).unwrap_or_default();
        p.iter().any(|(d, u)| *d == dev && u.contains("pushir:"))
    };
    until("the laptop has the phone's lane", lane).await;
    b.shutdown().await;
    drop(b);
    assert!(db.join("phonebook").exists(), "left as it slept");
    let to = a.clone();
    let t = std::time::Instant::now();
    let sent = tokio::spawn(async move { to.hand(dev, "on its way").await });
    // What the phone's mailbox hands Push.kt, read with no node running.
    let mut shown = None;
    for _ in 0..400 {
        let frames: Vec<Frame> = RANG.lock().unwrap().clone();
        for f in frames.iter().filter_map(|f| read(&db, f)) {
            if let Some(n) = Node::text(&db, &f).await {
                shown = Some((n, f));
            }
        }
        if shown.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (n, f) = shown.expect("shown from the push alone");
    let from = a.label(&a.device()).unwrap();
    assert_eq!((n.title, n.body.as_str()), (from, "on its way"), "by name");
    let taken = tokio::time::timeout(Duration::from_secs(15), sent).await;
    assert!(taken.expect("acked within seconds").unwrap().unwrap(), "taken");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert!(Node::text(&db, &f).await.is_none(), "shown once");
}
