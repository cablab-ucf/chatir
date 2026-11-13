//! How fast a file goes device to device on this machine (loopback), for
//! a few shapes (connections × streams each); with `TMPDIR=/dev/shm` the
//! link is measured, not the disk.
//! `cargo test --release -p chatir-core --test speed -- --ignored --nocapture`

use chatir_core::{Config, Event, Net, Node};
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn start(name: &str) -> Arc<Node> {
    let t = std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("chatir-speed-{t}-{name}"));
    let (net, link, notes, big, serve) = (Net::Direct, None, None, None, false);
    let name = name.into();
    let cfg = Config { dir, name, net, link, serve, notes, big };
    Node::start(cfg).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn speed() {
    let gb = std::env::var("CHATIR_SPEED_GB").ok();
    let size = gb.and_then(|g| g.parse().ok()).unwrap_or(2u64) << 30;
    let shapes = std::env::var("CHATIR_SHAPES").ok();
    let shapes = shapes.as_deref().unwrap_or("1x8 2x8 4x8 4x16 8x8 8x16");
    let (a, b) = (start("a").await, start("b").await);
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let knows = || a.view(&id, |s| s.owner_of(&b.device()).is_some());
    while knows() != Some(true) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Sparse: read from memory, so the receiving side is what is timed.
    let src = std::env::temp_dir().join("chatir-speed-src");
    std::fs::File::create(&src).unwrap().set_len(size).unwrap();
    let out = std::env::temp_dir().join("chatir-speed-out");
    let mut rx = b.subscribe();
    for shape in shapes.split_whitespace() {
        unsafe { std::env::set_var("CHATIR_SEND", shape) };
        let t = a.send_to(b.device(), &src).await.unwrap();
        while !matches!(rx.recv().await, Ok(Event::Incoming { id, .. }) if id == t)
        {
        }
        let at = Instant::now();
        let p = b.accept(&t, &out).await.unwrap();
        let id = serde_json::json!(t);
        loop {
            let r = b.transfers().into_iter().find(|r| r["id"] == id).unwrap();
            assert!(r["state"] != "failed", "{r}");
            if r["state"] == "done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let rate = size as f64 * 8e-9 / at.elapsed().as_secs_f64();
        println!("{shape:>5}: {rate:.2} Gbit/s");
        std::fs::remove_file(p).unwrap();
    }
    std::fs::remove_file(src).unwrap();
    let _ = std::fs::remove_dir_all(out);
}
