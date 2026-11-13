//! `--web`: the app's page, served by this node to its owner's browser,
//! keys staying here. Calls and uploads are POSTs to `api/…`; events and
//! call frames stream down as `[len u32][bytes]`, and only while the page
//! is in sight. With `--web-link`, the same HTTP rides iroh as irp's
//! irweb/http/2, so its page (`LINK#<id>;t=<token>`) opens this from any
//! browser, with no open port or certificate (and that page holds the
//! token). `api/` wants the token (`irweb-access`), fresh each run; the
//! page is public, as its source is.

use anyhow::{Context, Result, bail, ensure};
use chatir_core::{Node, api, crypto};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use iroh::{RelayUrl, SecretKey};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::{net::SocketAddr, path::Path, path::PathBuf, sync::Arc, sync::Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, broadcast, broadcast::error::RecvError};
use tokio::time::{Duration, timeout};

const UI: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/ui.rs"));
/// irp's page: it dials the relays irnet homes on when a link pins none.
const LINK: &str = "https://cablab-ucf.github.io/";
const ALPN: &[u8] = b"irweb/http/2";
const HEAD: usize = 16 << 10;
const BODY: usize = 8 << 20;
const CSP: &str = "default-src 'self'; img-src 'self' data:; media-src \
    'self' blob:; style-src 'self'; object-src 'none'; frame-ancestors \
    'none'; base-uri 'none'; form-action 'none'";

struct Web {
    n: Arc<Node>,
    token: String,
    up: PathBuf,
    /// Files saved for the page, each behind a one-time link (`save`).
    out: PathBuf,
    saved: Mutex<HashMap<String, PathBuf>>,
    /// Where files sent to this node come (`accept`).
    got: PathBuf,
    news: broadcast::Sender<Arc<str>>,
    cap: Semaphore,
}

/// `relays`: None, no link (LAN only); empty, irp's own.
pub async fn run(
    n: Arc<Node>,
    dir: PathBuf,
    addr: Option<SocketAddr>,
    relays: Option<Vec<RelayUrl>>,
) -> Result<()> {
    let Some(addr) = addr else { return std::future::pending().await };
    // Fresh each run, as is the link's key (not the node's: a link says
    // nothing of who you are): a run's links die with it, and no relay
    // ties one run to the next.
    let token = HEXLOWER.encode(&crypto::rand::<32>());
    let (news, _) = broadcast::channel(256);
    let (up, out) = (dir.join("offered"), dir.join("saving"));
    let _ = std::fs::remove_dir_all(&out); // the last run's, unfetched
    let saved = Mutex::default();
    let cap = Semaphore::new(256);
    let got = dir.join("received");
    let w = Arc::new(Web { n, token, up, out, saved, got, news, cap });
    tokio::spawn(forward(w.clone()));
    if !addr.ip().is_loopback() {
        eprintln!(
            "chatir: {addr} is not this machine alone: the page and its \
             token cross that network in the clear"
        );
    }
    let tcp = tokio::net::TcpListener::bind(addr).await?;
    let mut said = format!("http://{addr}/#{}\n", w.token);
    let ep = match relays {
        Some(r) => {
            let pin = r.first().map(|u| format!(";r={u}")).unwrap_or_default();
            // irnet's endpoint, as the node's: on the pinned relay, else
            // the lab's (n0's only while that is down), where the page dials.
            let lab = r.is_empty();
            let ep = chatir_core::irnet::on(&r[..r.len().min(1)])
                .secret_key(SecretKey::from_bytes(&crypto::rand()))
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await?;
            if lab {
                tokio::spawn(chatir_core::irnet::fallback(ep.clone()));
            }
            let id = BASE32_NOPAD.encode(ep.id().as_bytes()).to_lowercase();
            said += &format!("{LINK}#{id};t={}{pin}\n", w.token);
            Some(ep)
        }
        None => None,
    };
    // Each opens your chatir: printed to a terminal, not to a service's log.
    std::fs::write(dir.join("web"), &said)?;
    match std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        true => print!("Open chatir in a browser (keep these secret):\n{said}"),
        false => {
            println!("web: its links are in {}", dir.join("web").display())
        }
    }
    let link = async {
        let Some(ep) = ep else { return std::future::pending().await };
        while let Some(inc) = ep.accept().await {
            let w = w.clone();
            tokio::spawn(async move {
                let Ok(c) = inc.await else { return };
                while let Ok((o, r)) = c.accept_bi().await {
                    tokio::spawn(exchange(w.clone(), r, o, false));
                }
            });
        }
    };
    tokio::select! {
        () = link => Ok(()),
        r = async {
            loop {
                let (s, _) = tcp.accept().await?;
                let (r, o) = s.into_split();
                tokio::spawn(exchange(w.clone(), r, o, true));
            }
        } => r,
    }
}

/// Core events as the app gets them, and its notices as "alert"s.
async fn forward(w: Arc<Web>) {
    use chatir_core::Event::{Call, Changed};
    let mut rx = w.n.subscribe();
    let say = |event, payload| {
        let _ = w.news.send(
            json!({ "event": event, "payload": payload }).to_string().into(),
        );
    };
    loop {
        match rx.recv().await {
            Ok(e) => {
                if matches!(e, Changed { .. } | Call { .. }) {
                    for x in w.n.notices() {
                        say("alert", json!(x));
                    }
                }
                say("chatir", json!(e));
            }
            Err(RecvError::Lagged(_)) => {
                say("chatir", json!({ "t": "changed" }))
            }
            Err(RecvError::Closed) => break,
        }
    }
}

struct Req {
    method: String,
    path: String,
    head: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn get(&self, k: &str) -> Option<&str> {
        self.head.iter().find(|h| h.0 == k).map(|h| h.1.as_str())
    }

    /// `api/…`'s command, if that is what it asks.
    fn api(&self) -> Option<&str> {
        self.path.split('?').next().unwrap_or_default().strip_prefix("/api/")
    }

    /// Its token (and, from a browser here, our own page): only then is
    /// its body read and its command run.
    fn allowed(&self, token: &str, tcp: bool) -> bool {
        // Another site's page, in this browser: token or not, no.
        let origin =
            self.get("origin").map(|o| o.split_once("://").map(|x| x.1));
        let foreign = tcp && origin.is_some_and(|o| o != self.get("host"));
        let t = self.get("irweb-access").unwrap_or_default();
        !foreign && crypto::same(t.as_bytes(), token.as_bytes())
    }
}

/// Its head; the body (`body`) only once it may be run.
async fn read(r: &mut (impl AsyncRead + Unpin)) -> Result<Req> {
    let mut b = vec![];
    let end = loop {
        if let Some(i) = b.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        ensure!(b.len() < HEAD, "head too long");
        let mut chunk = [0; 4096];
        let n = r.read(&mut chunk).await?;
        ensure!(n > 0, "cut off");
        b.extend_from_slice(&chunk[..n]);
    };
    let mut lines = std::str::from_utf8(&b[..end])?.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (first.next(), first.next());
    let (method, path) = (method.unwrap_or_default(), path.unwrap_or_default());
    let head = lines.filter_map(|l| l.split_once(':'));
    let head = head.map(|(k, v)| (k.trim().to_lowercase(), v.trim().into()));
    let (method, path) = (method.into(), path.into());
    let body = b[end + 4..].to_vec();
    Ok(Req { method, path, head: head.collect(), body })
}

async fn body(r: &mut (impl AsyncRead + Unpin), q: &mut Req) -> Result<()> {
    match q.get("content-length").map(str::parse::<usize>).transpose()? {
        Some(n) => {
            ensure!(n <= BODY, "too big");
            let have = q.body.len().min(n);
            q.body.resize(n, 0);
            r.read_exact(&mut q.body[have..]).await?;
        }
        // irweb's page half-closes after its body
        None if q.method == "POST" => {
            r.take(BODY as u64).read_to_end(&mut q.body).await.map(drop)?
        }
        None => {}
    }
    Ok(())
}

async fn answer(
    o: &mut (impl AsyncWrite + Unpin),
    status: &str,
    kind: &str,
    body: Option<&[u8]>,
) -> std::io::Result<()> {
    head(o, status, kind, body.map(|b| b.len() as u64)).await?;
    o.write_all(body.unwrap_or_default()).await
}

/// A response's head; its body, of `len` if known, follows.
async fn head(
    o: &mut (impl AsyncWrite + Unpin),
    status: &str,
    kind: &str,
    len: Option<u64>,
) -> std::io::Result<()> {
    let len = len.map(|n| format!("content-length: {n}\r\n"));
    let head = format!(
        "HTTP/1.0 {status}\r\ncontent-type: {kind}\r\n{}cache-control: \
        no-cache\r\nx-content-type-options: nosniff\r\nreferrer-policy: \
        no-referrer\r\ncross-origin-resource-policy: same-origin\r\n\
        content-security-policy: {CSP}\r\n\r\n",
        len.unwrap_or_default()
    );
    o.write_all(head.as_bytes()).await
}

async fn exchange(
    w: Arc<Web>,
    mut r: impl AsyncRead + Unpin,
    mut o: impl AsyncWrite + Unpin,
    tcp: bool,
) {
    let Ok(_held) = w.cap.try_acquire() else { return };
    let q = timeout(Duration::from_secs(30), request(&mut r, &w.token, tcp));
    let _ = match q.await {
        Ok(Ok(Some(q))) => route(&w, q, &mut o).await,
        Ok(Ok(None)) => {
            answer(&mut o, "403 Forbidden", "text/plain", Some(b""))
                .await
                .map_err(Into::into)
        }
        _ => Ok(()),
    };
    let _ = o.shutdown().await;
}

/// A request, its body read only once it may run: none, a stranger's.
/// With 256 bits of token, a wrong one is answered at once (no wait
/// holds a slot).
async fn request(
    r: &mut (impl AsyncRead + Unpin),
    token: &str,
    tcp: bool,
) -> Result<Option<Req>> {
    let mut q = timeout(Duration::from_secs(5), read(r)).await??;
    // The page, and a saved file's one-time link (a browser's own
    // download, which sends no token): no body.
    if q.api().is_none_or(|c| q.method == "GET" && c.starts_with("save/")) {
        return Ok(Some(q));
    }
    if !q.allowed(token, tcp) {
        return Ok(None);
    }
    body(r, &mut q).await?;
    Ok(Some(q))
}

async fn route(
    w: &Arc<Web>,
    q: Req,
    o: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let path = q.path.split('?').next().unwrap_or_default();
    let Some(cmd) = q.api() else {
        let f = (q.method == "GET").then(|| page(path)).flatten();
        let Some((kind, b)) = f else {
            return Ok(
                answer(o, "404 Not Found", "text/plain", Some(b"")).await?
            );
        };
        return Ok(answer(o, "200 OK", kind, Some(&b)).await?);
    };
    match (q.method.as_str(), cmd) {
        ("GET", "events") => {
            let changed: Arc<str> =
                r#"{"event":"chatir","payload":{"t":"changed"}}"#.into();
            let bytes = |s: Arc<str>| s.as_bytes().to_vec();
            down(o, w.news.subscribe(), bytes, Some(changed)).await
        }
        ("GET", "tune") => {
            let bytes = |(who, f): (chatir_core::Id, Vec<u8>)| {
                [&who.0[..], &f].concat()
            };
            down(o, w.n.media(), bytes, None).await
        }
        ("GET", c) if c.starts_with("save/") => give(w, &c[5..], o).await,
        ("POST", cmd) => match post(w, cmd, &q).await {
            Ok(v) => {
                let b = v.to_string();
                Ok(answer(o, "200 OK", "application/json", Some(b.as_bytes()))
                    .await?)
            }
            Err(e) => {
                let b = format!("{e:#}");
                Ok(answer(o, "500 Error", "text/plain", Some(b.as_bytes()))
                    .await?)
            }
        },
        _ => Ok(answer(o, "404 Not Found", "text/plain", Some(b"")).await?),
    }
}

/// The page, with web.js before the app's own scripts.
fn page(path: &str) -> Option<(&'static str, Vec<u8>)> {
    let name = match path.trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    let b = UI.iter().find(|f| f.0 == name)?.1;
    let kind = match name.rsplit('.').next()? {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        _ => "text/plain; charset=utf-8",
    };
    if name != "index.html" {
        return Some((kind, b.to_vec()));
    }
    let html = String::from_utf8_lossy(b);
    let web = "<script src=\"./web.js\"></script></head>";
    Some((kind, html.replacen("</head>", web, 1).into_bytes()))
}

/// Frames as they come; `lag`, first and whenever some were missed.
async fn down<T: Clone>(
    o: &mut (impl AsyncWrite + Unpin),
    mut rx: broadcast::Receiver<T>,
    bytes: impl Fn(T) -> Vec<u8>,
    lag: Option<T>,
) -> Result<()> {
    head(o, "200 OK", "application/octet-stream", None).await?;
    let mut next = lag.clone();
    loop {
        let x = match next.take() {
            Some(x) => x,
            None => match rx.recv().await {
                Ok(x) => x,
                Err(RecvError::Lagged(_)) => match lag.clone() {
                    Some(x) => x,
                    None => continue,
                },
                Err(RecvError::Closed) => return Ok(()),
            },
        };
        let b = bytes(x);
        o.write_all(&(b.len() as u32).to_be_bytes()).await?;
        o.write_all(&b).await?;
        o.flush().await?;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A saved file, once, by its link: unknown or taken, 404.
async fn give(
    w: &Web,
    link: &str,
    o: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let p = lock(&w.saved).remove(link);
    let f = match p {
        Some(p) => tokio::fs::File::open(p).await.ok(),
        None => None,
    };
    let Some(mut f) = f else {
        return Ok(answer(o, "404 Not Found", "text/plain", Some(b"")).await?);
    };
    let len = f.metadata().await?.len();
    head(o, "200 OK", "application/octet-stream", Some(len)).await?;
    tokio::io::copy(&mut f, o).await?;
    Ok(())
}

async fn post(w: &Arc<Web>, cmd: &str, q: &Req) -> Result<Value> {
    match cmd {
        "chatir" => {
            let c: api::Call = serde_json::from_slice(&q.body)?;
            // Only what the page uploaded is shared: no other path here.
            let mut sealed = None;
            // Files come only to this node's own `received` folder.
            if let api::Call::Accept { dir, .. } = &c {
                ensure!(Path::new(dir) == w.got, "files come to {:?}", w.got);
            }
            if let api::Call::Offer { path, .. }
            | api::Call::Seal { path, .. }
            | api::Call::SendTo { path, .. } = &c
            {
                let p = Path::new(path);
                let name = p.file_name().and_then(|f| f.to_str());
                let ours = p.parent() == Some(&w.up) && name.is_some_and(hex32);
                ensure!(ours, "only files you upload are shared");
                let seal = matches!(c, api::Call::Seal { .. });
                sealed = seal.then(|| p.to_path_buf());
            }
            let r = api::call(&w.n, c).await;
            // Sealed, an upload's plain copy goes.
            sealed.map(std::fs::remove_file);
            r
        }
        // Where an accepted file comes to: this node's own folder.
        "downloads" => Ok(json!(w.got)),
        // Saved here (any size: `file` stops at 25 MB, in memory), then
        // fetched by a one-time link the browser downloads itself.
        "save" => {
            let c = serde_json::from_slice(&q.body)?;
            let api::Call::File { space, file } = c else { bail!("no file") };
            let t = HEXLOWER.encode(&crypto::rand::<16>());
            let p = w.n.save(&space, &file, &w.out.join(&t)).await?;
            lock(&w.saved).insert(t.clone(), p);
            // A minute to start it; then the link and its file are gone.
            let (w, link) = (w.clone(), format!("api/save/{t}"));
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                lock(&w.saved).remove(&t);
                let _ = std::fs::remove_dir_all(w.out.join(&t));
            });
            Ok(json!(link))
        }
        // A picked file in parts, as a page knows no paths.
        "upload" => {
            use std::io::Write;
            let id = q.get("x-upload").filter(|i| hex32(i));
            let p = w.up.join(id.context("no upload id")?);
            std::fs::create_dir_all(&w.up)?;
            let mut f = std::fs::OpenOptions::new();
            f.create(true).append(true).open(&p)?.write_all(&q.body)?;
            Ok(json!(p))
        }
        // Call frames, each `[len u32][frame]`, in the order sent.
        "media" => {
            let mut b = &q.body[..];
            while let Some((len, rest)) = b.split_first_chunk::<4>() {
                let len = (u32::from_be_bytes(*len) as usize).min(rest.len());
                w.n.send_media(&rest[..len]);
                b = &rest[len..];
            }
            Ok(Value::Null)
        }
        _ => bail!("no such call"),
    }
}

#[tokio::test]
async fn a_strangers_body_is_never_read() {
    let head = |t: &str, origin: &str| {
        format!(
            "POST /api/send HTTP/1.1\r\nhost: me\r\norigin: {origin}\r\n\
            irweb-access: {t}\r\ncontent-length: 8000000\r\n\r\n"
        )
    };
    // Its 8 MB never come: refused on its head alone.
    for (t, origin, tcp) in [
        ("wrong", "http://me", true),
        ("right", "http://evil", true),
        ("wrong", "x", false),
    ] {
        let r = head(t, origin).into_bytes();
        let got = request(&mut &r[..], "right", tcp).await.unwrap();
        assert!(got.is_none(), "{t} {origin}");
    }
    // The owner's: read on, so the missing body is an error.
    let r = head("right", "http://me").into_bytes();
    assert!(request(&mut &r[..], "right", true).await.is_err());
    let page = b"GET / HTTP/1.1\r\n\r\n";
    assert!(request(&mut &page[..], "right", true).await.unwrap().is_some());
    // A saved file's one-time link is its own secret; the rest want ours.
    let save = b"GET /api/save/x HTTP/1.1\r\n\r\n";
    assert!(request(&mut &save[..], "right", true).await.unwrap().is_some());
    let events = b"GET /api/events HTTP/1.1\r\n\r\n";
    assert!(request(&mut &events[..], "right", true).await.unwrap().is_none());
}
