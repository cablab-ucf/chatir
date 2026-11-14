//! Short codes (`k7m-x9q-p3a-b2c`) for any code (an invite, a contact, link or
//! storage code, a device's key): pairir's (one minute, one try, CPace; the
//! long code sealed to the taker's fresh X-Wing key, an optional password one
//! layer more).

use crate::Net;
use anyhow::{Result, bail, ensure};
use pairir::End;

/// Our codes' own: pairir salts them with this (the protocol's name, not
/// the app's).
const APP: &str = crate::crypto::PROTO;

/// Where giver and taker meet: our own relays, or (none) the lab's.
fn relays(net: &Net) -> Result<Vec<iroh::RelayUrl>> {
    match net {
        Net::Default => Ok(vec![]),
        Net::Relay(r) => Ok(r.clone()),
        _ => bail!("a short code is met at a relay: use the long code"),
    }
}

/// What `s` is: a code's kind ("join", "link", "host"), or "key" for a
/// device's key (64 hex); any `scheme:` before it ignored.
pub fn kind(s: &str) -> Option<&'static str> {
    let k = s.trim().rsplit(':').next()?;
    let key = k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit());
    if key { Some("key") } else { crate::op::kind(s) }
}

/// `text` as a QR code, an SVG.
pub fn qr(text: &str) -> Result<String> {
    let q = qrcode::QrCode::new(text.as_bytes())?;
    let mut svg = q.render::<qrcode::render::svg::Color>();
    Ok(svg.quiet_zone(true).min_dimensions(240, 240).build())
}

/// A short code for `long` (any code: [`kind`]), answered a minute,
/// and how it ends; with a password, the taker needs it too.
pub async fn give(
    net: &Net,
    long: String,
    password: Option<String>,
) -> Result<(String, tokio::task::JoinHandle<End>)> {
    pairir::give(&relays(net)?, APP, long.as_bytes(), password.as_deref()).await
}

/// The long code a short one stands for, while its giver answers.
pub async fn take(
    net: &Net,
    code: &str,
    password: Option<&str>,
) -> Result<String> {
    let long = pairir::take(&relays(net)?, APP, code, password).await?;
    let long = String::from_utf8(long.to_vec())?;
    ensure!(kind(&long).is_some(), "not a code");
    Ok(long)
}

/// Given on one device, taken on another over a relay: the long code,
/// sealed to the taker; then spent.
#[tokio::test(flavor = "multi_thread")]
async fn a_short_code_hands_over_its_long_one() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}/", relay.http_addr().unwrap());
    let net = Net::Relay(vec![url.parse().unwrap()]);
    let long = crate::op::code("join", &7u8);
    let (code, end) = give(&net, long.clone(), None).await.unwrap();
    assert_eq!(code.len(), 15, "xxx-xxx-xxx-xxx");
    assert_eq!(take(&net, &code.to_uppercase(), None).await.unwrap(), long);
    assert_eq!(end.await.unwrap(), End::Taken);
    assert!(take(&net, &code, None).await.is_err(), "spent");
    assert!(give(&Net::Lan, long.clone(), None).await.is_err(), "no relay");
    // With a password: the taker needs it too.
    let pw = Some("horse battery".to_string());
    let (code, _) = give(&net, long.clone(), pw).await.unwrap();
    let got = take(&net, &code, None).await;
    assert!(got.unwrap_err().to_string().contains("has a password"));
    let got = take(&net, &code, Some("wrong")).await;
    assert!(got.unwrap_err().to_string().contains("wrong password"));
    let got = take(&net, &code, Some("horse battery")).await.unwrap();
    assert_eq!(got, long, "asked for after its one try, opened here");
}

/// Every code there is, bare or as a camera app hands it on; a device's
/// key too, short-coded like the rest.
#[tokio::test(flavor = "multi_thread")]
async fn any_code_is_known_and_short_coded() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    for k in ["join", "link", "host"] {
        let c = crate::op::code(k, &7u8);
        assert_eq!(kind(&c), Some(k));
        assert_eq!(kind(&format!("chatir:{c}")), Some(k), "a link's");
    }
    let key = "ab".repeat(32);
    assert_eq!(kind(&key), Some("key"));
    assert_eq!(kind(&format!(" chatir:{key} ")), Some("key"));
    assert_eq!(kind(&key[1..]), None, "63 hex");
    assert_eq!(kind("hello"), None);
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}/", relay.http_addr().unwrap());
    let net = Net::Relay(vec![url.parse().unwrap()]);
    let (code, _) = give(&net, key.clone(), None).await.unwrap();
    assert_eq!(take(&net, &code, None).await.unwrap(), key);
}
