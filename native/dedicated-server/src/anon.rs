// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! Text that may carry identities (an OS error, a panic message) made safe for a log line
//! (contract §2.3, as `M/IntegratedHostRelay.cs:133-140` `Anonymous`): IPv4 addresses with an
//! optional port and IPv6-looking hex:colon runs (bracketed, with an optional port) become
//! `#ip`, digit runs of 6 or more (a XUID) become `#`, line breaks become spaces, at most 160
//! characters. Hand-written (no regex dependency); it masks at least what the C# patterns mask
//! (a leading word boundary is required, a trailing one is not).

/// The anonymised, single-line, bounded form of `text`.
pub fn anonymous(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len().min(256));
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let boundary = i == 0 || !is_word(chars[i - 1]);
        if ((boundary && (c.is_ascii_hexdigit() || c == ':')) || c == '[')
            && let Some(n) = address_at(&chars[i..])
        {
            out.push_str("#ip");
            i += n;
            continue;
        }
        if c.is_ascii_digit() {
            let n = chars[i..].iter().take_while(|d| d.is_ascii_digit()).count();
            if n >= 6 {
                out.push('#');
            } else {
                out.extend(&chars[i..i + n]);
            }
            i += n;
            continue;
        }
        out.push(if c == '\r' || c == '\n' || c.is_control() {
            ' '
        } else {
            c
        });
        i += 1;
    }
    out.chars().take(160).collect()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The length of an IPv4 (`a.b.c.d[:port]`) or IPv6-looking (`[h:h:...][:port]`) token at the
/// start of `s`, if one is there.
fn address_at(s: &[char]) -> Option<usize> {
    ipv4_at(s).or_else(|| ipv6_at(s))
}

fn digits(s: &[char], at: usize, max: usize) -> usize {
    s[at.min(s.len())..]
        .iter()
        .take(max)
        .take_while(|c| c.is_ascii_digit())
        .count()
}

/// `\b[0-9]{1,3}(\.[0-9]{1,3}){3}(:[0-9]{1,5})?\b`
fn ipv4_at(s: &[char]) -> Option<usize> {
    let mut at = 0;
    for k in 0..4 {
        if k > 0 {
            if s.get(at) != Some(&'.') {
                return None;
            }
            at += 1;
        }
        let n = digits(s, at, 3);
        if n == 0 {
            return None;
        }
        at += n;
    }
    if s.get(at) == Some(&':') {
        let n = digits(s, at + 1, 5);
        if n > 0 {
            at += 1 + n;
        }
    }
    // no trailing boundary check: masking `1.2.3.4x` too is stricter than the C#
    Some(at)
}

/// Hex:colon runs with at least two colons, optionally bracketed and with a port.
fn ipv6_at(s: &[char]) -> Option<usize> {
    let n = s
        .iter()
        .take_while(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '[' | ']'))
        .count();
    let mut token = &s[..n];
    while let Some(last) = token.last()
        && *last == '.'
    {
        token = &token[..token.len() - 1];
    }
    if token.iter().filter(|c| **c == ':').count() < 2 {
        return None;
    }
    Some(token.len())
}
