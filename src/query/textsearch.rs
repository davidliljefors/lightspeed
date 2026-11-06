//! Plain-text search over indexed files (`lightspeed/findText`): literal or regex,
//! case and whole-word options, run in parallel over the in-memory text.

use std::sync::atomic::{AtomicUsize, Ordering};

use memchr::memmem;
use rayon::prelude::*;

use super::Text;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// One match: file number (index into the searched paths) and byte range in its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub file: usize,
    pub start: u32,
    pub end: u32,
}

pub enum Matcher {
    Literal(memmem::Finder<'static>),
    Re(regex::Regex),
}

/// Matches kept per file, so one generated file cannot flood the result.
const PER_FILE: usize = 500;

impl Matcher {
    pub fn new(query: &str, o: Options) -> Result<Self, String> {
        if !o.regex && o.case_sensitive && !o.whole_word {
            return Ok(Self::Literal(memmem::Finder::new(query.as_bytes()).into_owned()));
        }
        let mut pat = if o.regex { query.to_owned() } else { regex::escape(query) };
        if o.whole_word {
            pat = format!(r"\b(?:{pat})\b");
        }
        regex::RegexBuilder::new(&pat).case_insensitive(!o.case_sensitive).multi_line(true).build().map(Self::Re).map_err(|e| e.to_string())
    }

    /// Byte ranges of the (non-empty) matches in `text`, at most `limit`.
    fn find_all(&self, text: &str, limit: usize) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        match self {
            Self::Literal(f) => {
                let n = f.needle().len() as u32;
                for p in f.find_iter(text.as_bytes()).take(limit) {
                    out.push((p as u32, p as u32 + n));
                }
            }
            Self::Re(re) => {
                for m in re.find_iter(text).filter(|m| !m.is_empty()).take(limit) {
                    out.push((m.start() as u32, m.end() as u32));
                }
            }
        }
        out
    }
}

pub struct Found {
    /// In path order, then position; at most the requested number.
    pub hits: Vec<Hit>,
    /// More matches exist than were returned.
    pub truncated: bool,
    pub files_searched: usize,
}

/// Searches `paths` (kept in the order given, so pass them sorted for stable results).
/// Files are taken in path order in chunks, and scanning stops once `max` matches are in hand.
pub fn search(paths: &[&str], text_of: &(dyn Fn(&str) -> Option<Text> + Sync), m: &Matcher, max: usize) -> Found {
    const CHUNK: usize = 1024;
    let searched = AtomicUsize::new(0);
    let mut hits: Vec<Hit> = Vec::new();
    let mut truncated = false;
    for (c, chunk) in paths.chunks(CHUNK).enumerate() {
        let per_file: Vec<Vec<(u32, u32)>> = chunk
            .par_iter()
            .map(|p| {
                let Some(t) = text_of(p) else { return Vec::new() };
                searched.fetch_add(1, Ordering::Relaxed);
                m.find_all(t.as_str(), PER_FILE + 1)
            })
            .collect();
        for (i, r) in per_file.into_iter().enumerate() {
            truncated |= r.len() > PER_FILE;
            for (start, end) in r.into_iter().take(PER_FILE) {
                if hits.len() == max {
                    truncated = true;
                } else {
                    hits.push(Hit { file: c * CHUNK + i, start, end });
                }
            }
        }
        if hits.len() >= max {
            // Whether later files match is unknown; the caller only needs to know there may be more.
            truncated |= (c + 1) * CHUNK < paths.len();
            break;
        }
    }
    Found { hits, truncated, files_searched: searched.load(Ordering::Relaxed) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn run(q: &str, o: Options, max: usize) -> (Vec<(usize, String)>, bool) {
        let files = ["a.h", "b.cpp", "c.cpp"];
        let src = |p: &str| -> Option<Text> {
            let s = match p {
                "a.h" => "struct Foo {};\nint foo_bar;\n",
                "b.cpp" => "Foo f; // FOO\nvoid g() { Foo(); }\n",
                _ => "nothing here\n",
            };
            Some(Text::Shared(Arc::from(s)))
        };
        let m = Matcher::new(q, o).unwrap();
        let f = search(&files, &src, &m, max);
        let texts = ["struct Foo {};\nint foo_bar;\n", "Foo f; // FOO\nvoid g() { Foo(); }\n"];
        (f.hits.iter().map(|h| (h.file, texts[h.file][h.start as usize..h.end as usize].to_owned())).collect(), f.truncated)
    }

    const LIT: Options = Options { regex: false, case_sensitive: true, whole_word: false };

    #[test]
    fn literal_is_case_sensitive_and_ordered() {
        let (h, t) = run("Foo", LIT, 100);
        assert_eq!(h.iter().map(|x| x.0).collect::<Vec<_>>(), [0, 1, 1]);
        assert!(!t);
    }

    #[test]
    fn ignore_case_and_whole_word() {
        let (h, _) = run("foo", Options { case_sensitive: false, ..LIT }, 100);
        assert_eq!(h.len(), 5, "{h:?}");
        let (h, _) = run("foo", Options { case_sensitive: false, whole_word: true, ..LIT }, 100);
        assert_eq!(h.len(), 4, "foo_bar is one word: {h:?}");
    }

    #[test]
    fn regex_and_cap() {
        let (h, _) = run(r"Foo\(\)", Options { regex: true, ..LIT }, 100);
        assert_eq!(h, [(1, "Foo()".to_owned())]);
        let (h, t) = run("Foo", LIT, 2);
        assert_eq!(h.len(), 2);
        assert!(t);
    }

    #[test]
    fn bad_regex_is_an_error_and_empty_matches_are_skipped() {
        assert!(Matcher::new("(", Options { regex: true, ..LIT }).is_err());
        let (h, _) = run("x*", Options { regex: true, ..LIT }, 100);
        assert!(h.is_empty());
    }
}
