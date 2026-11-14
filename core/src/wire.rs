//! The transport, pinned as v0.4.5 and later peers speak it (CLAUDE.md).
//! Each pin is a hash of the source that decides bytes on the wire, its
//! comments and spacing aside: a pin that moves is a wire change. Only an
//! enum variant appended at the end keeps old and new talking (postcard:
//! a field added anywhere, or anything reordered, breaks both ways); for
//! anything else the old form stays beside the new.

use std::path::PathBuf;

/// (file, item or "" for the whole file, its pin).
const PINS: &[(&str, &str, &str)] = &[
    ("crypto.rs", "", "7cf837a7ac906973"),
    ("op.rs", "", "df6e8c1b1f7fe41d"),
    ("sync.rs", "", "1e3defc8f3e8ebda"),
    ("call.rs", "", "7a747ca9595a581f"),
    ("code.rs", "const APP", "6bdd9ce91f92c8b9"),
    ("push.rs", "const ALPN", "25b892e629921b57"),
    ("push.rs", "enum Ask", "1f27d845abc7efcd"),
    ("push.rs", "const OP", "5bb043d3aec2bb22"),
    ("push.rs", "const ACK", "bcf22c66328fba4d"),
    ("push.rs", "const DIAL", "7d0e2025576eaa56"),
    // 0.4.9: kind TEXT appended; before it, `split` returns none for it
    // (`<= DIAL`), so an older receiver ignores a text by lane.
    ("push.rs", "fn split", "8d5901fd95c95940"),
    ("push.rs", "const TEXT", "14433a31acdfd4f9"),
    ("send.rs", "const ALPN", "82a3fd9ab3daf32b"),
    ("send.rs", "enum Ask", "cb8dd3892e8d9ad7"),
    ("send.rs", "enum End", "3ac0d6f8f7c94920"),
    ("send.rs", "enum To", "7c6ab3e6a1042114"),
    ("send.rs", "const BLOCK", "09d884f7ea0bc019"),
    ("send.rs", "const OK", "1b387728e40fdc7d"),
    ("send.rs", "const NO", "858908e685f4e720"),
    ("send.rs", "const BUSY", "1f42b715a003692d"),
    ("send.rs", "const GONE", "9c5a22527544cd77"),
    ("voice.rs", "fn frame", "b428a2685ac0358e"),
    ("node.rs", "const BLOB", "ed1e76cf336d6d7f"),
    ("node.rs", "const DIRECT", "2881192e0d2cf86e"),
    ("node.rs", "const HAND", "e5a845cf500d3b2d"),
    ("node.rs", "enum Ask", "40bef832f31f7034"),
    ("node.rs", "fn ev_label", "4efd4168d32558a7"),
    ("node.rs", "fn ev_ack", "fd5fcada693d870d"),
    ("node.rs", "fn hold_token", "7bd0b4878353508d"),
    ("node.rs", "fn level_of", "83a1628cc52cfd4f"),
    ("node.rs", "fn tell_push", "d80cef2949338a0b"),
];

/// Comments and spacing gone: what a formatter or a note may change.
fn norm(s: &str) -> String {
    let code = s.lines().map(|l| match l.trim_start().starts_with("//") {
        true => "",
        false => l.split(" // ").next().unwrap_or(l),
    });
    code.flat_map(str::split_whitespace).collect::<Vec<_>>().join(" ")
}

/// Item `head` ("enum Ask", "fn split", "const OP") in `src`: from its
/// line to its `;` (a const) or the brace that closes its first.
fn item(src: &str, head: &str) -> Option<String> {
    let (kind, name) = head.split_once(' ')?;
    let at = src.lines().scan(0, |o, l| {
        let start = *o;
        *o += l.len() + 1;
        Some((start, l))
    });
    let start = at
        .filter(|(_, l)| {
            let w: Vec<&str> =
                l.split(|c: char| !c.is_alphanumeric() && c != '_').collect();
            w.windows(2).any(|p| p[0] == kind && p[1] == name)
                && !l.trim_start().starts_with("//")
        })
        .map(|(s, _)| s)
        .next()?;
    let rest = &src[start..];
    if kind == "const" {
        return Some(rest[..=rest.find(';')?].to_string());
    }
    let open = rest.find('{')?;
    let mut depth = 0;
    for (i, c) in rest[open..].char_indices() {
        depth += match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        };
        if depth == 0 {
            return Some(rest[..=open + i].to_string());
        }
    }
    None
}

fn pin(dir: &std::path::Path, file: &str, head: &str) -> String {
    let src = std::fs::read_to_string(dir.join(file)).expect(file);
    let text = match head {
        "" => src,
        h => item(&src, h).unwrap_or_else(|| panic!("{h} not in {file}")),
    };
    blake3::hash(norm(&text).as_bytes()).to_hex()[..16].to_string()
}

/// `WIRE_SRC` checks another tree (an older release's `core/src`).
#[test]
fn the_wire_is_as_v0_4_5() {
    let dir = std::env::var_os("WIRE_SRC").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        PathBuf::from,
    );
    let moved: Vec<String> = PINS
        .iter()
        .filter_map(|(f, h, want)| {
            let got = pin(&dir, f, h);
            (got != *want).then(|| format!("{f} {h:?}: {got} (pinned {want})"))
        })
        .collect();
    assert!(
        moved.is_empty(),
        "the wire changed (CLAUDE.md: v0.4.5 and later peers must still \
         talk); keep the old form beside the new, then repin:\n{}",
        moved.join("\n")
    );
}

#[test]
fn wire_constants_are_as_v0_4_5() {
    assert_eq!(crate::crypto::PROTO, "spaces 2026-11");
    assert_eq!(crate::crypto::STREAM, b"spaces1\0");
    assert_eq!(crate::sync::ALPN, b"spaces/sync/1");
    assert_eq!(crate::call::ALPN, b"spaces/av/1");
    assert_eq!(crate::push::ALPN, b"spaces/push/1");
    assert_eq!(crate::send::ALPN, b"spaces/send/1");
    assert_eq!(
        (crate::push::OP, crate::push::ACK, crate::push::DIAL),
        (0, 1, 2)
    );
    assert_eq!(crate::push::TEXT, 3, "appended in 0.4.9");
    let k = crate::crypto::kdf("wire", &[b"v0.4.5"]);
    assert_eq!(data_encoding::HEXLOWER.encode(&k)[..16], *"035ad9700806b658");
    for (kind, first) in [("join", 'a'), ("link", 'a'), ("host", 'a')] {
        let c = crate::op::code(kind, &7u8);
        assert_eq!(crate::op::kind(&c), Some(kind));
        assert!(c.starts_with(first), "{kind}: {c}");
    }
    assert_eq!(crate::op::code("join", &7u8), "aedq");
    assert_eq!((pushir::FRAME, pushir::MAX), (1024, 966));
}
