//! Transcript tokens as text people read.
//!
//! Word-timing tools (Whisper and friends) split numbers and joined words into separate tokens:
//! `"$2" ".11"`, `"6" ",000"`, `"four" "-hour"`, `"it" "'s"`. Joined with spaces they read
//! `"$2 .11"`. [`joins`] says when a token continues the previous one; [`join_words`] and
//! [`word_units`] build readable text and the units captions highlight as one word. Captions,
//! summaries and every other transcript-derived text use these.

/// Characters a token starts with when it continues the previous token (no space before it).
const CONTINUES: &[char] = &['.', ',', '%', '\'', '\u{2019}', '-', ':', ';', ')', ']', '}', '!', '?', '\u{201D}', '\u{00BB}', '\u{2026}'];
/// Characters a token ends with when the next token continues it (no space after it).
const OPENS: &[char] = &['$', '\u{00A3}', '\u{20AC}', '(', '[', '{', '-', '\u{201C}', '\u{00AB}', '\u{2018}'];

fn has_alnum(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

/// `next` continues whatever came before it: it starts with `. , % ' ’ - : ; ) ] } ! ? ” » …`, or it is
/// punctuation only (and does not open something).
fn continues(next: &str) -> bool {
    let Some(first) = next.chars().next() else { return false };
    CONTINUES.contains(&first) || (!has_alnum(next) && !OPENS.contains(&first) && first != '"')
}

/// The token after `prev` continues it: `prev` ends with `$ £ € ( [ { - “ « ‘`, or is a lone opening
/// straight quote.
fn opens(prev: &str) -> bool {
    let Some(last) = prev.chars().last() else { return false };
    OPENS.contains(&last) || (last == '"' && !has_alnum(prev))
}

/// True when the token `next` continues the token `prev` without a space (see the module docs). A lone
/// straight quote `"` is taken as opening here; [`join_words`] and [`word_units`] also tell closing ones
/// apart by counting quotes.
pub fn joins(prev: &str, next: &str) -> bool {
    let (prev, next) = (prev.trim(), next.trim());
    !prev.is_empty() && !next.is_empty() && (continues(next) || opens(prev))
}

/// For each token: does it continue the previous non-empty token (no space before it)?
fn attach_flags<S: AsRef<str>>(tokens: &[S]) -> Vec<bool> {
    let mut out = vec![false; tokens.len()];
    let mut prev: Option<(&str, bool)> = None; // (text, is a closing quote)
    let mut quote_open = false;
    for (i, t) in tokens.iter().enumerate() {
        let t = t.as_ref().trim();
        if t.is_empty() {
            continue;
        }
        let lone_quote = t.starts_with('"') && !has_alnum(t);
        out[i] = match prev {
            None => false,
            // A closing straight quote hugs the word before it; an opening one does not.
            Some(_) if lone_quote => quote_open,
            Some((p, closing_quote)) => continues(t) || (!closing_quote && opens(p)),
        };
        let closing = lone_quote && quote_open;
        if lone_quote {
            quote_open = !quote_open;
        }
        prev = Some((t, closing));
    }
    out
}

/// Ranges of token indices that read as one word (a token and the tokens that continue it). Empty
/// tokens belong to no unit.
pub fn word_units<S: AsRef<str>>(tokens: &[S]) -> Vec<std::ops::Range<usize>> {
    let flags = attach_flags(tokens);
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if t.as_ref().trim().is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(r) if flags[i] => r.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// Transcript tokens as readable text: `["costs", "$2", ".11", "for", "a", "four", "-hour", "run"]`
/// → `"costs $2.11 for a four-hour run"`.
pub fn join_words<S: AsRef<str>>(tokens: &[S]) -> String {
    let flags = attach_flags(tokens);
    let mut s = String::new();
    for (i, t) in tokens.iter().enumerate() {
        let t = t.as_ref().trim();
        if t.is_empty() {
            continue;
        }
        if !s.is_empty() && !flags[i] {
            s.push(' ');
        }
        s.push_str(t);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_numbers_and_words_join() {
        let cases: &[(&[&str], &str)] = &[
            (&["costs", "$2", ".11", "now"], "costs $2.11 now"),
            (&["$189", ".70,", "then"], "$189.70, then"),
            (&["about", "6", ",000", "towers"], "about 6,000 towers"),
            (&["a", "four", "-hour", "run"], "a four-hour run"),
            (&["a", "fly", "-through"], "a fly-through"),
            (&["it", "'s", "fine"], "it's fine"),
            (&["it", "\u{2019}s"], "it\u{2019}s"),
            (&["50", "%", "off"], "50% off"),
            (&["at", "10", ":30"], "at 10:30"),
            (&["really", "?", "Yes", "."], "really? Yes."),
            (&["a", "(", "quiet", ")", "win"], "a (quiet) win"),
            (&["only", "$", "5"], "only $5"),
            (&["well-", "known"], "well-known"),
            (&["he", "said", "\"", "go", "\"", "now"], "he said \"go\" now"),
            (&["\u{201C}", "Hi", "\u{201D}", "she", "said"], "\u{201C}Hi\u{201D} she said"),
            (&["The", "end.", "Next"], "The end. Next"),
            (&["say", "\"hello\"", "twice"], "say \"hello\" twice"),
            (&[" spaced ", "", "out"], "spaced out"),
        ];
        for (tokens, want) in cases {
            assert_eq!(join_words(tokens), *want, "{tokens:?}");
        }
        assert_eq!(word_units(&["costs", "$2", ".11", "for", "four", "-hour"]), vec![0..1, 1..3, 3..4, 4..6]);
        assert_eq!(word_units(&["a", "", "b"]), vec![0..1, 2..3]);
        assert!(joins("6", ",000") && joins("$", "5") && !joins("the", "end"));
    }
}
