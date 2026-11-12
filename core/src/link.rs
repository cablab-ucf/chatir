//! Link previews, made by a message's sender as it is sent: only the
//! sender's device asks, and the preview travels sealed in the space.
//! Papers come from their index (arXiv, PubMed, Crossref by DOI), which
//! answers where publishers refuse readers and PDFs say nothing; other
//! pages are read as text for their meta tags, never run.

use crate::op::{Link, clip};
use anyhow::{Result, bail, ensure};
use std::collections::HashMap;

// YouTube's tags come after 750 KB of scripts; reading stops at </head>.
const PAGE: usize = 2 << 20;
const IMAGE: usize = 2 << 20;

/// The first https link in a message.
pub fn first(text: &str) -> Option<&str> {
    let edge = |c: char| "<>()[]{}\"'`,.;:!?*_~|".contains(c);
    let words = text.split_whitespace().map(|w| w.trim_matches(edge));
    words.into_iter().find(|w| w.starts_with("https://") && w.len() > 8)
}

/// Its title, description and image (bytes and extension), if any.
pub async fn fetch(url: &str) -> Result<(Link, Option<(Vec<u8>, &str)>)> {
    let web = crate::node::web();
    if let Some((title, text)) = paper(&web, url).await {
        let (title, text) = (clip(&title, 200), clip(&text, 400));
        return Ok((Link { url: url.into(), title, text, image: None }, None));
    }
    let (page, _) = get(&web, url, PAGE, "text/html").await?;
    let html = String::from_utf8_lossy(&page);
    let meta = metas(&html);
    let pick = |ks: &[&str]| ks.iter().find_map(|k| meta.get(*k)).cloned();
    let title =
        pick(&["og:title", "twitter:title", "citation_title", "dc.title"])
            .or_else(|| title(&html));
    let text = pick(&[
        "og:description",
        "twitter:description",
        "citation_abstract",
        "description",
    ]);
    let (title, text) = (title.unwrap_or_default(), text.unwrap_or_default());
    ensure!(!title.is_empty() || !text.is_empty(), "nothing to preview");
    let image = match pick(&["og:image", "twitter:image"]) {
        Some(src) => {
            let at = reqwest::Url::parse(url)?.join(&unescape(&src))?;
            // As the page itself: an address in a URL skips the resolver.
            match crate::node::open(&at) {
                true => get(&web, at.as_str(), IMAGE, "image/").await.ok(),
                false => None,
            }
        }
        None => None,
    };
    let image = image.and_then(|(b, kind)| {
        let ext = match kind.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/avif" => "avif",
            _ => return None,
        };
        Some((b, ext))
    });
    let link = Link {
        url: url.into(),
        title: clip(&unescape(&title), 200),
        text: clip(&unescape(&text), 400),
        image: None,
    };
    Ok((link, image))
}

/// A paper's title, and its authors, venue, year and abstract in a line:
/// from arXiv (any abs, pdf or html link), PubMed, or Crossref (a DOI in
/// the link, or a ScienceDirect PII).
async fn paper(web: &reqwest::Client, url: &str) -> Option<(String, String)> {
    let u = reqwest::Url::parse(url).ok()?;
    let host = u.host_str()?.trim_start_matches("www.");
    let path = percent_decode(u.path());
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let json = async |q: String| {
        let (b, _) = get(web, &q, PAGE, "").await.ok()?;
        serde_json::from_slice::<serde_json::Value>(&b).ok()
    };
    if host.ends_with("arxiv.org") && seg.len() >= 2 {
        let id = seg[1..].join("/");
        let id = id.trim_end_matches(".pdf");
        let q = format!("https://export.arxiv.org/api/query?id_list={id}");
        let (b, _) = get(web, &q, PAGE, "").await.ok()?;
        let x = String::from_utf8_lossy(&b);
        let e = &x[x.find("<entry>")?..];
        let names = e.split("<name>").skip(1);
        let au =
            names.filter_map(|n| Some(n[..n.find("</name>")?].to_string()));
        let year = tag(e, "published").map(|d| year(&d));
        let line = byline(au.collect(), Some("arXiv".into()), year);
        return Some((
            squash(&tag(e, "title")?),
            line + &abs(tag(e, "summary")),
        ));
    }
    if host == "pubmed.ncbi.nlm.nih.gov" {
        let id =
            seg.first().filter(|s| s.bytes().all(|b| b.is_ascii_digit()))?;
        let q = format!(
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi\
             ?db=pubmed&id={id}&retmode=json"
        );
        let r = json(q).await?;
        let r = &r["result"][*id];
        let au = r["authors"].as_array()?.iter();
        let au = au.filter_map(|a| Some(a["name"].as_str()?.to_string()));
        let venue = r["fulljournalname"].as_str().or(r["source"].as_str());
        let year = r["pubdate"].as_str().map(year);
        let line = byline(au.collect(), venue.map(Into::into), year);
        return Some((r["title"].as_str()?.to_string(), line));
    }
    let doi = doi(host, &path);
    let pii = seg.iter().position(|s| *s == "pii").and_then(|i| seg.get(i + 1));
    let w = match (doi, pii) {
        (Some(d), _) => json(format!("https://api.crossref.org/works/{d}"))
            .await
            .map(|r| r["message"].clone())?,
        (_, Some(p)) => {
            let q =
                "https://api.crossref.org/works?rows=1&filter=alternative-id:";
            json(format!("{q}{p}")).await?["message"]["items"][0].clone()
        }
        _ => return None,
    };
    let title = w["title"][0].as_str()?.to_string();
    let au = w["author"].as_array().into_iter().flatten();
    let au = au.filter_map(|a| {
        let f = a["family"].as_str()?;
        Some(a["given"].as_str().map_or(f.into(), |g| format!("{g} {f}")))
    });
    let venue = w["container-title"][0].as_str().map(Into::into);
    let year = w["issued"]["date-parts"][0][0].as_u64().map(|y| y.to_string());
    let line = byline(au.collect(), venue, year);
    Some((title, line + &abs(w["abstract"].as_str().map(Into::into))))
}

/// A DOI a link names: doi.org's, one in its path, or Nature's article id.
fn doi(host: &str, path: &str) -> Option<String> {
    if let Some(id) =
        path.strip_prefix("/articles/").filter(|_| host == "nature.com")
    {
        return Some(format!("10.1038/{}", id.trim_end_matches('/')));
    }
    let at = path.find("10.")?;
    (host == "doi.org" || path[..at].ends_with('/')).then_some(())?;
    let d = path[at..].trim_end_matches(".pdf").trim_end_matches('/');
    d.contains('/').then(|| d.to_string())
}

/// "A, B, C et al. · Venue · 2021".
fn byline(
    au: Vec<String>,
    venue: Option<String>,
    year: Option<String>,
) -> String {
    let mut who = au.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if au.len() > 3 {
        who += " et al.";
    }
    let parts = [Some(who), venue, year].into_iter().flatten();
    parts.filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// A date's first four characters: never a slice inside one.
fn year(d: &str) -> String {
    d.chars().take(4).collect()
}

/// " — abstract", its markup (JATS) stripped, or nothing.
fn abs(a: Option<String>) -> String {
    a.map(|a| format!(" — {}", squash(&strip(&a)))).unwrap_or_default()
}

/// The text of the first `<t>…</t>`.
fn tag(x: &str, t: &str) -> Option<String> {
    let open = x.find(&format!("<{t}"))?;
    let start = open + x[open..].find('>')? + 1;
    let end = start + x[start..].find(&format!("</{t}>"))?;
    Some(unescape(&x[start..end]))
}

fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// At most `max` bytes of a `kind` body, and its type.
async fn get(
    web: &reqwest::Client,
    url: &str,
    max: usize,
    kind: &str,
) -> Result<(Vec<u8>, String)> {
    let mut r = web.get(url).send().await?.error_for_status()?;
    let ct = r.headers().get(reqwest::header::CONTENT_TYPE);
    let ct = ct.and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ct = ct.split(';').next().unwrap_or_default().trim().to_lowercase();
    if !ct.starts_with(kind) {
        bail!("not {kind}: {ct}");
    }
    let mut b = vec![];
    let html = kind == "text/html";
    while let Some(c) = r.chunk().await? {
        b.extend_from_slice(&c);
        let tail = &b[b.len().saturating_sub(c.len() + 6)..];
        if html && tail.windows(7).any(|w| w.eq_ignore_ascii_case(b"</head>")) {
            break;
        }
        if b.len() > max {
            // A page's head is enough; an image cut short is none.
            ensure!(html, "too big");
            b.truncate(max);
            break;
        }
    }
    Ok((b, ct))
}

/// `<meta property|name=K content=V>`, keys lowercased.
fn metas(html: &str) -> HashMap<String, String> {
    let low = html.to_ascii_lowercase();
    let mut out = HashMap::new();
    // A few hundred tags of a few KB: a page of unclosed ones is read
    // once, not once per tag.
    for (at, _) in low.match_indices("<meta").take(512) {
        let near = low.floor_char_boundary((at + 4096).min(low.len()));
        let end = low[at..near].find('>').map_or(near, |e| at + e);
        let a = attrs(&html[at + 5..end]);
        let key = a.get("property").or_else(|| a.get("name"));
        if let (Some(k), Some(v)) = (key, a.get("content")) {
            out.entry(k.to_lowercase()).or_insert_with(|| v.clone());
        }
    }
    out
}

/// A tag's `k="v"`, `k='v'` and `k=v`, keys lowercased.
fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].split_whitespace().last().unwrap_or_default();
        let v = rest[eq + 1..].trim_start();
        let (val, next) = match v.chars().next() {
            Some(q @ ('"' | '\'')) => match v[1..].find(q) {
                Some(e) => (&v[1..e + 1], &v[e + 2..]),
                None => (&v[1..], ""),
            },
            _ => {
                let e = v.find(char::is_whitespace).unwrap_or(v.len());
                (&v[..e], &v[e..])
            }
        };
        out.insert(key.to_lowercase(), val.to_string());
        rest = next;
    }
    out
}

fn title(html: &str) -> Option<String> {
    let low = html.to_ascii_lowercase();
    let open = low.find("<title")?;
    let start = open + low[open..].find('>')? + 1;
    let end = start + low[start..].find("</title")?;
    Some(html[start..end].to_string())
}

fn unescape(s: &str) -> String {
    let pairs = [
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&apos;", "'"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ];
    pairs.iter().fold(s.to_string(), |s, (a, b)| s.replace(a, b))
}

#[test]
fn reads_what_a_page_says_of_itself() {
    let html = r#"<html><head><TITLE>Plain</TITLE>
        <meta property="og:title" content="Tom &amp; Jerry">
        <META name=description content='A "cat" and a mouse'>
        <meta property="og:image" content="/i.png"></head>"#;
    let m = metas(html);
    assert_eq!(unescape(&m["og:title"]), "Tom & Jerry");
    assert_eq!(m["description"], r#"A "cat" and a mouse"#);
    assert_eq!(m["og:image"], "/i.png");
    assert_eq!(title(html).as_deref(), Some("Plain"));
    assert_eq!(
        first("see (https://ex.com/a?b=1)."),
        Some("https://ex.com/a?b=1")
    );
    assert_eq!(first("http://ex.com https:// x"), None);
}

#[test]
fn papers_are_known_by_their_links() {
    let d = |h, p| doi(h, p);
    assert_eq!(d("doi.org", "/10.1038/x-1").as_deref(), Some("10.1038/x-1"));
    let sci = d("science.org", "/doi/10.1126/science.abj8754");
    assert_eq!(sci.as_deref(), Some("10.1126/science.abj8754"));
    let nat = d("nature.com", "/articles/s41586-021-03819-2");
    assert_eq!(nat.as_deref(), Some("10.1038/s41586-021-03819-2"));
    assert_eq!(d("ex.com", "/v10.2/a"), None, "not a DOI");
    assert_eq!(d("ex.com", "/10.5555"), None, "no suffix");
    let au = |n: usize| (0..n).map(|i| format!("A{i}")).collect();
    assert_eq!(byline(au(2), Some("J".into()), None), "A0, A1 · J");
    assert_eq!(
        byline(au(4), None, Some("2021".into())),
        "A0, A1, A2 et al. · 2021"
    );
    assert_eq!(abs(Some("<jats:p>Hi  there</jats:p>".into())), " — Hi there");
    assert_eq!(tag("<x a='1'>a &amp; b</x>", "x").as_deref(), Some("a & b"));
    assert_eq!(percent_decode("/doi/10.1002%2Fx"), "/doi/10.1002/x");
    assert_eq!(year("20é1-01"), "20é1");
}
