//! Time slot values: "now", "in ten minutes", "at 8:30", "tomorrow at seven".

use serde::{Deserialize, Serialize};

use crate::config::Gender;
use crate::hebrew::{clock_words, count_phrase, find_numbers};
use crate::text::{normalize, tokens, PhraseSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeSpec {
    Now,
    InMinutes { minutes: u32 },
    At { hour: u32, minute: u32, day_offset: u32 },
}

impl TimeSpec {
    /// Spoken form for read-backs.
    pub fn spoken(&self) -> String {
        match self {
            TimeSpec::Now => "עכשיו".into(),
            TimeSpec::InMinutes { minutes: 30 } => "בעוד חצי שעה".into(),
            TimeSpec::InMinutes { minutes: 15 } => "בעוד רבע שעה".into(),
            TimeSpec::InMinutes { minutes: 60 } => "בעוד שעה".into(),
            TimeSpec::InMinutes { minutes } => {
                format!("בעוד {}", count_phrase(*minutes as i64, Gender::Feminine, "דקה", "דקות"))
            }
            TimeSpec::At { hour, minute, day_offset } => {
                let day = match day_offset {
                    0 => "",
                    1 => "מחר ",
                    _ => "בעוד כמה ימים ",
                };
                format!("{day}ב{}", clock_words(*hour, *minute))
            }
        }
    }

    /// Canonical machine form sent to actions.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Parse a time expression out of an utterance. `now` holds the business's "now" phrases.
pub fn parse_time(utterance: &str, now: &PhraseSet) -> Option<(TimeSpec, f32)> {
    parse_time_span(&normalize(utterance), now).map(|(t, c, _)| (t, c))
}

/// Like [`parse_time`] on already-normalized text, also returning the byte span used.
pub fn parse_time_span(text: &str, now: &PhraseSet) -> Option<(TimeSpec, f32, (usize, usize))> {
    if let Some((_, span)) = now.find(text) {
        return Some((TimeSpec::Now, 0.95, span));
    }
    let toks = tokens(text);
    let mut offsets = Vec::with_capacity(toks.len());
    let mut at = 0;
    for t in &toks {
        offsets.push(at);
        at += t.len() + 1;
    }
    let span = |a: usize, b: usize| -> (usize, usize) {
        let b = b.min(toks.len()).max(a + 1);
        (offsets[a], offsets[b - 1] + toks[b - 1].len())
    };
    let r = parse_tokens(&toks)?;
    Some((r.0, r.1, span(r.2 .0, r.2 .1)))
}

/// Returns the spec, confidence and token range `[start, end)`.
fn parse_tokens(toks: &[&str]) -> Option<(TimeSpec, f32, (usize, usize))> {
    let day_offset = u32::from(toks.iter().any(|t| *t == "מחר" || *t == "tomorrow"));
    let toks = toks.to_vec();

    // Clock digits: "08:30", "8:30", "ב8:30".
    for (i, t) in toks.iter().enumerate() {
        let t = t.trim_start_matches(|c| "בל".contains(c));
        if let Some((h, m)) = t.split_once(':') {
            if let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>()) {
                if h < 24 && m < 60 {
                    let start = if i > 0 && toks[i - 1] == "בשעה" { i - 1 } else { i };
                    return Some((TimeSpec::At { hour: h, minute: m, day_offset }, 0.9, (start, i + 1)));
                }
            }
        }
    }

    // "בעוד ..." — relative.
    if let Some(pos) = toks.iter().position(|t| *t == "בעוד" || *t == "עוד") {
        let rest = &toks[pos + 1..];
        let joined = rest.join(" ");
        for (phrase, minutes, words) in
            [("חצי שעה", 30, 2), ("רבע שעה", 15, 2), ("שעה וחצי", 90, 2), ("שעה", 60, 1), ("דקה", 1, 1)]
        {
            if joined.starts_with(phrase) {
                return Some((TimeSpec::InMinutes { minutes }, 0.9, (pos, pos + 1 + words)));
            }
        }
        if let Some(n) = find_numbers(rest).first() {
            if n.start == 0 {
                let unit = rest.get(n.end).copied().unwrap_or("");
                let minutes = if unit.starts_with("שעות") { n.value * 60 } else { n.value };
                let unit_words =
                    usize::from(unit.starts_with("שעות") || unit.starts_with("דקות") || unit.starts_with("דקה"));
                if (1..=24 * 60).contains(&minutes) {
                    return Some((
                        TimeSpec::InMinutes { minutes: minutes as u32 },
                        0.85,
                        (pos, pos + 1 + n.end + unit_words),
                    ));
                }
            }
        }
    }

    // "בשמונה וחצי", "בשעה שבע", "בשבע ורבע".
    for (i, t) in toks.iter().enumerate() {
        let (word, explicit) = if *t == "בשעה" {
            match toks.get(i + 1) {
                Some(next) => (*next, true),
                None => continue,
            }
        } else if let Some(stripped) = t.strip_prefix('ב') {
            (stripped, false)
        } else {
            continue;
        };
        let after_index = if explicit { i + 2 } else { i + 1 };
        let mut probe: Vec<&str> = vec![word];
        probe.extend(toks.iter().skip(after_index).take(3));
        let Some(n) = find_numbers(&probe).first().copied() else { continue };
        let plausible_hour = (1..=12).contains(&n.value) || (explicit && (0..24).contains(&n.value));
        if n.start != 0 || !plausible_hour {
            continue;
        }
        let tail = probe.get(n.end).copied().unwrap_or("");
        let minute = match tail {
            "וחצי" => 30,
            "ורבע" => 15,
            _ => 0,
        };
        // Bare "בשמונה" is ambiguous with non-time uses; only accept it with a minute word
        // or the explicit "בשעה".
        if !explicit && minute == 0 {
            continue;
        }
        let used = if explicit { 1 } else { 0 } + n.end + usize::from(minute != 0);
        return Some((
            TimeSpec::At { hour: n.value as u32, minute, day_offset },
            if explicit { 0.85 } else { 0.75 },
            (i, i + used),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> PhraseSet {
        PhraseSet::new(&["עכשיו".to_string(), "כמה שיותר מהר".to_string(), "מיד".to_string()], true).unwrap()
    }

    #[test]
    fn parses_common_expressions() {
        assert_eq!(parse_time("צריך מונית עכשיו", &now()).unwrap().0, TimeSpec::Now);
        assert_eq!(parse_time("בעוד עשר דקות", &now()).unwrap().0, TimeSpec::InMinutes { minutes: 10 });
        assert_eq!(parse_time("בעוד חצי שעה", &now()).unwrap().0, TimeSpec::InMinutes { minutes: 30 });
        assert_eq!(parse_time("מחר ב-08:30", &now()).unwrap().0, TimeSpec::At { hour: 8, minute: 30, day_offset: 1 });
        assert_eq!(parse_time("בשמונה וחצי", &now()).unwrap().0, TimeSpec::At { hour: 8, minute: 30, day_offset: 0 });
        assert_eq!(parse_time("לרבי עקיבא", &now()), None);
    }

    #[test]
    fn spoken_forms() {
        assert_eq!(TimeSpec::InMinutes { minutes: 10 }.spoken(), "בעוד עשר דקות");
        assert_eq!(TimeSpec::At { hour: 8, minute: 30, day_offset: 1 }.spoken(), "מחר בשמונה וחצי");
    }
}
