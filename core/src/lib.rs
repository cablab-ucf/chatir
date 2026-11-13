//! chatir: a space is a grow-only set of sealed ops on members' devices,
//! folded deterministically, reconciled over iroh.

pub mod api;
mod call;
pub mod code;
pub mod crypto;
pub mod link;
mod node;
pub mod notes;
pub mod op;
pub mod push;
pub mod release;
mod send;
pub mod state;
mod store;
mod sync;
pub mod usage;
#[cfg(feature = "voice")]
pub mod voice;

pub use irnet;
pub use node::{
    Config, Event, Level, Net, Node, Notice, host_name, http, storage_name,
};
pub use op::{File, Invite, Op, Role};
pub use state::{Member, Message, State, Thread};

use data_encoding::HEXLOWER_PERMISSIVE as HEX;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

macro_rules! bytes {
    ($($t:ident $n:literal),*) => {$(
        /// Raw bytes on the wire, hex in JSON.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $t(pub [u8; $n]);
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(&HEX.encode(&self.0))
            }
        }
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(&HEX.encode(&self.0[..4]))
            }
        }
        impl FromStr for $t {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> anyhow::Result<Self> {
                let v = HEX.decode(s.trim().as_bytes())?;
                Ok(Self(v.try_into().map_err(|_| anyhow::anyhow!("bad id"))?))
            }
        }
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S)
            -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.collect_str(self)
                } else {
                    self.0.serialize(s)
                }
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D)
            -> Result<Self, D::Error> {
                if d.is_human_readable() {
                    let s = String::deserialize(d)?;
                    s.parse().map_err(de::Error::custom)
                } else {
                    <[u8; $n]>::deserialize(d).map(Self)
                }
            }
        }
    )*};
}
bytes!(Id 32, Ch 8);

impl Id {
    /// Named by the id's prefix, so everyone starting it at once agrees.
    pub fn thread(&self) -> Ch {
        Ch(self.0[..8].try_into().expect("8 of 32"))
    }
}

/// This computer's storage node code, from `chatir --install` or nixos.nix.
pub fn local_storage() -> Option<String> {
    // /run is root's on Linux; elsewhere (C:\\run) anyone could plant one.
    let run = cfg!(target_os = "linux").then(|| "/run/chatir/code".into());
    let p = std::env::var_os("CHATIR_CODE_FILE").map(Into::into).or(run);
    let mine = dirs::data_dir().map(|d| d.join("chatir-serve").join("code"));
    let read = |p| std::fs::read_to_string(p).ok();
    let code = p.and_then(read).or_else(|| mine.and_then(read))?;
    let code = code.trim().to_string();
    (!code.is_empty()).then_some(code)
}

/// Its own folder, or `root` itself for the first (from before profiles).
pub fn profile(root: &std::path::Path) -> std::path::PathBuf {
    let n = std::fs::read_to_string(root.join("profile")).unwrap_or_default();
    match n.trim() {
        "" => root.into(),
        n => root.join("profiles").join(n),
    }
}

/// irch's folder `from`, taken once as `to`: chatir is irch renamed, the
/// same people, spaces and files. A chatir folder from before (0.4.0-1,
/// whose people are others now) is kept beside it, as `…-0.4`, and this
/// starts afresh. `irch` in `to` marks one past this: nothing after
/// (an irch run since) displaces it.
pub fn adopt(
    from: Option<&std::path::Path>,
    to: &std::path::Path,
) -> anyhow::Result<()> {
    let mark = to.join("irch");
    let has = |d: &std::path::Path| {
        ["seed", "profiles"].iter().any(|f| d.join(f).exists())
    };
    if !mark.exists() {
        let from = from.filter(|f| has(f));
        if let Some(f) = from {
            let lock = std::fs::File::open(profile(f).join("lock"));
            if lock.is_ok_and(|l| l.try_lock().is_err()) {
                let f = f.display();
                anyhow::bail!("irch is running on {f}: stop it, and retry");
            }
        }
        if to.exists() && (has(to) || from.is_some()) {
            let mut aside = to.as_os_str().to_owned();
            aside.push("-0.4");
            std::fs::rename(to, aside)?;
        }
        if let Some(f) = from {
            std::fs::rename(f, to)?;
        }
    }
    std::fs::create_dir_all(to)?;
    Ok(std::fs::write(mark, "")?)
}

/// What signing out takes away.
pub const MINE: [&str; 21] = [
    "seed",
    "device",
    "local",
    "prefs",
    "closed",
    "spaces",
    "files",
    "blobs",
    "offered",
    "wake.json",
    "chatir.log",
    // Our lane and mailbox: kept, the next identity here would be heard
    // (and known) where this one was.
    "lane",
    "mailbox",
    "bells",
    "sealed",
    "shared",
    "sends",
    "allowed",
    "phonebook",
    "shown",
    "rung",
];

/// Unix time in milliseconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[test]
fn irch_is_taken_once() {
    let d = std::env::temp_dir().join(format!("chatir-adopt-{}", now()));
    let (irch, to) = (d.join("irch"), d.join("chatir"));
    let put = |p: &std::path::Path, f, s| {
        std::fs::create_dir_all(p).unwrap();
        std::fs::write(p.join(f), s).unwrap();
    };
    put(&irch, "seed", "theirs");
    put(&to, "seed", "0.4's");
    adopt(Some(&irch), &to).unwrap();
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap();
    assert_eq!(read(to.join("seed")), "theirs");
    assert_eq!(read(d.join("chatir-0.4").join("seed")), "0.4's");
    // An irch run since starts afresh: it displaces nothing.
    put(&irch, "seed", "new");
    adopt(Some(&irch), &to).unwrap();
    assert_eq!(read(to.join("seed")), "theirs");
    // A phone's 0.4 folder, no irch beside: set aside, a fresh start.
    let phone = d.join("phone");
    put(&phone, "seed", "0.4's");
    adopt(None, &phone).unwrap();
    assert!(!phone.join("seed").exists() && d.join("phone-0.4").exists());
    let _ = std::fs::remove_dir_all(d);
}
