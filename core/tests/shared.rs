//! Android: pushir's library holds the phone's mailbox for every app (its
//! owner may be another app); chatir is told its id, registers it with its
//! storage node, and rings reach it there. A process of its own:
//! `push::share` and `PUSHIR_RELAY` are process-wide.

use chatir_core::{Config, Net, Node, Op};
use std::sync::Arc;
use std::time::Duration;

fn dir(name: &str) -> std::path::PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    let t = t.unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("chatir-shared-{t}-{name}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn start(dir: &std::path::Path, name: &str, serve: bool) -> Arc<Node> {
    let cfg = Config {
        dir: dir.into(),
        name: name.into(),
        net: Net::Direct,
        link: None,
        serve,
        notes: None,
        big: None,
    };
    Node::start(cfg).await.unwrap()
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

#[tokio::test(flavor = "multi_thread")]
async fn a_storage_node_rings_the_phones_mailbox() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let at = format!("http://{}/", relay.http_addr().unwrap());
    // SAFETY: read as nodes start and send; nothing else here does.
    unsafe { std::env::set_var("CHATIR_PUSH_RELAY", &at) };
    unsafe { std::env::set_var("PUSHIR_RELAY", &at) };

    // The platform: pushir's library holds the mailbox, hears chatir's lane.
    let db = dir("b");
    let secret = chatir_core::push::secret(&db).unwrap();
    let key = iroh::SecretKey::generate();
    chatir_core::push::share(&key.public().to_string());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let on = move |f: &[u8]| {
            pushir::open(&secret, f).into_iter().for_each(|m| drop(tx.send(m)))
        };
        pushir::listen(&pushir::relay().unwrap(), &key, on).await
    });

    let s = start(&dir("s"), "s", true).await;
    let (a, b) =
        (start(&dir("a"), "a", false).await, start(&db, "b", false).await);
    let id = a.create("hub").await.unwrap();
    let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    b.add_storage(&s.storage_code().await.unwrap()).await.unwrap();
    let url = b.pushir().await.expect("taken by its storage node");
    assert!(url.starts_with("pushir:"), "{url}");
    b.set_push(Some(url)).await;
    assert!(b.pushing(), "its storage node took the route");
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("chatir:ring:");
    let told = || a.view(&id, |s| s.pushes_of(None).iter().any(ring));
    until("a knows b's push server", || told() == Some(true)).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    let op =
        Op::Post { channel: ch, text: "hi".into(), reply: None, files: vec![] };
    a.publish(&id, op).await.unwrap();
    let shown = async {
        loop {
            let got = rx.recv().await.unwrap();
            let Some(news) = Node::news(&db, &got).flatten() else { continue };
            if let Some(n) = Node::woken(&db, &news).0.filter(|n| !n.hush) {
                return n;
            }
        }
    };
    let n = tokio::time::timeout(Duration::from_secs(10), shown).await;
    let n = n.expect("pushed within seconds");
    assert_eq!((n.space, &n.body[..]), (id, "hi"));
}
