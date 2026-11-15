//! Small formatting and suggestion helpers shared by the modules (crate-private).

/// Levenshtein edit distance (byte-wise; ids are ASCII in practice).
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(!ca.eq_ignore_ascii_case(&cb));
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The candidate closest to `name`, if it is close enough to be a plausible typo.
pub(crate) fn suggest<'a, I: IntoIterator<Item = &'a str>>(name: &str, candidates: I) -> Option<&'a str> {
    let limit = (name.len() / 3).max(1) + usize::from(name.len() > 6);
    let mut best: Option<(usize, &'a str)> = None;
    for c in candidates {
        if c == name {
            continue;
        }
        let d = levenshtein(name, c);
        if d <= limit && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

/// `" Did you mean x?"` or an empty string.
pub(crate) fn did_you_mean<'a, I: IntoIterator<Item = &'a str>>(name: &str, candidates: I) -> String {
    match suggest(name, candidates) {
        Some(s) => format!(" Did you mean {s}?"),
        None => String::new(),
    }
}

/// Seconds with two decimals (never "-0.00").
pub(crate) fn secs(t: f64) -> String {
    let s = format!("{t:.2}");
    if s == "-0.00" {
        "0.00".into()
    } else {
        s
    }
}

/// Seconds with two decimals when that is exact, else three (so different frames never print alike:
/// 2.00, 2.033, 7.267).
pub(crate) fn tsecs(t: f64) -> String {
    let c = t * 100.0;
    if (c - c.round()).abs() < 1e-6 {
        secs(t)
    } else {
        let s = format!("{t:.3}");
        if s == "-0.000" {
            "0.000".into()
        } else {
            s
        }
    }
}

/// A number with up to three decimals and trailing zeros removed: `2`, `0.5`, `1.25`.
pub(crate) fn num(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
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

/// Signed offset for display: `+0.3`, `-1.2`.
pub(crate) fn signed(v: f64) -> String {
    if v >= 0.0 {
        format!("+{}", num(v))
    } else {
        num(v)
    }
}

/// A change of a timeline edge as it shows: on a frame grid (`fps`) `+1.000 s (30 frames)`, else
/// `+1.25 s`; `asked` (what was requested) is added when it differs: `+0.500 s (15 frames; asked +0.51 s)`.
pub(crate) fn change(d: f64, fps: Option<f64>, asked: Option<f64>) -> String {
    let mut s = match fps {
        Some(f) => {
            let (num, den) = crate::resolve::frame_rate(f);
            let n = (d * num as f64 / den as f64).round() as i64;
            let v = format!("{:+.3}", d);
            format!("{} s ({} frame{}", if v == "-0.000" { "+0.000".into() } else { v }, n, if n.abs() == 1 { "" } else { "s" })
        }
        None => format!("{} s", signed(d)),
    };
    if let Some(a) = asked.filter(|a| (a - d).abs() > 1e-6) {
        s.push_str(&format!("{}asked {} s", if fps.is_some() { "; " } else { " (" }, signed(a)));
        s.push(')');
    } else if fps.is_some() {
        s.push(')');
    }
    s
}

/// Joins at most `max` items, then `… (+N more)`.
pub(crate) fn join_limited<S: AsRef<str>>(items: &[S], max: usize) -> String {
    let mut out: Vec<&str> = items.iter().take(max).map(|s| s.as_ref()).collect();
    let more = items.len().saturating_sub(max);
    let mut s = out.join(", ");
    if more > 0 {
        s.push_str(&format!(" … (+{more} more)"));
    }
    out.clear();
    s
}

/// Truncates text for one-line display, adding an ellipsis.
pub(crate) fn short(text: &str, max_chars: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        one_line
    } else {
        let mut s: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

/// The file-name part of a media path (either slash).
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_and_formatting() {
        assert_eq!(levenshtein("c9", "c8"), 1);
        assert_eq!(suggest("crosfade", ["crossfade", "dip_black"]), Some("crossfade"));
        assert_eq!(suggest("zzz", ["crossfade"]), None);
        assert_eq!(num(2.0), "2");
        assert_eq!(num(0.5), "0.5");
        assert_eq!(num(-0.0001), "0");
        assert_eq!(secs(-0.001), "0.00");
        assert_eq!(signed(0.3), "+0.3");
        assert_eq!(join_limited(&["a", "b", "c"], 2), "a, b … (+1 more)");
        assert_eq!(short("hello   big world", 8), "hello b…");
        assert_eq!(file_name("C:\\x\\y/clip.mp4"), "clip.mp4");
    }
}
