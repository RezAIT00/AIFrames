//! Word timings from files and scripts: openai-whisper JSON, word-list sidecars (`NN.words.json`:
//! `[{"word","start","end"}]`), SRT and WebVTT (segment timings, words spread over each segment), the
//! sidecar next to a media file, and aligning a transcript to a known script (keep the recognised
//! timings, take spelling, case and punctuation from the script).

use aiframes_core::{Transcript, Word};
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};

/// A transcript read from a file, and which format it was.
#[derive(Clone, Debug)]
pub struct Imported {
    pub transcript: Transcript,
    /// "whisper_json" | "words_json" | "srt" | "vtt"
    pub format: &'static str,
}

/// Reads word timings (source seconds) from a file; the format is detected from the extension and
/// the content.
pub fn read_transcript(path: &Path) -> Result<Imported, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let text = text.trim_start_matches('\u{feff}');
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let r = match ext.as_str() {
        "srt" => parse_cues(text, "srt"),
        "vtt" => parse_cues(text, "vtt"),
        _ if text.trim_start().starts_with("WEBVTT") => parse_cues(text, "vtt"),
        _ if text.trim_start().starts_with('{') || text.trim_start().starts_with('[') => parse_json(text),
        _ => parse_cues(text, "srt"),
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    if r.transcript.words.is_empty() {
        return Err(format!("{} contains no words", path.display()));
    }
    Ok(r)
}

fn word(text: &str, start: f64, end: f64, conf: f32) -> Option<Word> {
    let t = text.trim();
    (!t.is_empty() && start.is_finite() && end.is_finite() && start >= 0.0).then(|| Word { text: t.to_string(), start, end: end.max(start), conf })
}

fn num(v: &J) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// One `{"word"|"text", "start", "end", "probability"|"conf"|"confidence"?}` entry.
fn json_word(w: &J) -> Option<Word> {
    let text = w.get("word").or_else(|| w.get("text")).and_then(J::as_str)?;
    let conf = w.get("probability").or_else(|| w.get("conf")).or_else(|| w.get("confidence")).and_then(num).map(|c| c.clamp(0.0, 1.0) as f32).unwrap_or(1.0);
    word(text, num(w.get("start")?)?, num(w.get("end")?)?, conf)
}

fn parse_json(text: &str) -> Result<Imported, String> {
    let v: J = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let language = v.get("language").and_then(J::as_str).unwrap_or("").to_string();
    // A plain word list (the `NN.words.json` sidecars) or {"words": [...]}.
    let list = v.as_array().or_else(|| v.get("words").and_then(J::as_array));
    if let Some(list) = list {
        let words: Vec<Word> = list.iter().filter_map(json_word).collect();
        return Ok(Imported { transcript: Transcript { language, words: sorted(words) }, format: "words_json" });
    }
    // openai-whisper: {"segments": [{"start","end","text","words": [...]}]}.
    let segs = v.get("segments").and_then(J::as_array).ok_or("expected a word list, {\"words\": [...]} or whisper JSON with \"segments\"")?;
    let mut words = Vec::new();
    for s in segs {
        match s.get("words").and_then(J::as_array) {
            Some(ws) if !ws.is_empty() => words.extend(ws.iter().filter_map(json_word)),
            _ => {
                let (Some(a), Some(b), Some(t)) = (s.get("start").and_then(num), s.get("end").and_then(num), s.get("text").and_then(J::as_str)) else { continue };
                words.extend(spread(t, a, b));
            }
        }
    }
    Ok(Imported { transcript: Transcript { language, words: sorted(words) }, format: "whisper_json" })
}

fn sorted(mut w: Vec<Word>) -> Vec<Word> {
    w.sort_by(|a, b| a.start.total_cmp(&b.start));
    w
}

/// The words of `text` spread over `[a, b]` in proportion to their length (segment-level timings).
pub fn spread(text: &str, a: f64, b: f64) -> Vec<Word> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let total: usize = toks.iter().map(|t| t.chars().count() + 1).sum();
    if toks.is_empty() || total == 0 || b <= a {
        return Vec::new();
    }
    let mut t = a;
    toks.iter()
        .filter_map(|tok| {
            let d = (b - a) * (tok.chars().count() + 1) as f64 / total as f64;
            let w = word(tok, t, t + d, 0.5);
            t += d;
            w
        })
        .collect()
}

/// `HH:MM:SS,mmm`, `MM:SS.mmm`, `SS.mmm` → seconds.
fn timecode(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    let secs: f64 = parts.last()?.parse().ok()?;
    let mut total = secs;
    let mut mul = 60.0;
    for p in parts.iter().rev().skip(1) {
        total += p.trim().parse::<f64>().ok()? * mul;
        mul *= 60.0;
    }
    Some(total)
}

/// Removes `<...>` tags (WebVTT styling and inline timestamps) from cue text.
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn parse_cues(text: &str, format: &'static str) -> Result<Imported, String> {
    let mut words = Vec::new();
    let mut lines = text.lines().peekable();
    let mut cues = 0;
    while let Some(line) = lines.next() {
        let Some((a, rest)) = line.split_once("-->") else { continue };
        let b = rest.split_whitespace().next().unwrap_or("");
        let (Some(a), Some(b)) = (timecode(a), timecode(b)) else { continue };
        let mut cue = String::new();
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() {
                break;
            }
            cue.push(' ');
            cue.push_str(&strip_tags(l));
            lines.next();
        }
        cues += 1;
        words.extend(spread(&cue, a, b));
    }
    if cues == 0 {
        return Err(format!("no {format} cues (lines like \"00:00:01,000 --> 00:00:03,500\") found"));
    }
    Ok(Imported { transcript: Transcript { language: String::new(), words: sorted(words) }, format })
}

/// The transcript sidecar next to a media file, if any: `<stem>.words.json`, `<stem>.json`,
/// `<stem>.srt`, `<stem>.vtt` (the first that parses into words).
pub fn find_sidecar(media: &Path) -> Option<(PathBuf, Imported)> {
    let stem = media.file_stem()?.to_string_lossy().to_string();
    let dir = media.parent()?;
    for name in [format!("{stem}.words.json"), format!("{stem}.json"), format!("{stem}.srt"), format!("{stem}.vtt")] {
        let p = dir.join(&name);
        if p.is_file() && p != media {
            if let Ok(t) = read_transcript(&p) {
                return Some((p, t));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Alignment to a script
// ---------------------------------------------------------------------------------------------

const ONES: [&str; 20] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

/// A whole number as spoken words ("25" → twenty five, "1200" → one thousand two hundred).
fn number_words(n: u64, out: &mut Vec<String>) {
    if n < 20 {
        out.push(ONES[n as usize].into());
    } else if n < 100 {
        out.push(TENS[(n / 10) as usize].into());
        if !n.is_multiple_of(10) {
            out.push(ONES[(n % 10) as usize].into());
        }
    } else if n < 1000 {
        out.push(ONES[(n / 100) as usize].into());
        out.push("hundred".into());
        if !n.is_multiple_of(100) {
            number_words(n % 100, out);
        }
    } else if n < 1_000_000 {
        number_words(n / 1000, out);
        out.push("thousand".into());
        if !n.is_multiple_of(1000) {
            number_words(n % 1000, out);
        }
    } else {
        out.push(n.to_string());
    }
}

/// The comparable units of a token: lower-case letters/digits, numbers spelled out, hyphenated
/// parts separate, money and percent spoken ("3D" → three d, "tower-defense" → tower defense,
/// "you'd" → youd, "$500" → five hundred dollars, "50%" → fifty percent, "&" → and).
pub fn units(token: &str) -> Vec<String> {
    let t = token.trim();
    if t == "&" {
        return vec!["and".into()];
    }
    let mut out = Vec::new();
    for part in t.split(['-', '—', '–', '/']) {
        let clean: String = part.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
        if clean.is_empty() {
            continue;
        }
        // Leading digits are a number; what follows is a suffix (3d, 4x, 1990s).
        let digits: String = clean.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            out.push(clean);
            continue;
        }
        match digits.parse::<u64>() {
            Ok(n) => number_words(n, &mut out),
            Err(_) => out.push(digits.clone()),
        }
        let rest = &clean[digits.len()..];
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }
    if !out.is_empty() {
        for (sym, word) in [('$', "dollars"), ('€', "euros"), ('£', "pounds"), ('%', "percent")] {
            if t.contains(sym) {
                out.push(word.into());
            }
        }
    }
    out
}

fn is_number_word(u: &str) -> bool {
    ONES.contains(&u) || TENS.iter().any(|t| !t.is_empty() && *t == u) || matches!(u, "hundred" | "thousand" | "million")
}

/// Spoken-number glue, so "a hundred and thirty eight" matches "138": "a" before hundred/thousand
/// is "one", "and" between number words is dropped (`None`).
fn number_glue(units: &[String]) -> Vec<Option<String>> {
    (0..units.len())
        .map(|i| {
            let (prev, next) = (i.checked_sub(1).map(|p| units[p].as_str()), units.get(i + 1).map(String::as_str));
            match units[i].as_str() {
                "a" if matches!(next, Some("hundred" | "thousand" | "million")) => Some("one".into()),
                "and" if prev.is_some_and(is_number_word) && next.is_some_and(is_number_word) => None,
                u => Some(u.to_string()),
            }
        })
        .collect()
}

/// Runs of single capital letters ("A I", "U. S. A.") become one written acronym token ("AI", "USA."):
/// scripts for a speaking voice spell acronyms out.
fn merge_spelled_letters(toks: Vec<String>) -> Vec<String> {
    let core = |t: &str| t.trim_end_matches(['.', ',', '!', '?', ';', ':']).to_string();
    let letter = |t: &str| {
        let c = core(t);
        c.len() == 1 && c.chars().all(|c| c.is_ascii_uppercase())
    };
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let mut j = i;
        // A run never starts with the pronoun "I" ("I A..." stays two words; "A I" is AI).
        while j < toks.len() && letter(&toks[j]) && !(j == i && core(&toks[j]) == "I") {
            j += 1;
        }
        if j - i >= 2 {
            let letters: String = toks[i..j].iter().map(|t| t.trim_end_matches(['.', ',', '!', '?', ';', ':']).to_string()).collect();
            // The last letter's punctuation ends the acronym ("A I." → "AI.").
            let trail = &toks[j - 1][1..];
            out.push(format!("{letters}{trail}"));
            i = j;
        } else {
            out.push(toks[i].clone());
            i += 1;
        }
    }
    out
}

/// How the caption text of aligned words is spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Spelling {
    /// The script's words, case and punctuation — except where the recognised text says the same
    /// thing in written form ("3D", "10", "$500", "50%", "AI"): that written form is kept.
    #[default]
    Written,
    /// Exactly the script's words (a script written for a voice may spell "three D", "ten").
    Script,
}

/// A stretch of the aligned transcript that could not be matched exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    /// Word indices in the new transcript (inclusive).
    pub from: usize,
    pub to: usize,
    /// Script text there.
    pub text: String,
    /// What the recognition heard there.
    pub heard: String,
    /// Source seconds.
    pub start: f64,
    pub end: f64,
}

/// Where the written form was kept instead of the script's spoken spelling.
#[derive(Clone, Debug, PartialEq)]
pub struct Kept {
    /// Word indices in the new transcript (inclusive).
    pub from: usize,
    pub to: usize,
    /// The script's words there and the written form used.
    pub script: String,
    pub written: String,
}

/// The result of [`align`].
#[derive(Clone, Debug)]
pub struct Aligned {
    pub words: Vec<Word>,
    /// Script words matched exactly / matched to a different recognised word / without one (timed by
    /// interpolation) / recognised words the script does not have.
    pub matched: usize,
    pub substituted: usize,
    pub inserted: usize,
    pub dropped: usize,
    pub low_confidence: Vec<Region>,
    /// Written forms kept ([`Spelling::Written`]).
    pub kept_written: Vec<Kept>,
}

/// Most `recognised × script` units aligned in one call (the alignment table is u16 per cell).
pub const MAX_ALIGN_CELLS: usize = 12_000_000;

/// [`align_with`] with [`Spelling::Written`].
pub fn align(words: &[Word], script: &str) -> Result<Aligned, String> {
    align_with(words, script, Spelling::Written)
}

#[derive(Clone, Copy, PartialEq)]
enum How {
    Exact,
    Sub,
    None,
}

fn written_form(t: &str) -> bool {
    t.chars().any(|c| c.is_ascii_digit() || "$€£%&".contains(c))
        || t.split_whitespace().any(|w| {
            let core: String = w.chars().filter(|c| c.is_alphanumeric()).collect();
            core.len() >= 2 && core.chars().all(|c| c.is_ascii_uppercase())
        })
}

fn same_words(a: &str, b: &str) -> bool {
    let n = |s: &str| -> String { s.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).flat_map(char::to_lowercase).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ") };
    n(a) == n(b)
}

const TRAIL: &[char] = &['.', ',', '!', '?', ';', ':', '"', '\'', ')', ']', '…'];
const LEAD: &[char] = &['"', '\'', '(', '[', '¿', '¡'];

/// Aligns recognised `words` to a known `script`: every script word gets the timing of the
/// recognised word(s) it matches (numbers spoken as words and written as digits match), script words
/// without a match are timed between their neighbours and reported as low-confidence regions. With
/// [`Spelling::Written`] a script passage that only spells out what the recognition wrote ("three D"
/// for "3D", "ten" for "10") keeps the written form, with the script's case and punctuation.
pub fn align_with(words: &[Word], script: &str, spelling: Spelling) -> Result<Aligned, String> {
    let mut toks: Vec<String> = script.split_whitespace().map(String::from).collect();
    if spelling == Spelling::Written {
        toks = merge_spelled_letters(toks);
    }
    if toks.is_empty() {
        return Err("the script is empty".into());
    }
    if words.is_empty() {
        return Err("the transcript has no words to take timings from".into());
    }
    // Units of both sides with their token and (recognised side) time slice.
    let mut a: Vec<(String, usize, f64, f64)> = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let u = units(&w.text);
        let n = u.len().max(1) as f64;
        for (k, unit) in u.into_iter().enumerate() {
            let d = (w.end - w.start) / n;
            a.push((unit, i, w.start + d * k as f64, w.start + d * (k + 1) as f64));
        }
    }
    let mut b: Vec<(String, usize)> = Vec::new();
    for (j, t) in toks.iter().enumerate() {
        for unit in units(t) {
            b.push((unit, j));
        }
    }
    let ga = number_glue(&a.iter().map(|x| x.0.clone()).collect::<Vec<_>>());
    a = a.into_iter().zip(ga).filter_map(|(x, g)| g.map(|u| (u, x.1, x.2, x.3))).collect();
    let gb = number_glue(&b.iter().map(|x| x.0.clone()).collect::<Vec<_>>());
    b = b.into_iter().zip(gb).filter_map(|(x, g)| g.map(|u| (u, x.1))).collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return Err("nothing comparable in the transcript or the script".into());
    }
    if n.saturating_mul(m) > MAX_ALIGN_CELLS || n + m >= u16::MAX as usize {
        return Err(format!("{n} transcript × {m} script units is too long to align at once; align a section with from/to (word indices) and the matching part of the script"));
    }
    // Edit-distance table; costs: match 0, substitution 1, insertion/deletion 1.
    let w = m + 1;
    let mut d = vec![0u16; (n + 1) * w];
    for (j, x) in d.iter_mut().enumerate().take(w) {
        *x = j as u16;
    }
    for i in 1..=n {
        d[i * w] = i as u16;
        for j in 1..=m {
            let sub = d[(i - 1) * w + j - 1] + u16::from(a[i - 1].0 != b[j - 1].0);
            d[i * w + j] = sub.min(d[(i - 1) * w + j] + 1).min(d[i * w + j - 1] + 1);
        }
    }
    // Backtrace: per script unit, the recognised unit it aligns to (and the reverse).
    let mut map: Vec<Option<(usize, bool)>> = vec![None; m];
    let mut rmap: Vec<Option<(usize, bool)>> = vec![None; n];
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 {
            let eq = a[i - 1].0 == b[j - 1].0;
            if d[i * w + j] == d[(i - 1) * w + j - 1] + u16::from(!eq) {
                map[j - 1] = Some((i - 1, eq));
                rmap[i - 1] = Some((j - 1, eq));
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if i > 0 && d[i * w + j] == d[(i - 1) * w + j] + 1 {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    // Per script token: its time span and how it matched.
    let tn = toks.len();
    let mut span: Vec<Option<(f64, f64)>> = vec![None; tn];
    let mut how = vec![How::Exact; tn];
    for (k, (_, tok)) in b.iter().enumerate() {
        match map[k] {
            Some((ai, eq)) => {
                let (s, e) = (a[ai].2, a[ai].3);
                span[*tok] = Some(match span[*tok] {
                    Some((x, y)) => (x.min(s), y.max(e)),
                    None => (s, e),
                });
                if !eq && how[*tok] == How::Exact {
                    how[*tok] = How::Sub;
                }
            }
            None => {
                if span[*tok].is_none() {
                    how[*tok] = How::None;
                }
            }
        }
    }
    for (t, h) in how.iter_mut().enumerate() {
        if *h == How::None && span[t].is_some() {
            *h = How::Sub;
        }
    }
    // Interpolate unmatched script tokens between their neighbours.
    let total_start = words.first().map(|w| w.start).unwrap_or(0.0);
    let total_end = words.iter().map(|w| w.end).fold(total_start, f64::max);
    let mut k = 0;
    while k < tn {
        if span[k].is_some() {
            k += 1;
            continue;
        }
        let run_start = k;
        while k < tn && span[k].is_none() {
            k += 1;
        }
        let prev_end = if run_start > 0 { span[run_start - 1].map(|s| s.1).unwrap_or(total_start) } else { total_start };
        let next_start = if k < tn { span[k].map(|s| s.0).unwrap_or(total_end) } else { total_end };
        let (lo, hi) = (prev_end, next_start.max(prev_end));
        let cnt = (k - run_start) as f64;
        for (q, t) in (run_start..k).enumerate() {
            span[t] = Some((lo + (hi - lo) * q as f64 / cnt, lo + (hi - lo) * (q + 1) as f64 / cnt));
        }
    }
    // Written forms: groups of script tokens and recognised words linked only by exact unit matches,
    // covering each other completely ("three D" ↔ "3D", "Ten" ↔ "10").
    let mut replace: Vec<Option<(usize, Vec<usize>)>> = vec![None; tn];
    if spelling == Spelling::Written {
        let mut parent: Vec<usize> = (0..tn + words.len()).collect();
        fn find(p: &mut [usize], x: usize) -> usize {
            let mut r = x;
            while p[r] != r {
                r = p[r];
            }
            let mut c = x;
            while p[c] != r {
                let next = p[c];
                p[c] = r;
                c = next;
            }
            r
        }
        for (k, (_, tok)) in b.iter().enumerate() {
            if let Some((ai, true)) = map[k] {
                let (x, y) = (find(&mut parent, *tok), find(&mut parent, tn + a[ai].1));
                parent[x] = y;
            }
        }
        let mut groups: std::collections::BTreeMap<usize, (Vec<usize>, Vec<usize>)> = std::collections::BTreeMap::new();
        for t in 0..tn {
            let r = find(&mut parent, t);
            groups.entry(r).or_default().0.push(t);
        }
        for wi in 0..words.len() {
            let r = find(&mut parent, tn + wi);
            groups.entry(r).or_default().1.push(wi);
        }
        for (s, r) in groups.values() {
            if s.is_empty() || r.is_empty() {
                continue;
            }
            let (s0, s1) = (s[0], s[s.len() - 1]);
            let (r0, r1) = (r[0], r[r.len() - 1]);
            // Tokens inside the run without comparable units (punctuation, number glue like "and") belong to it.
            let s_ok = (s0..=s1).all(|t| s.contains(&t) || !b.iter().any(|x| x.1 == t));
            let r_ok = r1 - r0 + 1 == r.len();
            let all_s = b.iter().enumerate().filter(|(_, (_, t))| s.contains(t)).all(|(k, _)| matches!(map[k], Some((ai, true)) if r.contains(&a[ai].1)));
            let all_r = a.iter().enumerate().filter(|(_, x)| r.contains(&x.1)).all(|(ai, _)| matches!(rmap[ai], Some((k, true)) if s.contains(&b[k].1)));
            if !(s_ok && r_ok && all_s && all_r) {
                continue;
            }
            let s_text = toks[s0..=s1].join(" ");
            let r_text: Vec<&str> = words[r0..=r1].iter().map(|w| w.text.trim()).collect();
            let r_text = aiframes_core::join_words(&r_text);
            if written_form(&r_text) && !written_form(&s_text) && !same_words(&s_text, &r_text) {
                replace[s0] = Some((s1 + 1, (r0..=r1).collect()));
            }
        }
    }
    // Output words (with how each matched), kept in time order.
    let mut out: Vec<(Word, How)> = Vec::with_capacity(tn);
    let mut kept = Vec::new();
    let mut t = 0;
    while t < tn {
        if let Some((end, rws)) = &replace[t] {
            let first = out.len();
            let lead: String = toks[t].chars().take_while(|c| LEAD.contains(c)).collect();
            let trail: String = {
                let last = &toks[end - 1];
                let core_len = last.trim_end_matches(TRAIL).len();
                last[core_len..].to_string()
            };
            let cap = toks[t].trim_start_matches(LEAD).chars().next().is_some_and(char::is_uppercase);
            for (q, wi) in rws.iter().enumerate() {
                let mut text = words[*wi].text.trim().trim_start_matches(LEAD).trim_end_matches(TRAIL).to_string();
                if q == 0 {
                    if cap {
                        let mut c = text.chars();
                        if let Some(f) = c.next() {
                            text = f.to_uppercase().chain(c).collect();
                        }
                    }
                    text = format!("{lead}{text}");
                }
                if q == rws.len() - 1 {
                    text.push_str(&trail);
                }
                out.push((Word { text, start: words[*wi].start, end: words[*wi].end, conf: 1.0 }, How::Exact));
            }
            let written: Vec<&str> = out[first..].iter().map(|(w, _)| w.text.as_str()).collect();
            kept.push(Kept { from: first, to: out.len() - 1, script: toks[t..*end].join(" "), written: aiframes_core::join_words(&written) });
            t = *end;
            continue;
        }
        let (s, e) = span[t].unwrap_or((total_start, total_start));
        let conf = match how[t] {
            How::Exact => 1.0,
            How::Sub => 0.5,
            How::None => 0.2,
        };
        out.push((Word { text: toks[t].clone(), start: s, end: e, conf }, how[t]));
        t += 1;
    }
    let mut prev_start = 0.0f64;
    for (w, _) in out.iter_mut() {
        w.start = w.start.max(prev_start);
        w.end = w.end.max(w.start);
        prev_start = w.start;
    }
    // Reports.
    let heard_between = |s: f64, e: f64| -> String {
        let v: Vec<&str> = words.iter().filter(|w| w.end > s + 1e-6 && w.start < e - 1e-6).map(|w| w.text.trim()).collect();
        aiframes_core::join_words(&v)
    };
    let mut low = Vec::new();
    let mut q = 0;
    while q < out.len() {
        if out[q].1 == How::Exact {
            q += 1;
            continue;
        }
        let q0 = q;
        while q < out.len() && out[q].1 != How::Exact {
            q += 1;
        }
        let (start, end) = (out[q0].0.start, out[q - 1].0.end);
        let text: Vec<&str> = out[q0..q].iter().map(|(w, _)| w.text.as_str()).collect();
        low.push(Region { from: q0, to: q - 1, text: aiframes_core::join_words(&text), heard: heard_between(start, end), start, end });
    }
    let used: std::collections::HashSet<usize> = rmap.iter().enumerate().filter(|(_, m)| m.is_some()).map(|(ai, _)| a[ai].1).collect();
    Ok(Aligned {
        matched: how.iter().filter(|h| **h == How::Exact).count(),
        substituted: how.iter().filter(|h| **h == How::Sub).count(),
        inserted: how.iter().filter(|h| **h == How::None).count(),
        dropped: (0..words.len()).filter(|wi| !used.contains(wi)).count(),
        words: out.into_iter().map(|(w, _)| w).collect(),
        low_confidence: low,
        kept_written: kept,
    })
}

/// A kept written form as JSON for tool results.
pub fn kept_json(k: &Kept) -> J {
    json!({"from": k.from, "to": k.to, "script": k.script, "written": k.written})
}

/// A region as JSON for tool results.
pub fn region_json(r: &Region) -> J {
    json!({"from": r.from, "to": r.to, "script": r.text, "heard": r.heard, "src": [crate::util::t3(r.start), crate::util::t3(r.end)]})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, s: f64, e: f64) -> Word {
        Word { text: text.into(), start: s, end: e, conf: 1.0 }
    }

    #[test]
    fn units_spell_numbers_and_split_parts() {
        assert_eq!(units("10"), vec!["ten"]);
        assert_eq!(units("3D"), vec!["three", "d"]);
        assert_eq!(units("Twenty-five,"), vec!["twenty", "five"]);
        assert_eq!(units("25"), vec!["twenty", "five"]);
        assert_eq!(units("You'd"), vec!["youd"]);
        assert_eq!(units("1200"), vec!["one", "thousand", "two", "hundred"]);
        assert!(units("—").is_empty());
    }

    #[test]
    fn align_takes_script_spelling_and_keeps_timings() {
        let heard = vec![w("ten", 0.0, 0.3), w("minutes", 0.3, 0.8), w("1", 1.0, 1.2), w("hour", 1.2, 1.6), w("and", 2.0, 2.1), w("for", 2.1, 2.3), w("hours", 2.3, 2.8)];
        let a = align_with(&heard, "Ten minutes. One hour. And four hours.", Spelling::Script).unwrap();
        let texts: Vec<&str> = a.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["Ten", "minutes.", "One", "hour.", "And", "four", "hours."]);
        assert_eq!((a.words[2].start, a.words[2].end), (1.0, 1.2), "digits matched as words keep their timing");
        assert_eq!(a.substituted, 1, "for ≠ four");
        assert_eq!(a.low_confidence.len(), 1);
        assert_eq!(a.low_confidence[0].text, "four");
        assert_eq!(a.low_confidence[0].heard, "for");
    }

    #[test]
    fn align_interpolates_missing_and_drops_extra() {
        let heard = vec![w("make", 0.0, 0.2), w("a", 0.2, 0.3), w("3D", 0.3, 0.7), w("uh", 0.7, 0.9), w("game", 1.0, 1.4)];
        let a = align_with(&heard, "Make a three D Bloons game.", Spelling::Script).unwrap();
        let texts: Vec<&str> = a.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["Make", "a", "three", "D", "Bloons", "game."]);
        assert!((a.words[2].start - 0.3).abs() < 1e-9 && (a.words[3].end - 0.7).abs() < 1e-9, "3D is split over three D: {:?}", a.words);
        assert!(a.words[4].start >= a.words[3].end - 1e-9 && a.words[4].end <= a.words[5].start + 1e-9, "Bloons fits between");
        assert!(a.words.windows(2).all(|p| p[1].start >= p[0].start), "in time order");
        assert_eq!(a.dropped, 0, "uh was substituted by Bloons");
        assert!(a.low_confidence.iter().any(|r| r.text.contains("Bloons")));
    }

    #[test]
    fn written_forms_are_kept() {
        let heard = vec![
            w("Make", 0.0, 0.2),
            w("a", 0.2, 0.3),
            w("3D", 0.3, 0.7),
            w("game.", 0.7, 1.0),
            w("10", 1.2, 1.5),
            w("minutes,", 1.5, 1.9),
            w("$500", 2.0, 2.6),
            w("and", 2.6, 2.7),
            w("50%", 2.7, 3.1),
            w("of", 3.1, 3.2),
            w("AI", 3.2, 3.5),
            w("138", 3.6, 4.2),
            w("lives", 4.2, 4.5),
        ];
        let script = "Make a three D game. Ten minutes. Five hundred dollars and fifty percent of A I, a hundred and thirty eight lives.";
        let a = align(&heard, script).unwrap();
        let texts: Vec<&str> = a.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["Make", "a", "3D", "game.", "10", "minutes.", "$500", "and", "50%", "of", "AI,", "138", "lives."]);
        assert_eq!((a.words[2].start, a.words[2].end), (0.3, 0.7));
        let kept: Vec<(&str, &str)> = a.kept_written.iter().map(|k| (k.script.as_str(), k.written.as_str())).collect();
        assert_eq!(kept, vec![("three D", "3D"), ("Ten", "10"), ("Five hundred dollars", "$500"), ("fifty percent", "50%"), ("a hundred and thirty eight", "138")]);
        assert!(a.low_confidence.is_empty(), "{:?}", a.low_confidence);
        // Real word corrections still come from the script; script spelling is still available.
        let a = align(&[w("clawed", 0.0, 0.4), w("10", 0.5, 0.8)], "Claude ten.").unwrap();
        assert_eq!(a.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), vec!["Claude", "10."]);
        let a = align_with(&heard[..4], "Make a three D game.", Spelling::Script).unwrap();
        assert_eq!(a.words[2].text, "three");
        assert!(a.kept_written.is_empty());
    }

    #[test]
    fn formats() {
        let whisper = r#"{"language":"en","segments":[{"start":0,"end":2,"text":" Hello there","words":[{"word":" Hello","start":0.1,"end":0.5,"probability":0.9},{"word":" there","start":0.6,"end":1.0}]},{"start":2,"end":3,"text":"no words here"}]}"#;
        let r = parse_json(whisper).unwrap();
        assert_eq!(r.format, "whisper_json");
        assert_eq!(r.transcript.language, "en");
        assert_eq!(r.transcript.words.len(), 5);
        assert_eq!(r.transcript.words[0].text, "Hello");
        assert!((r.transcript.words[0].conf - 0.9).abs() < 1e-6);
        let side = r#"[{"word":"I","start":0.0,"end":0.16},{"word":"gave","start":0.16,"end":0.36}]"#;
        assert_eq!(parse_json(side).unwrap().transcript.words.len(), 2);
        let srt = "1\n00:00:01,000 --> 00:00:03,000\nHello big world\n\n2\n00:00:04,000 --> 00:00:05,000\n<i>Bye</i>\n";
        let r = parse_cues(srt, "srt").unwrap();
        assert_eq!(r.transcript.words.len(), 4);
        assert!((r.transcript.words[0].start - 1.0).abs() < 1e-9 && (r.transcript.words[2].end - 3.0).abs() < 1e-9);
        assert_eq!(r.transcript.words[3].text, "Bye");
        let vtt = "WEBVTT\n\n00:01.500 --> 00:02.500 align:start\nOne <c.x>two</c>\n";
        let r = parse_cues(vtt, "vtt").unwrap();
        assert_eq!(r.transcript.words.len(), 2);
        assert!((r.transcript.words[0].start - 1.5).abs() < 1e-9);
        assert!(parse_cues("nothing here", "srt").is_err());
    }
}
