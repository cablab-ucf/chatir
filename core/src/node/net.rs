//! The endpoint: iroh up and at rest, relays, what answers, the web.

use super::*;

/// As far as its address says: not a LAN's, a carrier's 100.64/10, nor
/// one of those hidden in IPv6 (::ffff:10.0.0.1, 64:ff9b::10.0.0.1).
pub(crate) fn public(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr::{V4, V6};
    match ip {
        V4(a) => {
            let o = a.octets();
            !(a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && o[1] & 0xc0 == 64)
                || (o[0] == 198 && o[1] & 0xfe == 18))
        }
        V6(a) => {
            let s = a.segments();
            let nat64 = s[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
            let v4 = std::net::Ipv4Addr::from_bits(a.to_bits() as u32);
            match a.to_ipv4_mapped() {
                Some(v4) => public(&V4(v4)),
                None if nat64 => public(&V4(v4)),
                None => {
                    !(a.is_loopback()
                        || a.is_unspecified()
                        || a.is_multicast()
                        || (s[0] & 0xfe00) == 0xfc00
                        || (s[0] & 0xffc0) == 0xfe80)
                }
            }
        }
    }
}

/// `url` or `url#key`: relays are open; a key from whoever runs one lifts
/// its rate limit for us (pushir's relay), for transfers too big to wait.
pub(super) fn relay(l: &str) -> Option<(RelayUrl, &str)> {
    let (u, k) = l.trim().split_once('#').unwrap_or((l.trim(), ""));
    Some((u.parse().ok()?, k))
}

/// Our key for each relay that gave us one (`relay-keys`: `url#key` lines).
pub(super) fn keys(dir: &Path) -> Vec<(RelayUrl, String)> {
    let f = std::fs::read_to_string(dir.join("relay-keys"));
    let f = f.unwrap_or_default();
    let k = f.lines().filter_map(relay).filter(|k| !k.1.is_empty());
    k.map(|(u, k)| (u, k.to_string())).collect()
}

/// Whether `relay` takes `key` as lifting its rate limit (pushir's relay,
/// `/key`): `None` when it does not say (down, or not pushir's).
async fn key_works(relay: &RelayUrl, key: &str) -> Option<bool> {
    let u = relay.join("key").ok()?;
    let r = http().get(u).bearer_auth(key).send().await.ok()?;
    match r.status().as_u16() {
        204 => Some(true),
        403 => Some(false),
        _ => None,
    }
}

/// The relays links home on: the lab's (irnet), or our own (`--relay`).
pub(super) fn homes(net: &Net) -> Vec<RelayUrl> {
    match net {
        Net::Default => irnet::relays(),
        Net::Relay(urls) => urls.clone(),
        _ => vec![],
    }
}

/// irnet's endpoint, as every app of the lab's binds it: each handshake
/// post-quantum or refused, BBRv3, the system's roots and proxy, no
/// third-party lookup (no DNS, no DHT, no captive-portal probes): a device
/// is reached by the addresses we hold for it (`Node::findable`), through
/// the relays those name. Homed on the lab's relay (n0's only while that
/// is down: `irnet::fallback`), or on our own. While up, iroh pings its
/// relay every 15 s and each open path every 5 s, so chatir holds it only
/// while a link is needed; asleep, a device holds pushir's socket.
pub(crate) fn endpoint(net: &Net) -> iroh::endpoint::Builder {
    match net {
        Net::Default => irnet::endpoint(),
        Net::Relay(urls) => irnet::on(urls),
        Net::Lan => {
            irnet::endpoint().relay_mode(RelayMode::Disabled).address_lookup(
                iroh_mdns_address_lookup::MdnsAddressLookup::builder(),
            )
        }
        Net::Direct => irnet::endpoint().relay_mode(RelayMode::Disabled),
    }
}

impl ProtocolHandler for Proto {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let gone = || AcceptError::from_err(std::io::Error::other("closing"));
        let n = self.0.upgrade().ok_or_else(gone)?;
        let r = match c.alpn() {
            sync::ALPN => sync::respond(&n, &c).await,
            call::ALPN => {
                n.answer(c).await;
                Ok(())
            }
            HAND => n.handed(&c).await,
            DIRECT => n.told(&c).await,
            crate::send::ALPN => {
                n.lend(c).await;
                Ok(())
            }
            _ => n.give(&c).await,
        };
        r.map_err(|e| {
            AcceptError::from_err(std::io::Error::other(e.to_string()))
        })
    }
}

/// Each file asked for, to whom it may go: an offered one only to the
/// members of the space it was posted in.
pub(super) fn gate(
    n: std::sync::Weak<Node>,
) -> iroh_blobs::provider::events::EventSender {
    use iroh_blobs::provider::events::*;
    let mask = EventMask {
        connected: ConnectMode::Intercept,
        get: RequestMode::Intercept,
        get_many: RequestMode::Disabled,
        // Asked like a get, and unanswered: whether a file is here is told
        // to nobody.
        observe: ObserveMode::Intercept,
        ..EventMask::DEFAULT
    };
    let (tx, mut rx) = EventSender::channel(32, mask);
    tokio::spawn(async move {
        let mut who = HashMap::new();
        while let Some(m) = rx.recv().await {
            match m {
                ProviderMessage::ClientConnected(m) => {
                    if let Some(d) = m.endpoint_id {
                        who.insert(m.connection_id, d);
                    }
                    let _ = m.tx.send(Ok(())).await;
                }
                ProviderMessage::ConnectionClosed(m) => {
                    who.remove(&m.inner.connection_id);
                }
                ProviderMessage::GetRequestReceived(m) => {
                    let d = who.get(&m.inner.connection_id);
                    let h = Id(*m.inner.request.hash.as_bytes());
                    let n = n.upgrade().zip(d);
                    let ok = n.is_some_and(|(n, d)| n.may_get(d, &h));
                    let r =
                        if ok { Ok(()) } else { Err(AbortReason::Permission) };
                    let _ = m.tx.send(r).await;
                }
                _ => {}
            }
        }
    });
    tx
}

/// A direct link, closed `QUIET_LINK` after its last use (an open one is
/// pinged every 5 s).
pub(super) async fn quiet(links: Links, d: EndpointId, c: Connection) {
    loop {
        let ours = |l: &(Connection, Instant)| l.0.stable_id() == c.stable_id();
        let used = lock(&links).get(&d).filter(|l| ours(l)).map(|l| l.1);
        let Some(used) = used.filter(|_| c.close_reason().is_none()) else {
            return c.close(0u32.into(), b"replaced");
        };
        let left = QUIET_LINK.saturating_sub(used.elapsed());
        if left.is_zero() {
            lock(&links).remove(&d);
            return c.close(0u32.into(), b"quiet");
        }
        tokio::time::sleep(left).await;
    }
}

impl ProtocolHandler for Kept {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        self.0.accept(c).await
    }
}

/// TLS as iroh sets it up, roots compiled in.
pub fn http() -> reqwest::Client {
    client().build().expect("HTTP client")
}

pub(crate) fn client() -> reqwest::ClientBuilder {
    #[cfg(not(target_os = "android"))]
    let ca = iroh::tls::CaTlsConfig::system();
    #[cfg(target_os = "android")]
    let ca = iroh::tls::CaTlsConfig::embedded();
    let tls = ca.client_config(irnet::tls()).expect("TLS");
    let b = reqwest::Client::builder().tls_backend_preconfigured(tls);
    // Stalls end a request, not its length: an update can be large.
    let b = b.connect_timeout(Duration::from_secs(20));
    b.read_timeout(Duration::from_secs(30))
}

/// Link previews: https to public addresses, every hop of up to three
/// redirects too (an address in a URL skips the resolver's check).
pub(crate) fn web() -> reqwest::Client {
    use reqwest::redirect::Policy;
    let hop = Policy::custom(|a| match a.previous().len() {
        3.. => a.error("too many redirects"),
        _ if !open(a.url()) => a.error("not a public address"),
        _ => a.follow(),
    });
    let b = client().redirect(hop).https_only(true).dns_resolver(Public);
    b.user_agent("chatir link preview").build().expect("HTTP client")
}

/// https, and no address of a LAN's or this machine's.
pub(crate) fn open(u: &reqwest::Url) -> bool {
    let host = u.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let ip = host.parse::<std::net::IpAddr>().ok();
    u.scheme() == "https" && ip.is_none_or(|a| public(&a))
}

impl reqwest::dns::Resolve for Public {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let all = tokio::net::lookup_host((host, 0)).await?;
            let v: Vec<_> = all.filter(|a| public(&a.ip())).collect();
            if v.is_empty() {
                return Err("no public address".into());
            }
            Ok(Box::new(v.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl Node {
    /// iroh, if up (asleep, it is dropped: its port goes with it).
    pub(super) fn ep(&self) -> Option<Endpoint> {
        let i = read(&self.iroh);
        i.as_ref().map(|i| i.ep.clone()).filter(|e| !e.is_closed())
    }

    pub(super) fn gossip(&self) -> Option<Gossip> {
        let i = read(&self.iroh);
        i.as_ref().map(|i| i.gossip.clone())
    }

    /// iroh, up: bound again if it slept (same key, same port), its
    /// router serving all we serve.
    pub(crate) async fn up(&self) -> Result<Endpoint> {
        *lock(&self.used) = Instant::now();
        if let Some(ep) = self.ep() {
            return Ok(ep);
        }
        let _one = self.binding.lock().await;
        if let Some(ep) = self.ep() {
            return Ok(ep);
        }
        ensure!(!self.down.load(Relaxed), "shut down");
        let i = self.bound().await?;
        let ep = i.ep.clone();
        *write(&self.iroh) = Some(i);
        Ok(ep)
    }

    async fn bound(&self) -> Result<Iroh> {
        let n = self.this.get().cloned().unwrap_or_default();
        let at = |port: u16| {
            let b = endpoint(&self.net);
            // Our key for a relay of ours goes along: no rate limit there.
            let keys = keys(&self.dir);
            let keyed = |u: RelayUrl| {
                let c = iroh::RelayConfig::from(u.clone());
                match keys.iter().find(|k| k.0 == u) {
                    Some(k) => c.with_auth_token(k.1.clone()),
                    None => c,
                }
            };
            let map: iroh::RelayMap =
                homes(&self.net).into_iter().map(keyed).collect();
            let b = match keys.is_empty() || map.is_empty() {
                true => b,
                false => b.relay_mode(RelayMode::Custom(map)),
            };
            let v6 = BindOpts::default().set_is_required(false);
            let b = b.secret_key(self.key.clone());
            let b = b.address_lookup(self.lookup.clone()).clear_ip_transports();
            let b = b.bind_addr((Ipv4Addr::UNSPECIFIED, port))?;
            anyhow::Ok(
                b.bind_addr_with_opts((Ipv6Addr::UNSPECIFIED, port), v6)?,
            )
        };
        // The same port as last time: peers that know only it still reach us.
        // Just asleep, its sockets may take a moment to go.
        let port = lock(&self.local).port;
        let mut tries = 0;
        let ep = loop {
            match at(port)?.bind().await {
                Ok(ep) => break ep,
                Err(_) if port != 0 && tries < 20 => tries += 1,
                Err(e) if port != 0 => {
                    warn!("port {port} unavailable ({e:#})");
                    break at(0)?.bind().await?;
                }
                Err(e) => return Err(e.into()),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let v4 = ep.bound_sockets().into_iter().find(|a| a.is_ipv4());
        let port = v4.map_or(0, |a| a.port());
        if std::mem::replace(&mut lock(&self.local).port, port) != port {
            self.persist();
        }
        if matches!(self.net, Net::Direct) {
            let at = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let a = EndpointAddr::from_parts(ep.id(), [TransportAddr::Ip(at)]);
            self.lookup.add_endpoint_info(a);
        }
        let gossip =
            Gossip::builder().max_message_size(GOSSIP_MAX).spawn(ep.clone());
        let gate = Some(gate(n.clone()));
        // Every link counted, each way (`traffic`).
        use crate::traffic::Counted;
        let t = &self.traffic;
        let blobs = Kept(BlobsProtocol::new(&self.files, gate));
        let mut router = Router::builder(ep.clone())
            .accept(iroh_blobs::ALPN, Counted(blobs, t.clone()))
            .accept(iroh_gossip::ALPN, Counted(gossip.clone(), t.clone()))
            .accept(sync::ALPN, Counted(Proto(n.clone()), t.clone()))
            .accept(BLOB, Counted(Proto(n.clone()), t.clone()))
            .accept(HAND, Counted(Proto(n.clone()), t.clone()))
            .accept(DIRECT, Counted(Proto(n.clone()), t.clone()))
            .accept(crate::send::ALPN, Counted(Proto(n.clone()), t.clone()))
            .accept(call::ALPN, Counted(Proto(n), t.clone()));
        // A storage node tells its devices the relay their lanes go on.
        if let Some(p) = &self.server {
            router =
                router.accept(crate::push::ALPN, Counted(p.clone(), t.clone()));
        }
        let router = router.spawn();
        // The lab's relay down for 10 s: n0's, until it answers again.
        if matches!(self.net, Net::Default) {
            tokio::spawn(irnet::fallback(ep.clone()));
        }
        Ok(Iroh { ep, gossip, router, keys: keys(&self.dir) })
    }

    /// A connection to `to`, iroh up for it; `to` knocked first, so its
    /// own iroh wakes if it slept.
    pub(crate) async fn connect(
        &self,
        to: impl Into<EndpointAddr>,
        alpn: &[u8],
    ) -> Result<Connection> {
        let to = to.into();
        self.knock(to.id);
        let ep = self.up().await?;
        self.findable(&[to.id]);
        let c = ep.connect(to, alpn).await?;
        self.traffic.count(&c);
        Ok(c)
    }

    /// Whether `d` is up in `sp` (said `Here`, not `Gone`), or is one of
    /// its storage nodes, always on.
    pub(super) fn present(&self, sp: &Space, d: &EndpointId) -> bool {
        lock(&sp.seen).iter().any(|((_, x), s)| x == d && s.0)
            || lock(&sp.book).st.hosts().iter().any(|a| a.id == *d)
            || self.stores(&sp.id).iter().any(|s| s.0.id == *d)
    }

    /// A dial frame to `d`'s lane, if a space we share says where and `d`
    /// is up there: its iroh may sleep, its node is running.
    pub(super) fn knock(&self, d: EndpointId) {
        for sp in self.all().into_iter().filter(|sp| self.present(sp, &d)) {
            let k = sp.inv.as_ref().map(|i| lock(&sp.book).key(i).0);
            if let (Some(at), Some(k)) = (self.lanes(&sp.id).remove(&d), k) {
                let f =
                    [&[crate::push::DIAL][..], &ev_label(&sp.id, &k)].concat();
                let dir = self.dir.clone();
                tokio::spawn(
                    async move { crate::push::event(&dir, &at, &f).await },
                );
                return;
            }
        }
    }

    /// `d`'s lane told to catch up, in a space we share where it has one:
    /// asleep (its node down), it wakes, says it is here, and hears what
    /// waits for it.
    pub(crate) fn rouse(&self, d: EndpointId) {
        for sp in self.all() {
            let (url, k) = {
                let b = lock(&sp.book);
                let k = sp.inv.as_ref().map(|i| b.key(i).0);
                let mut all = b.st.pushes_of(None).into_iter();
                (all.find(|p| p.0 == d).map(|p| p.1), k)
            };
            let to = url.as_deref().and_then(crate::push::lane_of);
            if let (Some(to), Some(k)) = (to, k) {
                let f =
                    [&[crate::push::OP][..], &ev_label(&sp.id, &k)].concat();
                let dir = self.dir.clone();
                tokio::spawn(
                    async move { crate::push::event(&dir, &to, &f).await },
                );
                return;
            }
        }
    }

    /// iroh asleep now if nothing needs it (not waiting out `IDLE`).
    pub async fn quiet(&self) -> bool {
        self.rest_iroh(Duration::ZERO).await
    }

    /// Whether iroh is up (else asleep: our lane wakes it).
    pub fn iroh_up(&self) -> bool {
        self.ep().is_some()
    }

    /// iroh asleep if nothing needs it: no connection open, no call, no
    /// gossip, untouched `idle`; never a storage node's, nor one our lane
    /// cannot wake. Whether it slept.
    pub(super) async fn rest_iroh(&self, idle: Duration) -> bool {
        let quiet = |ep: &Endpoint| {
            let m = &ep.metrics().socket;
            let open = m.num_conns_opened.get() > m.num_conns_closed.get();
            !ep.is_closed()
                && !open
                && self.serve.is_none()
                && self.wakes()
                && self.in_call().is_none()
                && lock(&self.links).is_empty()
                && self.all().iter().all(|s| s.tx().is_none())
                && lock(&self.used).elapsed() >= idle
        };
        let _one = self.binding.lock().await;
        let i = {
            let mut i = write(&self.iroh);
            match i.as_ref().is_some_and(|i| quiet(&i.ep)) {
                true => i.take(),
                false => None,
            }
        };
        let Some(i) = i else { return false };
        let moved = crate::usage::counted(&i.ep);
        crate::usage::note(&self.dir, "iroh asleep", moved);
        let _ = i.router.shutdown().await;
        i.ep.close().await;
        true
    }

    /// Woken, in front, or on a new network: re-probe, re-dial, catch up now.
    pub async fn resume(self: &Arc<Self>) {
        self.send.kick.notify_waiters();
        crate::push::reset().await;
        if let Some(ep) = self.ep() {
            ep.network_change().await;
        }
        for sp in self.all() {
            let peers = self.bootstrap(&sp);
            peers.iter().take(3).for_each(|p| self.sync(&sp, *p));
            if let Some(tx) = sp.tx() {
                let _ = tx.join_peers(peers).await;
            }
            self.here(&sp).await;
        }
    }

    /// Whether our link to `d` is direct (peer to peer), not relayed.
    pub(super) async fn direct(&self, d: EndpointId) -> bool {
        self.path(d).await == "direct"
    }

    /// How we reach `d` now: "direct", the relay's address, or "none".
    pub(super) async fn path(&self, d: EndpointId) -> String {
        use iroh::endpoint::TransportAddrUsage::Active;
        let Some(ep) = self.ep() else { return "none".into() };
        let Some(i) = ep.remote_info(d).await else {
            return "none".into();
        };
        let on: Vec<_> =
            i.addrs().filter(|a| matches!(a.usage(), Active)).collect();
        match on.iter().find(|a| a.addr().is_ip()) {
            Some(_) => "direct".into(),
            None => on.first().map_or("none".into(), |a| match a.addr() {
                iroh::TransportAddr::Relay(u) => u.to_string(),
                _ => "none".into(),
            }),
        }
    }

    /// A link to `to` made and closed: how long it took (none: not in
    /// 10 s), and how it went.
    async fn probe(&self, to: EndpointAddr, alpn: &[u8]) -> serde_json::Value {
        let (d, t) = (to.id, Instant::now());
        let ten = Duration::from_secs(10);
        let c = tokio::time::timeout(ten, self.connect(to, alpn)).await;
        let Ok(Ok(c)) = c else {
            return serde_json::json!({ "ms": null, "path": "none" });
        };
        let ms = t.elapsed().as_millis() as u64;
        let path = self.path(d).await;
        c.close(0u32.into(), b"doctor");
        serde_json::json!({ "ms": ms, "path": path })
    }

    /// What reaching `user` (else our own devices) takes now: our relay,
    /// and a link timed to each of their devices and push servers. A push
    /// server is reached, not asked: nothing rings.
    pub async fn doctor(
        self: &Arc<Self>,
        user: Option<Id>,
    ) -> serde_json::Value {
        use iroh::Watcher as _;
        // Started just now (chatir --doctor): our relay first.
        if let Ok(ep) = self.up().await {
            let five = Duration::from_secs(5);
            let _ = tokio::time::timeout(five, ep.online()).await;
        }
        let who = user.unwrap_or_else(|| self.id());
        let (mut devs, mut push) = (BTreeMap::new(), BTreeSet::new());
        for id in self.spaces() {
            self.view(&id, |s| {
                for (d, t) in s.devices_of(&who) {
                    let at = devs.entry(d).or_insert(t);
                    *at = (*at).max(t);
                }
                for (d, url) in s.pushes_of(Some(&who)) {
                    push.extend(
                        url.split_whitespace().map(|r| (d, r.to_string())),
                    );
                }
            });
        }
        devs.remove(&self.device());
        let devices = devs.into_iter().map(|(d, wrote)| async move {
            let mut v = self.probe(d.into(), sync::ALPN).await;
            v["device"] = d.fmt_short().to_string().into();
            v["name"] = self.label(&d).into();
            v["wrote"] = wrote.into();
            v
        });
        let relay = self.ep().map(|e| e.home_relay_status().get());
        let relay = relay.iter().flatten().map(
            |s| serde_json::json!({ "url": s.url(), "up": s.is_connected() }),
        );
        let order: Vec<_> =
            homes(&self.net).iter().map(RelayUrl::to_string).collect();
        let lane = self.lane_relay();
        serde_json::json!({
            "relay": relay.collect::<Vec<_>>(),
            "order": order,
            "lane": lane,
            "wakes": self.wakes(),
            "devices": n0_future::join_all(devices).await,
        })
    }

    /// The devices worth listing (ours, those allowed, any linked this
    /// run), newest news first: whose, our name for it, whether it is up
    /// (as a space we share last heard), since when, when it last wrote,
    /// how it is reached now, and what it and we moved this run.
    pub async fn connections(self: &Arc<Self>) -> Vec<serde_json::Value> {
        let (me, seen, allowed) =
            (self.id(), self.traffic.seen(), self.allowed());
        let mut devs: BTreeSet<EndpointId> =
            self.devices().into_iter().map(|d| d.0).collect();
        devs.extend(allowed.keys().chain(seen.keys()));
        devs.remove(&self.device());
        // Its newest word in any space: here or gone, and when.
        let mut up: HashMap<EndpointId, (bool, u64)> = HashMap::new();
        for sp in self.all() {
            for ((_, d), (here, ts, _)) in lock(&sp.seen).iter() {
                let e = up.entry(*d).or_insert((*here, *ts));
                if *ts > e.1 {
                    *e = (*here, *ts);
                }
            }
        }
        let row = |d: EndpointId| {
            let user = self.whose(&d);
            let wrote = user.and_then(|u| {
                let of = |s: &Id| self.view(s, |st| st.devices_of(&u));
                let all = self.spaces().into_iter().filter_map(|s| of(&s));
                all.flatten().filter(|x| x.0 == d).map(|x| x.1).max()
            });
            let ok = allowed.contains_key(&d);
            let name = allowed.get(&d).filter(|n| !n.is_empty()).cloned();
            let t = seen.get(&d);
            let (on, at) = up.get(&d).copied().unzip();
            async move {
                serde_json::json!({
                    "id": d, "short": d.fmt_short().to_string(),
                    "name": name.or_else(|| self.label(&d)),
                    "user": user, "uname": user.and_then(|u| self.name_of(&u, None)),
                    "own": user == Some(me), "allowed": ok,
                    "up": on, "since": at, "wrote": wrote, "path": self.path(d).await,
                    "sent": t.map_or(0, |t| t.sent), "got": t.map_or(0, |t| t.got),
                    "open": t.map_or(0, |t| t.open), "linked": t.map(|t| t.last),
                })
            }
        };
        let mut v = n0_future::join_all(devs.into_iter().map(row)).await;
        let news = |x: &serde_json::Value| {
            ["since", "wrote", "linked"]
                .iter()
                .filter_map(|k| x[k].as_u64())
                .max()
        };
        v.sort_by_key(|x| std::cmp::Reverse(news(x)));
        v
    }

    /// Our key for a relay (`url#key`, from whoever runs it), or none
    /// (`url`): kept in `relay-keys`, used as iroh next starts (at once,
    /// if it is idle).
    pub async fn relay_key(&self, line: &str) -> Result<Option<bool>> {
        let (url, k) = relay(line).context("not a relay's address")?;
        ensure!(
            k.is_empty() || k.len() >= 32,
            "a key is 32 characters or more"
        );
        let f = self.dir.join("relay-keys");
        let old = std::fs::read_to_string(&f).unwrap_or_default();
        let mut v: Vec<&str> = old
            .lines()
            .filter(|l| relay(l).is_none_or(|r| r.0 != url))
            .collect();
        let new = format!("{url}#{k}");
        if !k.is_empty() {
            v.push(&new);
        }
        store::write(&f, v.join("\n").as_bytes())?;
        // iroh idle: asleep now, so it wakes with the key (else at the next
        // start).
        self.rest_iroh(Duration::ZERO).await;
        Ok(match k.is_empty() {
            true => None,
            false => key_works(&url, k).await,
        })
    }

    /// Our relay keys, each as its relay answers now (`key_works`).
    /// Each relay key: its relay, whether that relay takes it now, and
    /// whether iroh runs with it (none while iroh sleeps: it wakes with
    /// it); and the key, but its last four characters hidden.
    pub async fn relay_keys(&self) -> Vec<serde_json::Value> {
        let on = read(&self.iroh).as_ref().map(|i| i.keys.clone());
        let checks = keys(&self.dir).into_iter().map(|(u, k)| {
            let on = on.as_ref().map(|v| v.iter().any(|x| x.0 == u && x.1 == k));
            async move {
                let ok = key_works(&u, &k).await;
                let tail = k.char_indices().rev().nth(3).map_or(0, |c| c.0);
                let shown = format!("…{}", &k[tail..]);
                serde_json::json!({ "url": u.to_string(), "ok": ok, "on": on, "key": shown })
            }
        });
        n0_future::join_all(checks).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_fetch_only_public_https() {
        let ok = |u: &str| open(&reqwest::Url::parse(u).unwrap());
        assert!(ok("https://example.com/a") && ok("https://1.1.1.1/"));
        for u in [
            "http://example.com/",
            "https://10.0.0.1/",
            "https://127.0.0.1:8080/",
            "https://[::ffff:192.168.1.1]/",
            "https://169.254.169.254/latest",
        ] {
            assert!(!ok(u), "{u}");
        }
        assert!(!host_name().is_empty());
    }
}
