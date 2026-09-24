//! Text normalization for *matching* (not for speaking; see [`crate::speech`]).
//!
//! STT output varies in punctuation, niqqud, quote marks and spacing. Everything the fast
//! path compares goes through [`normalize`] first, on both sides, so config phrases and
//! transcripts meet in the same form.

use regex::Regex;

/// Hebrew one-letter prefixes that attach to the next word (ו, ה, ב, ל, מ, ש, כ).
pub const HEBREW_PREFIXES: &str = "והבלמשכ";

/// Lowercase, strip niqqud/cantillation, drop quote marks and gershayim inside words
/// (`נתב"ג` → `נתבג`), turn other punctuation into spaces, collapse whitespace.
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = true;
    for ch in input.chars() {
        let c = match ch {
            // Niqqud and cantillation marks.
            '\u{0591}'..='\u{05C7}' if ch != '\u{05BE}' => continue,
            // Gershayim and double quotes mark acronyms (נתב"ג): dropped.
            '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' => continue,
            // Geresh changes pronunciation (ז'בוטינסקי, צ'יפס): kept, in one spelling.
            '\'' | '\u{05F3}' | '\u{2019}' | '`' => '\'',
            // Maqaf and dashes join words: treat as space.
            '\u{05BE}' | '-' | '\u{2013}' | '\u{2014}' => ' ',
            c if c.is_alphanumeric() => c,
            ':' => ':',
            _ => ' ',
        };
        if c == ' ' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

pub fn tokens(normalized: &str) -> Vec<&str> {
    normalized.split(' ').filter(|t| !t.is_empty()).collect()
}

/// A compiled phrase: matches the normalized phrase as whole words, optionally allowing up
/// to two attached Hebrew prefix letters on its first word ("והמונית", "למונית").
#[derive(Debug, Clone)]
pub struct Phrase {
    pub text: String,
    regex: Regex,
    words: usize,
}

impl Phrase {
    pub fn new(phrase: &str, allow_prefixes: bool) -> Result<Self, regex::Error> {
        let text = normalize(phrase);
        let prefix = if allow_prefixes { format!("[{HEBREW_PREFIXES}]{{0,2}}") } else { String::new() };
        let regex = Regex::new(&format!(r"(?:^| ){prefix}{}(?: |$)", regex::escape(&text)))?;
        let words = tokens(&text).len();
        Ok(Self { text, regex, words })
    }

    /// Byte range of the first match in `normalized`, excluding the separating spaces.
    pub fn find(&self, normalized: &str) -> Option<(usize, usize)> {
        self.regex.find(normalized).map(|m| {
            let s = m.as_str();
            let start = m.start() + usize::from(s.starts_with(' '));
            let end = m.end() - usize::from(s.ends_with(' ') && s.len() > 1);
            (start, end)
        })
    }

    pub fn word_count(&self) -> usize {
        self.words
    }
}

/// A set of phrases, longest first so "לא הבנתי" wins over "לא".
#[derive(Debug, Clone, Default)]
pub struct PhraseSet {
    phrases: Vec<Phrase>,
}

impl PhraseSet {
    pub fn new<'a>(items: impl IntoIterator<Item = &'a String>, allow_prefixes: bool) -> Result<Self, regex::Error> {
        let mut phrases = items
            .into_iter()
            .filter(|p| !normalize(p).is_empty())
            .map(|p| Phrase::new(p, allow_prefixes))
            .collect::<Result<Vec<_>, _>>()?;
        phrases.sort_by(|a, b| b.text.chars().count().cmp(&a.text.chars().count()));
        Ok(Self { phrases })
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }

    /// Longest phrase found anywhere, with its span.
    pub fn find(&self, normalized: &str) -> Option<(&Phrase, (usize, usize))> {
        self.phrases.iter().find_map(|p| p.find(normalized).map(|span| (p, span)))
    }

    /// True when a phrase covers the whole utterance, ignoring the given filler words.
    pub fn matches_whole(&self, normalized: &str, fillers: &PhraseSet) -> bool {
        let stripped = fillers.strip(normalized);
        self.phrases.iter().any(|p| p.text == stripped)
    }

    /// Remove every occurrence of any phrase in the set.
    pub fn strip(&self, normalized: &str) -> String {
        let mut toks: Vec<&str> = tokens(normalized);
        for p in &self.phrases {
            let pt: Vec<&str> = tokens(&p.text);
            if pt.is_empty() {
                continue;
            }
            let mut i = 0;
            while i + pt.len() <= toks.len() {
                if toks[i..i + pt.len()] == pt[..] {
                    toks.drain(i..i + pt.len());
                } else {
                    i += 1;
                }
            }
        }
        toks.join(" ")
    }

    /// Byte offset of the earliest occurrence of any phrase.
    pub fn first_position(&self, normalized: &str) -> Option<usize> {
        self.phrases.iter().filter_map(|p| p.find(normalized).map(|(s, _)| s)).min()
    }

    /// How many distinct phrases occur in `normalized`.
    pub fn count(&self, normalized: &str) -> usize {
        self.phrases.iter().filter(|p| p.find(normalized).is_some()).count()
    }

    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.phrases.iter().map(|p| p.text.as_str())
    }
}

/// Tracks which bytes of an utterance the fast path has explained, to decide whether the
/// LLM is needed.
#[derive(Debug, Clone)]
pub struct Coverage {
    covered: Vec<bool>,
    text: String,
}

impl Coverage {
    pub fn new(normalized: &str) -> Self {
        Self { covered: vec![false; normalized.len()], text: normalized.to_string() }
    }

    pub fn mark(&mut self, span: (usize, usize)) {
        let end = span.1.min(self.covered.len());
        for c in &mut self.covered[span.0.min(end)..end] {
            *c = true;
        }
    }

    pub fn mark_words(&mut self, set: &PhraseSet) {
        for p in &set.phrases {
            let mut from = 0;
            while from < self.text.len() {
                let Some((s, e)) = p.find(&self.text[from..]) else { break };
                self.mark((from + s, from + e));
                from += e.max(1);
                while from < self.text.len() && !self.text.is_char_boundary(from) {
                    from += 1;
                }
            }
        }
    }

    pub fn is_covered(&self, byte: usize) -> bool {
        self.covered.get(byte).copied().unwrap_or(false)
    }

    /// The text with every explained character blanked to `_` (byte offsets preserved), so
    /// slot patterns only see what is still unexplained.
    pub fn masked(&self) -> String {
        let mut out = String::with_capacity(self.text.len());
        for (i, ch) in self.text.char_indices() {
            if ch != ' ' && self.covered[i] {
                out.extend(std::iter::repeat_n('_', ch.len_utf8()));
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// Fraction of non-space characters explained.
    pub fn ratio(&self) -> f32 {
        let mut total = 0usize;
        let mut covered = 0usize;
        for (i, b) in self.text.bytes().enumerate() {
            if b == b' ' {
                continue;
            }
            total += 1;
            if self.covered[i] {
                covered += 1;
            }
        }
        if total == 0 {
            1.0
        } else {
            covered as f32 / total as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_acronyms_punctuation_and_niqqud() {
        assert_eq!(normalize("  צריך מונית לנתב\"ג!  "), "צריך מונית לנתבג");
        assert_eq!(normalize("שָׁלוֹם, מה?"), "שלום מה");
        assert_eq!(normalize("ז׳בוטינסקי-פינת רבי עקיבא"), "ז'בוטינסקי פינת רבי עקיבא");
        assert_eq!(normalize("08:30"), "08:30");
    }

    #[test]
    fn phrase_allows_hebrew_prefixes_and_whole_words() {
        let p = Phrase::new("מונית", true).unwrap();
        assert!(p.find("צריך מונית עכשיו").is_some());
        assert!(p.find("צריך למונית").is_some());
        assert!(p.find("מוניתון").is_none());
        let exact = Phrase::new("מה", false).unwrap();
        assert!(exact.find("למה").is_none());
    }

    #[test]
    fn phrase_set_prefers_longest() {
        let set = PhraseSet::new(&["לא".to_string(), "לא הבנתי".to_string()], false).unwrap();
        let (p, _) = set.find("לא הבנתי אותך").unwrap();
        assert_eq!(p.text, "לא הבנתי");
    }

    #[test]
    fn coverage_counts_explained_characters() {
        let text = normalize("צריך מונית עכשיו");
        let mut c = Coverage::new(&text);
        let set = PhraseSet::new(&["צריך".to_string(), "מונית".to_string()], true).unwrap();
        c.mark_words(&set);
        assert!(c.ratio() > 0.5 && c.ratio() < 0.8);
    }
}
