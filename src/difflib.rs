//! A port of the parts of CPython's `difflib` the file tools use:
//! `SequenceMatcher` (with `isjunk=None`, `autojunk=True`) and `unified_diff`.
//!
//! Python's file tools show their changes as unified diffs, both in the approval
//! prompt and in what the model reads back. A different diff algorithm (Myers,
//! patience) can group the same change into different hunks, so this follows
//! CPython's Ratcliff/Obershelp matcher step for step, including its "popular
//! element" heuristic.

use std::collections::HashMap;

/// One edit operation: `(tag, i1, i2, j1, j2)` as in `SequenceMatcher.get_opcodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opcode {
    pub tag: Tag,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}

/// CPython's `SequenceMatcher(None, a, b)` over lines.
pub struct SequenceMatcher<'a> {
    a: &'a [&'a str],
    b: &'a [&'a str],
    /// Indices of each element of `b`, minus the "popular" ones.
    b2j: HashMap<&'a str, Vec<usize>>,
}

impl<'a> SequenceMatcher<'a> {
    pub fn new(a: &'a [&'a str], b: &'a [&'a str]) -> Self {
        let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, element) in b.iter().enumerate() {
            b2j.entry(element).or_default().push(index);
        }
        // autojunk: in sequences of 200+ elements, anything occurring more than
        // 1% of the time (plus one) is too common to anchor a match on.
        if b.len() >= 200 {
            let ntest = b.len() / 100 + 1;
            b2j.retain(|_, indices| indices.len() <= ntest);
        }
        Self { a, b, b2j }
    }

    /// The longest matching block in `a[alo..ahi]` and `b[blo..bhi]`: `(i, j, size)`.
    fn find_longest_match(
        &self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let (a, b) = (self.a, self.b);
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, element) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut new_j2len = HashMap::new();
            if let Some(indices) = self.b2j.get(element) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|prev| j2len.get(&prev))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    new_j2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = new_j2len;
        }

        // Extend the match with equal elements on both sides. With no junk
        // function, CPython's junk-only extension loops never apply.
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    /// Non-overlapping matching blocks, ending with the sentinel `(len(a), len(b), 0)`.
    pub fn matching_blocks(&self) -> Vec<(usize, usize, usize)> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0, la, 0, lb)];
        let mut blocks = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort_unstable();

        // Collapse adjacent blocks.
        let mut collapsed = Vec::new();
        let (mut i1, mut j1, mut k1) = (0, 0, 0);
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    collapsed.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            collapsed.push((i1, j1, k1));
        }
        collapsed.push((la, lb, 0));
        collapsed
    }

    pub fn opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0, 0);
        let mut codes = Vec::new();
        for (ai, bj, size) in self.matching_blocks() {
            let tag = match (i < ai, j < bj) {
                (true, true) => Some(Tag::Replace),
                (true, false) => Some(Tag::Delete),
                (false, true) => Some(Tag::Insert),
                (false, false) => None,
            };
            if let Some(tag) = tag {
                codes.push(Opcode {
                    tag,
                    i1: i,
                    i2: ai,
                    j1: j,
                    j2: bj,
                });
            }
            i = ai + size;
            j = bj + size;
            if size > 0 {
                codes.push(Opcode {
                    tag: Tag::Equal,
                    i1: ai,
                    i2: i,
                    j1: bj,
                    j2: j,
                });
            }
        }
        codes
    }

    /// Hunks of changes with up to `n` lines of context, as `get_grouped_opcodes(n)`.
    pub fn grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.opcodes();
        if codes.is_empty() {
            codes.push(Opcode {
                tag: Tag::Equal,
                i1: 0,
                i2: 1,
                j1: 0,
                j2: 1,
            });
        }
        // Trim leading and trailing context to at most `n` lines.
        if let Some(first) = codes.first_mut().filter(|code| code.tag == Tag::Equal) {
            first.i1 = first.i1.max(first.i2.saturating_sub(n));
            first.j1 = first.j1.max(first.j2.saturating_sub(n));
        }
        if let Some(last) = codes.last_mut().filter(|code| code.tag == Tag::Equal) {
            last.i2 = last.i2.min(last.i1 + n);
            last.j2 = last.j2.min(last.j1 + n);
        }

        let mut groups = Vec::new();
        let mut group = Vec::new();
        for code in codes {
            let Opcode {
                tag,
                mut i1,
                i2,
                mut j1,
                j2,
            } = code;
            // A long unchanged range ends one hunk and starts the next.
            if tag == Tag::Equal && i2 - i1 > n * 2 {
                group.push(Opcode {
                    tag,
                    i1,
                    i2: i2.min(i1 + n),
                    j1,
                    j2: j2.min(j1 + n),
                });
                groups.push(std::mem::take(&mut group));
                i1 = i1.max(i2.saturating_sub(n));
                j1 = j1.max(j2.saturating_sub(n));
            }
            group.push(Opcode {
                tag,
                i1,
                i2,
                j1,
                j2,
            });
        }
        if !group.is_empty() && !(group.len() == 1 && group[0].tag == Tag::Equal) {
            groups.push(group);
        }
        groups
    }
}

/// `"\n".join(difflib.unified_diff(a, b, fromfile, tofile, lineterm="", n=n))`.
pub fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str, n: usize) -> String {
    let matcher = SequenceMatcher::new(a, b);
    let mut lines: Vec<String> = Vec::new();
    for group in matcher.grouped_opcodes(n) {
        if lines.is_empty() {
            lines.push(format!("--- {fromfile}"));
            lines.push(format!("+++ {tofile}"));
        }
        let (first, last) = (group[0], group[group.len() - 1]);
        lines.push(format!(
            "@@ -{} +{} @@",
            format_range(first.i1, last.i2),
            format_range(first.j1, last.j2)
        ));
        for code in &group {
            if code.tag == Tag::Equal {
                lines.extend(a[code.i1..code.i2].iter().map(|line| format!(" {line}")));
                continue;
            }
            if matches!(code.tag, Tag::Replace | Tag::Delete) {
                lines.extend(a[code.i1..code.i2].iter().map(|line| format!("-{line}")));
            }
            if matches!(code.tag, Tag::Replace | Tag::Insert) {
                lines.extend(b[code.j1..code.j2].iter().map(|line| format!("+{line}")));
            }
        }
    }
    lines.join("\n")
}

/// A unified-diff range: `start,length`, or just `start` for one line.
fn format_range(start: usize, stop: usize) -> String {
    let length = stop - start;
    match length {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        _ => format!("{},{length}", start + 1),
    }
}

/// Python's `str.splitlines()`: split on every line boundary Python recognizes,
/// without a trailing empty element.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.char_indices().find(|(_, c)| is_line_break(*c)) {
            None => {
                lines.push(rest);
                break;
            }
            Some((index, c)) => {
                lines.push(&rest[..index]);
                let mut next = index + c.len_utf8();
                if c == '\r' && rest[next..].starts_with('\n') {
                    next += 1;
                }
                rest = &rest[next..];
            }
        }
    }
    lines
}

/// A character Python's `str.splitlines()` treats as a line boundary.
pub fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}
