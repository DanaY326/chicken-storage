//! Bridge to the Chicken VM.
//!
//! Some of the store's logic is written in Chicken (see `chicken/*.chasm`)
//! and assembled into `chicken/*.ch` with `tools/chasm.py`. The assembled
//! programs are compiled into the binary, so the server never reads them from
//! disk at runtime.

use chicken_esolang::Chicken;
use std::panic::{self, AssertUnwindSafe};

/// `KEYS` pattern matching. Input `pattern\x01key\x01`, output `1` or `0`.
const GLOB: &str = include_str!("../chicken/glob.ch");
/// Signed integer addition for INCR/DECR/INCRBY/DECRBY. Input `a\x01b\x01`.
const ADD: &str = include_str!("../chicken/add.ch");

/// Marks the end of each part of a program's input.
const SEP: char = '\x01';

/// Runs a Chicken program and returns whatever it leaves on top of the stack.
///
/// The interpreter reports errors by panicking, so the run is wrapped in
/// `catch_unwind` and a panic becomes an `Err`.
pub fn run(code: &str, input: &str, backwards_compatible: bool) -> Result<String, String> {
    let code = code.to_string();
    let input = input.to_string();
    panic::catch_unwind(AssertUnwindSafe(move || {
        Chicken::new(code, &input, false, backwards_compatible).run()
    }))
    .map_err(|e| {
        let msg = e
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| e.downcast_ref::<&str>().copied())
            .unwrap_or("unknown error");
        format!("chicken VM error: {msg}")
    })
}

/// Does `key` match the glob `pattern` (`*` and `?` wildcards)?
///
/// Keys or patterns containing the `\x01` separator can't be passed to the
/// Chicken program, so they never match.
pub fn glob_match(pattern: &str, key: &str) -> Result<bool, String> {
    if pattern.contains(SEP) || key.contains(SEP) {
        return Ok(false);
    }
    match run(GLOB, &format!("{pattern}{SEP}{key}{SEP}"), false)?.as_str() {
        "1" => Ok(true),
        "0" => Ok(false),
        other => Err(format!("chicken glob returned {other:?}")),
    }
}

/// Computes `a + b` in Chicken. The caller must make sure the sum fits in an
/// i64 (the Chicken VM would panic or wrap around otherwise).
pub fn add(a: i64, b: i64) -> Result<i64, String> {
    let out = run(ADD, &format!("{a}{SEP}{b}{SEP}"), false)?;
    out.parse().map_err(|_| format!("chicken add returned {out:?}"))
}

/// The original interpreter is JS-compatible and prints characters as HTML
/// entities like `&#72;`. This turns them back into characters.
pub fn decode_entities(s: &str) -> String {
    let mut result = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("&#") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        let decoded = after[digits..]
            .starts_with(';')
            .then(|| after[..digits].parse::<u32>().ok().and_then(char::from_u32))
            .flatten();
        match decoded {
            Some(ch) => {
                result.push(ch);
                rest = &after[digits + 1..];
            }
            None => {
                result.push_str("&#");
                rest = after;
            }
        }
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation to check the Chicken one against.
    fn glob_reference(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, _) => s.is_empty(),
            (Some('*'), _) => glob_reference(&p[1..], s) || (!s.is_empty() && glob_reference(p, &s[1..])),
            (Some(_), None) => false,
            (Some('?'), Some(_)) => glob_reference(&p[1..], &s[1..]),
            (Some(a), Some(b)) => a == b && glob_reference(&p[1..], &s[1..]),
        }
    }

    #[test]
    fn glob_basics() {
        let cases = [
            ("*", "anything", true),
            ("*", "", true),
            ("", "", true),
            ("", "a", false),
            ("user:*", "user:42", true),
            ("user:*", "session:42", false),
            ("h?llo", "hello", true),
            ("h?llo", "hllo", false),
            ("h*llo", "heeeello", true),
            ("*:*:end", "a:b:c:end", true),
            ("a*b*c", "abxbxc", true),
            ("a*b*c", "abxbx", false),
            ("**", "x", true),
            ("ünï*", "ünïcode", true),
            ("with space*", "with space too", true),
        ];
        for (pattern, key, expected) in cases {
            assert_eq!(glob_match(pattern, key), Ok(expected), "pattern {pattern:?} key {key:?}");
        }
    }

    #[test]
    fn glob_matches_reference_exhaustively() {
        // Every pattern over {a, b, ?, *} up to length 4 against every key
        // over {a, b} up to length 4.
        fn words(alphabet: &[char], max: usize) -> Vec<String> {
            let mut all = vec![String::new()];
            let mut frontier = vec![String::new()];
            for _ in 0..max {
                frontier = frontier
                    .iter()
                    .flat_map(|w| alphabet.iter().map(move |c| format!("{w}{c}")))
                    .collect();
                all.extend(frontier.iter().cloned());
            }
            all
        }
        let patterns = words(&['a', 'b', '?', '*'], 4);
        let keys = words(&['a', 'b'], 4);
        for p in &patterns {
            for k in &keys {
                let pc: Vec<char> = p.chars().collect();
                let kc: Vec<char> = k.chars().collect();
                assert_eq!(glob_match(p, k), Ok(glob_reference(&pc, &kc)), "pattern {p:?} key {k:?}");
            }
        }
    }

    #[test]
    fn separator_never_matches() {
        assert_eq!(glob_match("*", "a\x01b"), Ok(false));
    }

    #[test]
    fn add_works() {
        let cases = [
            (0, 0),
            (1, 1),
            (41, 1),
            (-1, 1),
            (5, -10),
            (-7, -8),
            (1234567890, 9876543210),
            (i64::MAX - 1, 1),
            (i64::MIN + 1, -1),
            (i64::MIN, 0),
            (i64::MAX, i64::MIN),
        ];
        for (a, b) in cases {
            assert_eq!(add(a, b), Ok(a + b), "{a} + {b}");
        }
    }

    #[test]
    fn add_rejects_non_numbers() {
        assert!(run(ADD, "12x\x011\x01", false).unwrap() == "chicken");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(decode_entities("&#72;i &#bad; &#33;"), "Hi &#bad; !");
    }
}
