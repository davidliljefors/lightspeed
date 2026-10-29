//! Answering from an index that lags behind an open buffer.
//!
//! The index describes each open file as of the last time it was indexed (`Doc::indexed`), while
//! the editor already holds newer text. Instead of re-indexing before every request, a request
//! is answered in the *indexed* world: the cursor is mapped from the buffer to the indexed text,
//! queries run on the indexed text, and the answer is mapped back to the buffer. Anything that
//! touches the edit is dropped rather than guessed, so the answer can be stale but never points
//! at the wrong place.
//!
//! The two texts are related by their longest common prefix and suffix, which is exact for the
//! usual case of one contiguous edit and conservative for several (everything between the first
//! and last change counts as changed).

/// What changed between two versions of a text: everything outside the common prefix and suffix.
pub struct EditMap {
    pre: usize,
    suf: usize,
    new_len: usize,
}

fn ident(s: &str, i: usize) -> bool {
    s.as_bytes().get(i).is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
}

/// Maps `a..b` from `from` to `to`, whose common prefix/suffix are `pre`/`suf` bytes. A range
/// that overlaps the change, or that touches it where text would fuse into one identifier, has no image.
fn shift(pre: usize, suf: usize, from: &str, to: &str, a: usize, b: usize) -> Option<(usize, usize)> {
    if b <= pre {
        if b == pre && (ident(from, b) || ident(to, b)) {
            return None;
        }
        return Some((a, b));
    }
    let (from_end, to_end) = (from.len() - suf, to.len() - suf);
    if a < from_end {
        return None;
    }
    if a == from_end && ((a > 0 && ident(from, a - 1)) || (to_end > 0 && ident(to, to_end - 1))) {
        return None;
    }
    let start = to_end + (a - from_end);
    Some((start, start + (b - a)))
}

impl EditMap {
    pub fn between(old: &str, new: &str) -> Self {
        let both = |i: usize, j: usize| old.is_char_boundary(i) && new.is_char_boundary(j);
        let mut pre = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
        while pre > 0 && !both(pre, pre) {
            pre -= 1;
        }
        let room = old.len().min(new.len()) - pre;
        let mut suf = old.bytes().rev().zip(new.bytes().rev()).take(room).take_while(|(a, b)| a == b).count();
        while suf > 0 && !both(old.len() - suf, new.len() - suf) {
            suf -= 1;
        }
        Self { pre, suf, new_len: new.len() }
    }

    /// A range of the old text, as it reads in the new one.
    pub fn old_to_new(&self, old: &str, new: &str, a: u32, b: u32) -> Option<(u32, u32)> {
        shift(self.pre, self.suf, old, new, a as usize, b as usize).map(|(a, b)| (a as u32, b as u32))
    }

    /// A range of the new text, as it read in the old one.
    pub fn new_to_old(&self, old: &str, new: &str, a: u32, b: u32) -> Option<(u32, u32)> {
        shift(self.pre, self.suf, new, old, a as usize, b as usize).map(|(a, b)| (a as u32, b as u32))
    }

    /// Whether the change reaches the line of `off` (a byte offset in the new text). A statement
    /// edited on the cursor's own line can change what the cursor means even when the identifier under it did not.
    pub fn touches_line(&self, new: &str, off: u32) -> bool {
        let off = (off as usize).min(new.len());
        let start = new[..off].rfind('\n').map_or(0, |i| i + 1);
        let end = new[off..].find('\n').map_or(new.len(), |i| off + i);
        let changed_end = self.new_len - self.suf;
        self.pre <= end && changed_end >= start && changed_end != start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(old: &str, new: &str, needle: &str) -> (Option<(u32, u32)>, Option<(u32, u32)>) {
        let em = EditMap::between(old, new);
        let at = |s: &str| s.find(needle).map(|i| (i as u32, (i + needle.len()) as u32));
        (at(old).and_then(|(a, b)| em.old_to_new(old, new, a, b)), at(new).and_then(|(a, b)| em.new_to_old(old, new, a, b)))
    }

    #[test]
    fn text_after_an_insertion_shifts() {
        let (o, n) = ("int a;\nint target;\n", "int a;\nint added;\nint target;\n");
        let (fwd, back) = map(o, n, "target");
        assert_eq!(fwd.map(|(a, _)| &n[a as usize..]), Some("target;\n"));
        assert_eq!(back.map(|(a, _)| &o[a as usize..]), Some("target;\n"));
    }

    #[test]
    fn text_before_the_change_stays_and_the_change_itself_has_no_image() {
        let (o, n) = ("int keep;\nint old;\n", "int keep;\nint changed;\n");
        assert_eq!(map(o, n, "keep").0, Some((4, 8)));
        assert_eq!(map(o, n, "old").0, None);
        assert_eq!(map(o, n, "changed").1, None);
    }

    #[test]
    fn a_range_straddling_the_change_has_no_image() {
        assert_eq!(map("foo bar baz", "foo bXr baz", "bar").0, None);
        assert_eq!(map("foo bar baz", "foo bXr baz", "foo").0, Some((0, 3)));
        assert_eq!(map("foo bar baz", "foo bXr baz", "baz").0, Some((8, 11)));
    }

    #[test]
    fn identifiers_that_fuse_with_the_edit_are_dropped() {
        assert_eq!(map("int a;", "int ab;", "a").0, None, "typing after an identifier changes it");
        assert_eq!(map("f(x);", "yf(x);", "f").0, None, "typing before an identifier changes it");
        assert_eq!(map("int a;\nint b;", "int a;\n\nint b;", "int b").0, Some((8, 13)), "a new blank line does not");
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        let (o, n) = ("// é\nint x;\n", "// è\nint x;\n");
        let em = EditMap::between(o, n);
        assert!(o.is_char_boundary(em.pre) && n.is_char_boundary(em.pre));
        assert_eq!(map(o, n, "x").0, Some((10, 11)));
    }

    #[test]
    fn line_touching() {
        let (o, n) = ("a;\nb;\nc;\n", "a;\nb2;\nc;\n");
        let em = EditMap::between(o, n);
        assert!(em.touches_line(n, 4), "the edited line");
        assert!(!em.touches_line(n, 0), "an earlier line");
        assert!(!em.touches_line(n, 8), "a later line");
        let added = "a;\nnew;\nb;\nc;\n";
        let em = EditMap::between("a;\nb;\nc;\n", added);
        assert!(!em.touches_line(added, 8), "the line that moved down is not edited");
    }
}
