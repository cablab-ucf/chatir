//! Ringing a sleeping phone through a push server: each space gets its own
//! ring token, from one kept secret, the space and its epoch: after a
//! rekey, whoever it left out rings nothing. Members ring the push server
//! with it (QUIC); pushir hands it on. With no push server, a phone's lane
//! alone is rung, by a labelled event (node.rs).

use crate::crypto::kdf;
use crate::op::uncode;
use crate::{Id, store};
use anyhow::{Result, ensure};
use iroh::{Endpoint, EndpointAddr, TransportAddr};
use std::path::Path;
use std::time::Duration;
use tokio::time::timeout;

pub const ALPN: &[u8] = b"chatir/wake/1";
const SLOW: Duration = Duration::from_secs(15);
/// A space's id and one sealed op.
pub(crate) const MAX: usize = 64 * 1024;
/// token ‖ space ‖ epoch tag
pub(crate) const HEAD: usize = 16 + 32 + 8;

pub type News = (crate::Id, Vec<u8>);

/// The secret, kept so tokens hold.
fn secret(dir: &Path) -> Result<[u8; 16]> {
    let f = dir.join("bell");
    let k = match std::fs::read(&f) {
        Ok(k) if k.len() == 48 => k,
        _ => {
            std::fs::create_dir_all(dir)?;
            let k = crate::crypto::rand::<48>().to_vec();
            store::write(&f, &k)?;
            k
        }
    };
    Ok(k[32..].try_into()?)
}

/// A space's ring token for a push server to know us by.
pub fn push_token(dir: &Path, space: &Id, tag: &[u8; 8]) -> Result<[u8; 16]> {
    let k = kdf("chatir bell", &[&secret(dir)?, &space.0, tag]);
    Ok(k[..16].try_into()?)
}

/// A push server rung (`chatir:ring:`; relays https only: any member names
/// them); taken, it hands the ring on to the phone.
pub async fn ring(
    ep: &Endpoint,
    url: &str,
    space: &Id,
    raw: Option<&[u8]>,
) -> Result<()> {
    let (to, tag, token): (EndpointAddr, [u8; 8], [u8; 16]) =
        uncode("ring", url)?;
    // With a relay to meet at, no private address: a member could aim
    // every ringer at a LAN. Without one (LAN only), any.
    let relayed = to.addrs.iter().any(|a| a.is_relay());
    let ok = |a: &&TransportAddr| match a {
        TransportAddr::Relay(u) => u.scheme() == "https",
        TransportAddr::Ip(a) => !relayed || crate::node::public(&a.ip()),
        _ => false,
    };
    let addrs: Vec<_> = to.addrs.iter().filter(ok).take(8).cloned().collect();
    let go = async {
        let c =
            ep.connect(EndpointAddr::from_parts(to.id, addrs), ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        let mut m = [&token[..], &space.0, &tag].concat();
        if let Some(raw) = raw.filter(|r| r.len() <= MAX) {
            m.extend_from_slice(raw);
        }
        tx.write_all(&m).await?;
        tx.finish()?;
        let got = rx.read_to_end(1).await?;
        c.close(0u8.into(), b"");
        ensure!(got == [2], "not its code, or an old one");
        Ok(())
    };
    timeout(SLOW, go).await?
}
