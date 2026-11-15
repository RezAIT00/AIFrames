//! Argument parsing and formatting helpers shared by the tools and the CLI.

use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};

pub type ToolResult = Result<J, String>;

/// Optional string argument (empty strings count as missing).
pub fn arg_str<'a>(a: &'a J, key: &str) -> Option<&'a str> {
    a.get(key).and_then(J::as_str).map(str::trim).filter(|s| !s.is_empty())
}

pub fn req_str<'a>(a: &'a J, key: &str) -> Result<&'a str, String> {
    arg_str(a, key).ok_or_else(|| format!("missing '{key}'"))
}

/// Optional number; numeric strings are accepted.
pub fn arg_f64(a: &J, key: &str) -> Result<Option<f64>, String> {
    match a.get(key) {
        None | Some(J::Null) => Ok(None),
        Some(v) => num_of(v).map(Some).ok_or_else(|| format!("'{key}' must be a number (got {v})")),
    }
}

pub fn req_f64(a: &J, key: &str) -> Result<f64, String> {
    arg_f64(a, key)?.ok_or_else(|| format!("missing '{key}' (a number)"))
}

pub fn num_of(v: &J) -> Option<f64> {
    match v {
        J::Number(n) => n.as_f64(),
        J::String(s) => s.trim().trim_end_matches('s').trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|x| x.is_finite())
}

/// Largest integer argument accepted anywhere (exactly representable as f64, fits every index type).
pub const MAX_INT_ARG: u64 = 1 << 40;

/// Whole number ≥ 0 (integer-valued floats like `3.0` are accepted; `3.5`, negatives and absurdly
/// large values are errors, never truncated).
pub fn arg_usize(a: &J, key: &str) -> Result<Option<usize>, String> {
    Ok(arg_int(a, key, 0, MAX_INT_ARG)?.map(|v| v as usize))
}

/// Whole number in `lo..=hi`, else an error naming the range.
pub fn arg_int(a: &J, key: &str, lo: u64, hi: u64) -> Result<Option<u64>, String> {
    let Some(v) = arg_f64(a, key)? else { return Ok(None) };
    match whole(v) {
        Some(n) if (lo..=hi).contains(&n) => Ok(Some(n)),
        _ => Err(format!("'{key}' must be a whole number from {lo} to {hi} (got {v})")),
    }
}

/// Whole number clamped into `lo..=hi` (for cosmetic sizes and limits). Non-integers are errors.
pub fn arg_int_clamped(a: &J, key: &str, lo: u64, hi: u64) -> Result<Option<u64>, String> {
    let Some(v) = arg_f64(a, key)? else { return Ok(None) };
    if !(v.is_finite() && v.fract() == 0.0) {
        return Err(format!("'{key}' must be a whole number (got {v})"));
    }
    Ok(Some(if v <= lo as f64 { lo } else if v >= hi as f64 { hi } else { v as u64 }))
}

/// A JSON value as a whole number ≥ 0 (integer-valued floats accepted).
pub fn int_of(v: &J) -> Option<u64> {
    match v {
        J::Number(n) => n.as_u64().or_else(|| n.as_f64().and_then(whole)),
        J::String(s) => s.trim().parse::<f64>().ok().and_then(whole),
        _ => None,
    }
}

fn whole(v: f64) -> Option<u64> {
    (v.is_finite() && v.fract() == 0.0 && v >= 0.0 && v <= MAX_INT_ARG as f64).then_some(v as u64)
}

pub fn arg_bool(a: &J, key: &str, default: bool) -> Result<bool, String> {
    match a.get(key) {
        None | Some(J::Null) => Ok(default),
        Some(J::Bool(b)) => Ok(*b),
        Some(J::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Ok(true),
            "false" | "no" | "0" | "off" => Ok(false),
            _ => Err(format!("'{key}' must be true or false (got {s:?})")),
        },
        Some(J::Number(n)) => Ok(n.as_f64().unwrap_or(0.0) != 0.0),
        Some(v) => Err(format!("'{key}' must be true or false (got {v})")),
    }
}

/// A list of strings: an array, a single string, or a comma-separated string.
pub fn arg_strs(a: &J, key: &str) -> Result<Option<Vec<String>>, String> {
    match a.get(key) {
        None | Some(J::Null) => Ok(None),
        Some(J::String(s)) => Ok(Some(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())),
        Some(J::Array(v)) => v
            .iter()
            .map(|x| match x {
                J::String(s) => Ok(s.trim().to_string()),
                J::Number(n) => Ok(n.to_string()),
                other => Err(format!("'{key}' must be a list of strings (got {other})")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(v) => Err(format!("'{key}' must be a string or a list of strings (got {v})")),
    }
}

/// `[a, b]` range of numbers (array, or "a,b" / "a-b" string).
pub fn arg_range(a: &J, key: &str) -> Result<Option<[f64; 2]>, String> {
    let v = match a.get(key) {
        None | Some(J::Null) => return Ok(None),
        Some(v) => v,
    };
    let nums: Vec<f64> = match v {
        J::Array(xs) => xs.iter().map(|x| num_of(x).ok_or_else(|| format!("'{key}' must be [start, end] in seconds"))).collect::<Result<_, _>>()?,
        J::String(s) => s.split([',', ' ']).filter(|x| !x.is_empty()).map(|x| x.parse::<f64>().map_err(|_| format!("'{key}' must be \"start,end\" in seconds"))).collect::<Result<_, _>>()?,
        _ => return Err(format!("'{key}' must be [start, end] in seconds")),
    };
    match nums.as_slice() {
        [x, y] if y > x => Ok(Some([*x, *y])),
        _ => Err(format!("'{key}' must be [start, end] with end > start (got {v})")),
    }
}

/// Rounds to `d` decimals for compact JSON.
pub fn r(v: f64, d: i32) -> f64 {
    if !v.is_finite() {
        return v;
    }
    let m = 10f64.powi(d);
    let x = (v * m).round() / m;
    if x == 0.0 {
        0.0
    } else {
        x
    }
}

/// Timeline/source seconds rounded to milliseconds.
pub fn t3(v: f64) -> f64 {
    r(v, 3)
}

/// `[start, end]` rounded.
pub fn span_j(a: f64, b: f64) -> J {
    json!([t3(a), t3(b)])
}

/// "12.40" style seconds.
pub fn secs(t: f64) -> String {
    let s = format!("{t:.2}");
    if s == "-0.00" {
        "0.00".into()
    } else {
        s
    }
}

/// A number with up to three decimals, trailing zeros removed.
pub fn num(v: f64) -> String {
    let mut s = format!("{v:.3}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        "0".into()
    } else {
        s
    }
}

/// YouTube-style timestamp: `m:ss`, or `h:mm:ss` when `long`.
pub fn timestamp(t: f64, long: bool) -> String {
    let s = t.max(0.0).floor() as u64;
    let (h, m, sec) = (s / 3600, (s / 60) % 60, s % 60);
    if long || h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Absolute path for a user-supplied path string, relative to `base` (else the current directory).
pub fn abs_path(p: &str, base: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(p.trim().trim_matches('"'));
    if p.is_absolute() {
        return normalize(&p);
    }
    if let Some(b) = base {
        return normalize(&b.join(p));
    }
    normalize(&std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p))
}

/// Removes `.` and `..` components lexically (`a/b/../c` → `a/c`).
pub fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Shortens text to `max` characters with an ellipsis.
pub fn short(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// File name of a path string.
pub fn file_name(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// Edit distance for "did you mean" suggestions (optimal string alignment: a swap of two neighbouring
/// characters counts as one edit, so "titel" is 1 away from "title").
pub fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let w = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * w];
    for (j, x) in d.iter_mut().enumerate().take(w) {
        *x = j;
    }
    for i in 1..=a.len() {
        d[i * w] = i;
        for j in 1..=b.len() {
            let cost = usize::from(!a[i - 1].eq_ignore_ascii_case(&b[j - 1]));
            let mut v = (d[(i - 1) * w + j] + 1).min(d[i * w + j - 1] + 1).min(d[(i - 1) * w + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1].eq_ignore_ascii_case(&b[j - 2]) && a[i - 2].eq_ignore_ascii_case(&b[j - 1]) {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
        }
    }
    d[a.len() * w + b.len()]
}

/// `" Did you mean x?"` when a candidate is close to `name`, else "".
pub fn did_you_mean<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    let limit = (name.len() / 3).max(1) + usize::from(name.len() > 6);
    let best = candidates.into_iter().filter(|c| *c != name).map(|c| (levenshtein(name, c), c)).filter(|(d, _)| *d <= limit).min_by_key(|(d, _)| *d);
    match best {
        Some((_, c)) => format!(" Did you mean {c}?"),
        None => String::new(),
    }
}

/// Lower-case word without punctuation, for transcript search.
pub fn norm_word(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(timestamp(0.0, false), "0:00");
        assert_eq!(timestamp(83.9, false), "1:23");
        assert_eq!(timestamp(3725.0, false), "1:02:05");
        assert_eq!(timestamp(65.0, true), "0:01:05");
        assert_eq!(num(2.5000), "2.5");
        assert_eq!(num(-0.0001), "0");
        assert_eq!(secs(-0.001), "0.00");
        assert_eq!(arg_range(&json!({"r": "1.5,3"}), "r").unwrap(), Some([1.5, 3.0]));
        assert!(arg_range(&json!({"r": [3, 1]}), "r").is_err());
        assert_eq!(arg_strs(&json!({"c": "c1, c2"}), "c").unwrap(), Some(vec!["c1".to_string(), "c2".to_string()]));
        assert_eq!(arg_f64(&json!({"t": "12.5s"}), "t").unwrap(), Some(12.5));
        assert_eq!(arg_usize(&json!({"n": 3.0}), "n").unwrap(), Some(3));
        assert!(arg_usize(&json!({"n": 3.5}), "n").is_err());
        assert!(arg_int(&json!({"w": 4294967312u64}), "w", 16, 16384).is_err(), "never truncated");
        assert!(arg_usize(&json!({"n": 1.8446744073709552e19}), "n").is_err());
        assert_eq!(arg_int_clamped(&json!({"c": 1e19}), "c", 0, 50).unwrap(), Some(50));
        assert_eq!(int_of(&json!(3.0)), Some(3));
        assert_eq!(int_of(&json!(3.2)), None);
        assert!(arg_bool(&json!({"b": "maybe"}), "b", false).is_err());
        assert_eq!(norm_word(" Hello,"), "hello");
        assert_eq!(did_you_mean("splt_clip", ["split_clip", "trim_clip"]), " Did you mean split_clip?");
        assert_eq!(short("abcdef", 4), "abc…");
        assert_eq!(file_name("a/b\\c.mp4"), "c.mp4");
    }
}
