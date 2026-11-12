//! Short codes (`k7m-x9q-p3a`) for an invite or a link code: irpair's (one
//! minute, one try; the long code sealed to the taker's fresh X-Wing key,
//! an optional password one layer more).

use crate::{Net, node::endpoint};
use anyhow::{Result, ensure};

/// A code is irch's alone: irpair salts it with this.
const APP: &str = "irch";

/// Where a giver homes: the relays we would.
fn relays(net: &Net) -> Vec<iroh::RelayUrl> {
    match net {
        Net::Default => {
            crate::node::RELAYS.iter().filter_map(|r| r.parse().ok()).collect()
        }
        Net::Relay(r) => r.clone(),
        _ => vec![],
    }
}

/// A short code for `long` (an invite or link code), answered a minute;
/// with a password, the taker needs it too.
pub async fn give(
    net: &Net,
    long: String,
    password: Option<String>,
) -> Result<String> {
    let ep = endpoint(net, None);
    irpair::give(ep, APP, long.as_bytes(), password.as_deref()).await
}

/// The long code a short one stands for, while its giver answers.
pub async fn take(
    net: &Net,
    code: &str,
    password: Option<&str>,
) -> Result<String> {
    let ep = endpoint(net, None);
    let long = irpair::take(ep, &relays(net), APP, code, password).await?;
    let long = String::from_utf8(long.to_vec())?;
    ensure!(long.starts_with("irch:"), "not an irch code");
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
    let long = "irch:invite:abc".to_string();
    let code = give(&net, long.clone(), None).await.unwrap();
    assert_eq!(code.len(), 11, "xxx-xxx-xxx");
    assert_eq!(take(&net, &code.to_uppercase(), None).await.unwrap(), long);
    assert!(take(&net, &code, None).await.is_err(), "spent");
    // With a password: the taker needs it too.
    let pw = Some("horse battery".to_string());
    let code = give(&net, long.clone(), pw).await.unwrap();
    let got = take(&net, &code, None).await;
    assert!(got.unwrap_err().to_string().contains("has a password"));
    let got = take(&net, &code, Some("wrong")).await;
    assert!(got.unwrap_err().to_string().contains("wrong password"));
    let got = take(&net, &code, Some("horse battery")).await.unwrap();
    assert_eq!(got, long, "asked for after its one try, opened here");
}
