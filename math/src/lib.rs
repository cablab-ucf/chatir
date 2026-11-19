//! Typst math as MathML: no code, files or packages, so it only typesets.

use std::sync::LazyLock;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Feature, Library, LibraryExt, World};
use typst_html::HtmlDocument;

const MAX: usize = 2048;

static LIB: LazyLock<LazyHash<Library>> = LazyLock::new(|| {
    let html = [Feature::Html].into_iter().collect();
    LazyHash::new(Library::builder().with_features(html).build())
});
static BOOK: LazyLock<LazyHash<FontBook>> =
    LazyLock::new(|| LazyHash::new(FontBook::new()));

struct Math(Source);

impl World for Math {
    fn library(&self) -> &LazyHash<Library> {
        &LIB
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &BOOK
    }
    fn main(&self) -> FileId {
        self.0.id()
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        (id == self.0.id())
            .then(|| self.0.clone())
            .ok_or(FileError::AccessDenied)
    }
    fn file(&self, _: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }
    fn font(&self, _: usize) -> Option<Font> {
        None
    }
    fn today(&self, _: Option<Duration>) -> Option<Datetime> {
        None
    }
}

pub fn mathml(src: &str, display: bool) -> Result<String, String> {
    // `#` and `$` leave math, `std` reaches all of Typst (endless loops,
    // memory-filling arrays), and a string's methods compute.
    let code = src.contains(['#', '$']) || src.contains("std");
    if src.len() > MAX || code || src.contains("\".") {
        return Err("not plain Typst math".into());
    }
    let pad = if display { " " } else { "" };
    let text = format!("${pad}{src}{pad}$");
    // Deep math outruns a pool thread's 2 MiB before Typst's own limits.
    let run = std::thread::Builder::new().stack_size(16 << 20);
    let html = run.spawn(move || html(text)).map_err(|e| e.to_string())?;
    let html = html.join().map_err(|_| "typst failed".to_string())??;
    let (a, b) = (html.find("<math"), html.rfind("</math>"));
    match (a, b) {
        (Some(a), Some(b)) => Ok(html[a..b + 7].into()),
        _ => Err("no math".into()),
    }
}

fn html(text: String) -> Result<String, String> {
    let path = VirtualPath::new("m.typ").map_err(|e| format!("{e:?}"))?;
    let id = FileId::new(RootedPath::new(VirtualRoot::Project, path));
    let doc = typst::compile::<HtmlDocument>(&Math(Source::new(id, text)));
    typst::comemo::evict(10);
    let err = |e: typst::ecow::EcoVec<typst::diag::SourceDiagnostic>| {
        e.first().map_or("typst failed".into(), |d| d.message.to_string())
    };
    typst_html::html(&doc.output.map_err(err)?, &Default::default())
        .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::{html, mathml};

    #[test]
    fn typesets_math_and_nothing_else() {
        let m = mathml("x^2", false).unwrap();
        assert!(m.starts_with("<math><msup>") && m.ends_with("</math>"));
        let d = mathml("sum_(k=1)^n k = (n(n+1))/2", true).unwrap();
        assert!(d.starts_with(r#"<math display="block">"#));
        assert!(d.contains("<mfrac>") && d.contains("<munderover>"));
        assert!(mathml(r#"#read("/etc/passwd")"#, false).is_err());
        assert!(mathml("x$ #panic()", false).is_err());
        assert!(mathml("frac(", false).is_err());
        assert!(
            mathml(r#"std.range(std.int("100000000000"))"#, false).is_err()
        );
        assert!(mathml(r#""a".repeat(99999999)"#, false).is_err());
        // Too deep: an error on any thread, never a stack overflow.
        let deep = format!("{}x{}", "frac(".repeat(200), ", 1)".repeat(200));
        let pool = std::thread::Builder::new().stack_size(2 << 20);
        let r = pool.spawn(move || mathml(&deep, false)).unwrap().join();
        assert!(r.unwrap().is_err());
    }

    #[test]
    fn the_vendored_stylesheet_is_typsts() {
        let h = html("$x$".into()).unwrap();
        let css =
            &h[h.find("<style>").unwrap() + 7..h.find("</style>").unwrap()];
        let ours = include_str!("../../app/ui/vendor/typst-math.css");
        assert_eq!(ours.trim_end(), css.trim_end());
    }
}
#[cfg(test)]
mod scope {
    use super::mathml;

    /// Math mode resolves only the math scope: the rest of Typst (files,
    /// computation, the clock) is unknown there, not just denied by name.
    #[test]
    fn only_the_math_scope_is_reachable() {
        for s in [
            "calc.abs(-5)",
            "upper(\"ab\")",
            "repr(1)",
            "lorem(3)",
            "array.range(3)",
            "range(3).len()",
            "datetime.today()",
            "sys.version",
            "read(\"/etc/passwd\")",
            "box(width: 1pt)",
            "link(\"https://evil.example\")[x]",
            "place(dx: 1pt)[x]",
        ] {
            assert!(mathml(s, false).is_err(), "{s} reached");
        }
        assert!(mathml("frac(a, b) + sum_(i=1)^n x_i", true).is_ok());
        // `image` there is the symbol ⊷, not the function: nothing loads.
        let img = mathml("image(\"x.png\")", false).unwrap();
        assert!(img.contains('⊷') && !img.contains("<img"));
    }

    /// Whatever a message holds, rendering ends, panics nothing, and keeps
    /// to its bounds: random and nested math, near the length limit.
    #[test]
    fn any_input_renders_or_is_refused() {
        let atoms = [
            "a", "x_i", "^2", "frac(", "sqrt(", "(", ")", "[", "]", "{", "}",
            ",", ";", "mat(", "vec(", "&", "\\\\", "\"", "'", "sum_", "lr(",
            "|", "op(", "attach(", "cases(", " ", "=", "/", "abs(", "∫", "→",
            "😀", "\u{0}", "..", ":",
        ];
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3000 {
            let n = (next() % 400) as usize;
            let s: String = (0..n)
                .map(|_| atoms[(next() % atoms.len() as u64) as usize])
                .collect();
            let _ = mathml(&s, next() % 2 == 0);
        }
        // Deep nesting, at the limit: refused or rendered, never a crash.
        let deep = "frac(".repeat(400) + &")".repeat(400);
        let _ = mathml(&deep, true);
        assert!(mathml(&"x".repeat(2049), false).is_err(), "over the limit");
    }
}
