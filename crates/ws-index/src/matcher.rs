//! Byte-level matching primitives shared by the query engine. Kept in the index
//! crate so both the engine and the query compiler can use them without a cycle.

/// Lowercase an ASCII-heavy string into a byte vector for case-insensitive
/// matching. Non-ASCII bytes pass through unchanged (matching then falls back to
/// exact bytes, which is correct for the common UTF-8 file-name case).
pub fn to_needle(s: &str) -> Vec<u8> {
    s.bytes().map(|b| b.to_ascii_lowercase()).collect()
}

/// Case-insensitive substring test: does `hay` contain `needle`?
/// `needle` must already be ASCII-lowercased (see [`to_needle`]).
#[inline]
pub fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    let nlen = needle.len();
    if nlen == 0 {
        return true;
    }
    if hay.len() < nlen {
        return false;
    }
    let first_lo = needle[0];
    let first_up = first_lo.to_ascii_uppercase();
    let last = hay.len() - nlen;
    let mut i = 0;
    while i <= last {
        // Jump to the next position whose byte could start a match.
        let rel = if first_lo == first_up {
            memchr::memchr(first_lo, &hay[i..=last])
        } else {
            memchr::memchr2(first_lo, first_up, &hay[i..=last])
        };
        let Some(rel) = rel else { return false };
        let pos = i + rel;
        if eq_ci(&hay[pos + 1..pos + nlen], &needle[1..]) {
            return true;
        }
        i = pos + 1;
    }
    false
}

#[inline]
fn eq_ci(a: &[u8], b_lower: &[u8]) -> bool {
    // b_lower is already lowercased; lowercase a on the fly.
    for (x, y) in a.iter().zip(b_lower) {
        if x.to_ascii_lowercase() != *y {
            return false;
        }
    }
    true
}

/// Case-insensitive `ends_with`, for extension matching.
#[inline]
pub fn ends_with_ci(hay: &[u8], suffix_lower: &[u8]) -> bool {
    if hay.len() < suffix_lower.len() {
        return false;
    }
    eq_ci(&hay[hay.len() - suffix_lower.len()..], suffix_lower)
}

/// Simple `*`/`?` glob match over bytes, case-insensitive. `pat` should be
/// lowercased. Used for `*.ext` and wildcard name patterns.
pub fn glob_ci(hay: &[u8], pat: &[u8]) -> bool {
    // Iterative glob with backtracking; O(n*m) worst case, fine for file names.
    let (mut h, mut p) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while h < hay.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p].eq_ignore_ascii_case(&hay[h])) {
            h += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = h;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            h = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}
