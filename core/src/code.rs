//! Short codes (`k7m-x9q-p3a-b2c`) for an invite or a link code: pairir's (one
//! minute, one try, CPace; the long code sealed to the taker's fresh X-Wing key,
//! an optional password one layer more).

use crate::Net;
use anyhow::{Result, bail, ensure};
use pairir::End;

/// A code is chatir's alone: pairir salts it with this.
const APP: &str = "chatir";

/// Where giver and taker meet: our own relays, or (none) the lab's.
fn relays(net: &Net) -> Result<Vec<iroh::RelayUrl>> {
    match net {
        Net::Default => Ok(vec![]),
        Net::Relay(r) => Ok(r.clone()),
        _ => bail!("a short code is met at a relay: use the long code"),
    }
}

/// A short code for `long` (an invite or link code), answered a minute,
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
    ensure!(long.starts_with("chatir:"), "not a chatir code");
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
    let long = "chatir:invite:abc".to_string();
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
