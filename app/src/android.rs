//! Android: `ChatirPlugin.kt` sees network changes iroh cannot; `Push.kt` wakes.

extern crate pushir; // org.pushir.Native, for pushir's Android library

use anyhow::Context;
use chatir_core::push;
use chatir_core::wake::News;
use chatir_core::{Node, Notice};
use jni::errors::LogErrorAndDefault as Log;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::{Env, EnvUnowned};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager, Wry};

pub struct Bridge(PluginHandle<Wry>);

pub fn init() -> TauriPlugin<Wry> {
    Builder::new("chatir")
        .setup(|app, api| {
            let h = api
                .register_android_plugin("center.cxn.chatir", "ChatirPlugin")?;
            app.manage(Bridge(h));
            Ok(())
        })
        .build()
}

#[derive(Serialize)]
struct Watch {
    channel: Channel,
}

#[derive(Serialize)]
struct Text<'a> {
    text: &'a str,
}

/// Via the browser: Play Protect takes apps that install apps for malware.
pub async fn upgrade(app: &tauri::AppHandle) -> anyhow::Result<String> {
    let url = format!("{}/download/chatir.apk", chatir_core::release::LATEST);
    let bridge = app.try_state::<Bridge>().context("no bridge")?;
    bridge.call("browse", Url { url: &url }).await;
    Ok("Your browser downloads chatir: open it to install".into())
}

pub async fn notify_check(
    app: &tauri::AppHandle,
    ask: &str,
) -> anyhow::Result<serde_json::Value> {
    let b = app.try_state::<Bridge>().context("no bridge")?;
    // The app's own traffic and the battery, for Network use.
    if ask == "traffic" {
        let got = b.0.run_mobile_plugin_async("traffic", serde_json::json!({}));
        return Ok(got.await?);
    }
    let cmd = match ask {
        "fix" => Some("allowAlerts"),
        "test" => Some("testAlert"),
        "battery" => Some("batteryUse"),
        _ => None,
    };
    if let Some(cmd) = cmd {
        b.call(cmd, serde_json::json!({})).await;
    }
    let got = b.0.run_mobile_plugin_async("alerts", serde_json::json!({}));
    Ok(got.await?)
}

#[derive(Serialize)]
struct Url<'a> {
    url: &'a str,
}

#[derive(Serialize)]
struct Write<'a> {
    path: &'a str,
    uri: &'a str,
}

impl Bridge {
    pub async fn watch_network(&self, app: tauri::AppHandle) {
        static ONCE: AtomicBool = AtomicBool::new(false);
        if ONCE.swap(true, SeqCst) {
            return;
        }
        let channel = Channel::new(move |b| {
            let v: serde_json::Value = b.deserialize().unwrap_or_default();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                match v["fg"].as_bool() {
                    Some(fg) => crate::seen(&app, fg).await,
                    None => {
                        if let Some(n) = crate::running() {
                            n.resume().await
                        }
                    }
                }
            });
            Ok(())
        });
        self.call("watchNetwork", Watch { channel }).await;
    }

    pub async fn alert(&self, x: &Notice) {
        self.call("alert", x).await;
    }

    pub async fn hush(&self) {
        self.call("hush", serde_json::json!({})).await;
    }

    pub async fn in_call(&self, on: bool) {
        self.call("inCall", serde_json::json!({ "on": on })).await;
    }

    pub async fn share(&self, text: &str) {
        self.call("share", Text { text }).await;
    }

    /// Where to save `name`, in Android's picker: `None` if put away.
    pub async fn save_as(&self, name: &str) -> anyhow::Result<Option<String>> {
        #[derive(serde::Deserialize)]
        struct Picked {
            uri: Option<String>,
        }
        let r = self
            .0
            .run_mobile_plugin_async::<Picked>("saveAs", Text { text: name });
        Ok(r.await?.uri)
    }

    /// The file at `path` to where `save_as` said, and gone from here.
    pub async fn write_to(
        &self,
        path: &std::path::Path,
        uri: &str,
    ) -> anyhow::Result<()> {
        let path = &path.to_string_lossy();
        let r = self
            .0
            .run_mobile_plugin_async::<()>("writeTo", Write { path, uri });
        Ok(r.await?)
    }

    /// False if no app opens it.
    pub async fn browse(&self, url: &str) -> bool {
        let r = self.0.run_mobile_plugin_async::<()>("browse", Url { url });
        r.await.is_ok()
    }

    async fn call(&self, cmd: &str, args: impl Serialize) {
        if let Err(e) = self.0.run_mobile_plugin_async::<()>(cmd, args).await {
            tracing::warn!("android {cmd}: {e}");
        }
    }
}

// tao sets ndk-context once per process, asserting nobody did before.
static OURS: AtomicBool = AtomicBool::new(false);
static TAO: AtomicBool = AtomicBool::new(false);

static NEWS: std::sync::Mutex<Vec<News>> = std::sync::Mutex::new(vec![]);

/// Notices as JSON: at once, or (`full`) after catching up.
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_run<'l>(
    mut env: EnvUnowned<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
    full: bool,
) -> JString<'l> {
    env.with_env(|e| reply(e, &ctx, wake(profile(e, &dir), full)))
        .resolve::<Log>()
}

/// A call declined from its notification: said by the node the ring
/// started, so its caller hangs up and our other devices stop ringing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_decline<'l>(
    mut env: EnvUnowned<'l>,
    _: JClass<'l>,
    space: JString<'l>,
    room: JString<'l>,
) {
    env.with_env(|e| -> jni::errors::Result<()> {
        let (s, r) = (text(e, &space), text(e, &room));
        let (Ok(s), Ok(r)) = (s.parse(), r.parse()) else { return Ok(()) };
        tauri::async_runtime::block_on(async {
            if let Some(n) = crate::running() {
                n.decline(&s, r).await;
            }
        });
        Ok(())
    })
    .resolve::<Log>()
}

/// Rung through a storage node, else on our lane alone (`live`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_live<'l>(
    mut env: EnvUnowned<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
) -> JString<'l> {
    env.with_env(|e| reply(e, &ctx, live(profile(e, &dir)))).resolve::<Log>()
}

/// "" if null.
fn text(e: &Env, s: &JString) -> String {
    if s.is_null() {
        String::new()
    } else {
        s.try_to_string(e).unwrap_or_default()
    }
}

fn profile(e: &Env, dir: &JString) -> PathBuf {
    chatir_core::profile(std::path::Path::new(&text(e, dir)))
}

fn reply<'l, T: Serialize + Default>(
    env: &mut Env<'l>,
    ctx: &JObject,
    f: impl Future<Output = anyhow::Result<T>>,
) -> jni::errors::Result<JString<'l>> {
    hold(env, ctx);
    let r = tauri::async_runtime::block_on(f);
    let r =
        r.inspect_err(|e| tracing::warn!("wake: {e:#}")).unwrap_or_default();
    env.new_string(serde_json::to_string(&r).unwrap_or_default())
}

/// Android's context for native code, where the activity's runtime has
/// not set it.
fn hold(env: &Env, ctx: &JObject) {
    if !TAO.load(SeqCst)
        && !OURS.load(SeqCst)
        && let (Ok(vm), Ok(g)) = (env.get_java_vm(), env.new_global_ref(ctx))
    {
        let (vm, c) = (vm.get_raw().cast(), g.into_raw().cast());
        unsafe { ndk_context::initialize_android_context(vm, c) };
        OURS.store(true, SeqCst);
    }
}

/// The activity is starting, and tao brings its own context.
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_release<'l>(
    _: EnvUnowned<'l>,
    _: JClass<'l>,
) {
    TAO.store(true, SeqCst);
    if OURS.swap(false, SeqCst) {
        unsafe { ndk_context::release_android_context() };
    }
}

async fn wake(dir: PathBuf, full: bool) -> anyhow::Result<Vec<Notice>> {
    let hide = dir.join("private").exists();
    let quiet = dir.join("silent").exists();
    let v = told(dir, full).await?;
    Ok(v.into_iter().map(|x| crate::veil(x, hide, quiet)).collect())
}

async fn told(dir: PathBuf, full: bool) -> anyhow::Result<Vec<Notice>> {
    let n = node(dir).await?;
    let news =
        std::mem::take(&mut *NEWS.lock().unwrap_or_else(|e| e.into_inner()));
    let spaces: Vec<_> = news.iter().map(|x| x.0).collect();
    news.into_iter().for_each(|x| n.deliver(x));
    if crate::FG.load(SeqCst) {
        return Ok(vec![]); // in sight: the app shows it
    }
    if !full {
        // A DM whose call ended stops ringing.
        let mut v = crate::alerts(&n, false);
        // Its caller hears that it rings here (the ring said it came).
        for x in v.iter().filter(|x| x.ring) {
            n.ringing(&x.space, x.room).await;
        }
        let ended =
            spaces.into_iter().filter(|s| n.loud(s) && n.calls(s).is_empty());
        v.extend(ended.map(|space| Notice {
            space,
            room: chatir_core::state::DM,
            title: String::new(),
            body: String::new(),
            ring: false,
            hush: true,
            quiet: false,
        }));
        return Ok(v);
    }
    n.resume().await;
    let (mut rx, end) =
        (n.subscribe(), Instant::now() + Duration::from_secs(30));
    while Instant::now() < end
        && matches!(
            tokio::time::timeout(Duration::from_secs(4), rx.recv()).await,
            Ok(Ok(_))
        )
    {}
    let notices = crate::alerts(&n, false);
    // Ringing: up until it is answered (in sight) or ends.
    let ringing = || n.spaces().iter().any(|s| !n.calls(s).is_empty());
    for _ in 0..45 {
        if !ringing() || crate::FG.load(SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    crate::nap().await;
    Ok(notices)
}

async fn node(dir: PathBuf) -> anyhow::Result<std::sync::Arc<Node>> {
    crate::boot(crate::config(dir, None, None)?).await
}

/// Our lane (the phone's mailbox), and our route if a storage node we know
/// takes it: told to our spaces once a run, and when the mailbox changed.
/// What comes, comes to `Wake.ring`.
async fn live(dir: PathBuf) -> anyhow::Result<()> {
    static TOLD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let Some(lane) = push::lane_at(&dir) else { return Ok(()) };
    if TOLD.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(&lane) {
        return Ok(());
    }
    let n = node(dir).await?;
    let lane = n.pushir().await;
    n.set_push(lane).await;
    *TOLD.lock().unwrap_or_else(|e| e.into_inner()) = Some(lane);
    // A minute to tell the spaces, then asleep (unless in sight).
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        crate::nap().await;
    });
    Ok(())
}

/// chatir's lane secret on the phone's mailbox, for pushir's library; empty
/// if it cannot be kept (never zeros: a secret anyone knows).
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_lane<'l>(
    mut env: EnvUnowned<'l>,
    _: JClass<'l>,
    dir: JString<'l>,
) -> JByteArray<'l> {
    env.with_env(|e| {
        let k = push::secret(&profile(e, &dir));
        e.byte_array_from_slice(k.as_ref().map_or(&[][..], |k| &k[..]))
    })
    .resolve::<Log>()
}

/// The phone's mailbox id, from pushir's library ("" until known).
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_share<'l>(
    mut env: EnvUnowned<'l>,
    _: JClass<'l>,
    id: JString<'l>,
) {
    env.with_env(|e| -> jni::errors::Result<()> {
        push::share(&text(e, &id));
        Ok(())
    })
    .resolve::<Log>()
}

/// What pushir brought for chatir: to its node if one runs, else its notices
/// and whether to catch up.
#[unsafe(no_mangle)]
pub extern "system" fn Java_center_cxn_chatir_Wake_ring<'l>(
    mut env: EnvUnowned<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
    data: JByteArray<'l>,
) -> JString<'l> {
    env.with_env(|e| {
        let dir = profile(e, &dir);
        let b = e.convert_byte_array(&data).unwrap_or_default();
        reply(e, &ctx, async move {
            if let Some(n) = crate::running() {
                n.hear(&b);
                return Ok(Shown::default());
            }
            Ok(match Node::news(&dir, &b) {
                Some(Some(news)) => shown(&dir, news),
                Some(None) => Shown { catch: true, ..Shown::default() },
                None => Shown::default(),
            })
        })
    })
    .resolve::<Log>()
}

/// What a ring brings, to show now, and whether to catch up for it. With
/// chatir in use its node takes it; else no node starts (`Node::woken`).
#[derive(serde::Serialize, Default)]
struct Shown {
    show: Vec<Notice>,
    catch: bool,
}

fn shown(dir: &std::path::Path, news: News) -> Shown {
    if let Some(n) = crate::running() {
        n.deliver(news);
        return Shown::default();
    }
    let (n, catch) = Node::woken(dir, &news);
    // A call: the node starts as it rings, so it connects as it is answered.
    let catch = catch || n.as_ref().is_some_and(|x| x.ring);
    if catch {
        NEWS.lock().unwrap_or_else(|e| e.into_inner()).push(news);
    }
    let hide = dir.join("private").exists();
    let n = n.filter(|_| !crate::FG.load(SeqCst));
    let quiet = dir.join("silent").exists();
    let show = n.into_iter().map(|x| crate::veil(x, hide, quiet)).collect();
    Shown { show, catch }
}
