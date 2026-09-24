//! Preparing text for a TTS engine: pronunciations, then Hebrew spoken-form normalization.
//!
//! Numbers are the classic failure on a phone line ("82₪" read as a date, a phone number
//! read as a quantity), so every digit sequence leaves here as words, with the context
//! that decides how it is said: money, percentage, time, date, phone number, or a plain
//! (feminine, counting-form) number such as a house number.

use std::sync::OnceLock;

use regex::{Captures, Regex};

use crate::config::Gender;
use crate::hebrew::{clock_words, count_phrase, number_words};

const MONTHS: [&str; 12] =
    ["ינואר", "פברואר", "מרץ", "אפריל", "מאי", "יוני", "יולי", "אוגוסט", "ספטמבר", "אוקטובר", "נובמבר", "דצמבר"];

struct Patterns {
    money_before: Regex,
    money_after: Regex,
    percent: Regex,
    phone: Regex,
    time: Regex,
    date: Regex,
    identifier: Regex,
    number: Regex,
    spaces: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("static regex");
        Patterns {
            money_before: re(r"₪\s*(\d{1,6})(?:[.,](\d{1,2}))?"),
            money_after: re(r"(\d{1,6})(?:[.,](\d{1,2}))?\s*(?:₪|ש״ח|ש\x22ח|שח\b)"),
            percent: re(r"(\d{1,3})\s*%"),
            // Israeli numbers: 05X-XXXXXXX, 0X-XXXXXXX, 07X, 1-800, +972.
            // `\b` is useless next to Hebrew letters (they are word characters too), so
            // each pattern carries its own non-digit boundary in `pre` and `(?:\D|$)`.
            phone: re(r"(?P<pre>^|[^\d+])(?P<num>(?:\+972[\s-]?|0)(?:5\d|7\d|[2-489])(?:[\s-]?\d){7}|1[\s-]?[78]00[\s-]?\d{2,3}[\s-]?\d{3,4})(?P<post>\D|$)"),
            time: re(r"(?P<pre>^|\D)([01]?\d|2[0-3]):([0-5]\d)(?P<post>\D|$)"),
            date: re(r"(?P<pre>^|\D)([0-3]?\d)[/.]([01]?\d)(?:[/.](\d{2,4}))?(?P<post>[^\d/.]|$)"),
            // An identifier after a word that announces one ("מספר ההזמנה הוא AB-20491",
            // "תיק השירות 2026-0919-44") is read character by character, never as a quantity.
            identifier: re(r"(?P<lead>(?:ה?הזמנה|ה?מעקב|ה?תיק(?: ה?שירות)?|ה?אסמכתא|ה?אישור|ה?אימות|קוד|order|tracking)(?:\s+(?:שלך|שלכם|מספר|הוא|היא|הם|#|no\.?|number))*\s*[:#]?\s*)(?P<id>[A-Za-z]{0,4}-?[0-9][A-Za-z0-9-]{3,})"),
            number: re(r"\d+"),
            spaces: re(r"\s+"),
        }
    })
}

fn digits_spelled(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_digit)
        .map(|c| number_words(i64::from(c.to_digit(10).unwrap_or(0)), Gender::Feminine))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Letters and digits one by one; separators become pauses.
fn spell_identifier(id: &str) -> String {
    id.chars()
        .filter_map(|c| match c {
            '0'..='9' => Some(number_words(i64::from(c.to_digit(10).unwrap_or(0)), Gender::Feminine)),
            c if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase().to_string()),
            '-' => Some(",".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ,", ",")
}

fn money(whole: &str, cents: Option<&str>) -> String {
    let n: i64 = whole.parse().unwrap_or(0);
    let mut out = count_phrase(n, Gender::Masculine, "שקל אחד", "שקלים");
    if let Some(c) = cents.and_then(|c| c.parse::<i64>().ok()).filter(|c| *c > 0) {
        out.push_str(&format!(" ו{}", count_phrase(c, Gender::Feminine, "אגורה אחת", "אגורות")));
    }
    out
}

/// A business pronunciation dictionary, compiled once: whole words (case-insensitive for
/// Latin script) replaced by their spoken form, longest entries first.
#[derive(Debug, Default)]
pub struct Pronouncer {
    rules: Vec<(Regex, String)>,
}

impl Pronouncer {
    pub fn new<'a>(dictionary: impl IntoIterator<Item = (&'a String, &'a String)>) -> Result<Self, regex::Error> {
        let mut entries: Vec<_> = dictionary.into_iter().collect();
        entries.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
        let rules = entries
            .into_iter()
            .map(|(word, spoken)| {
                let pattern = format!(r"(?i)(^|[^\p{{L}}\p{{N}}]){}($|[^\p{{L}}\p{{N}}])", regex::escape(word));
                Regex::new(&pattern).map(|re| (re, spoken.clone()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { rules })
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (re, spoken) in &self.rules {
            out = re.replace_all(&out, |c: &Captures<'_>| format!("{}{}{}", &c[1], spoken, &c[2])).into_owned();
        }
        out
    }
}

/// Hebrew spoken-form normalization. Leaves no digits behind.
pub fn normalize_hebrew(text: &str) -> String {
    let p = patterns();
    let mut s = text.to_string();
    s = p.money_before.replace_all(&s, |c: &Captures<'_>| money(&c[1], c.get(2).map(|m| m.as_str()))).into_owned();
    s = p.money_after.replace_all(&s, |c: &Captures<'_>| money(&c[1], c.get(2).map(|m| m.as_str()))).into_owned();
    s = p
        .percent
        .replace_all(&s, |c: &Captures<'_>| {
            let n: i64 = c[1].parse().unwrap_or(0);
            format!("{} אחוז", number_words(n, Gender::Masculine))
        })
        .into_owned();
    s = p
        .identifier
        .replace_all(&s, |c: &Captures<'_>| format!("{}{}", &c["lead"], spell_identifier(&c["id"])))
        .into_owned();
    s = p
        .phone
        .replace_all(&s, |c: &Captures<'_>| format!("{}{}{}", &c["pre"], digits_spelled(&c["num"]), &c["post"]))
        .into_owned();
    s = p
        .time
        .replace_all(&s, |c: &Captures<'_>| {
            format!("{}{}{}", &c["pre"], clock_words(c[2].parse().unwrap_or(0), c[3].parse().unwrap_or(0)), &c["post"])
        })
        .into_owned();
    s = p
        .date
        .replace_all(&s, |c: &Captures<'_>| {
            let day: i64 = c[2].parse().unwrap_or(0);
            let month: usize = c[3].parse().unwrap_or(0);
            if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
                return c[0].to_string();
            }
            let mut out = format!("{}{} ב{}", &c["pre"], number_words(day, Gender::Masculine), MONTHS[month - 1]);
            if let Some(y) = c.get(4).and_then(|y| y.as_str().parse::<i64>().ok()) {
                let year = if y < 100 { 2000 + y } else { y };
                out.push_str(&format!(" {}", number_words(year, Gender::Feminine)));
            }
            out.push_str(&c["post"]);
            out
        })
        .into_owned();
    s = p
        .number
        .replace_all(&s, |c: &Captures<'_>| match c[0].parse::<i64>() {
            Ok(n) if (c[0].len() <= 4 && !c[0].starts_with('0')) || &c[0] == "0" => number_words(n, Gender::Feminine),
            _ => digits_spelled(&c[0]),
        })
        .into_owned();
    p.spaces.replace_all(s.trim(), " ").into_owned()
}

/// Everything a TTS request needs done to its text.
pub fn prepare_for_tts(text: &str, language: &str, pronouncer: &Pronouncer) -> String {
    let text = pronouncer.apply(text);
    if language.starts_with("he") {
        normalize_hebrew(&text)
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn money_percent_and_numbers() {
        assert_eq!(normalize_hebrew("זה יעלה 82₪"), "זה יעלה שמונים ושניים שקלים");
        assert_eq!(normalize_hebrew("₪1"), "שקל אחד");
        assert_eq!(normalize_hebrew("הנחה של 10%"), "הנחה של עשרה אחוז");
        assert_eq!(normalize_hebrew("רבי עקיבא 12"), "רבי עקיבא שתים עשרה");
    }

    #[test]
    fn phones_times_dates() {
        assert_eq!(normalize_hebrew("03-1234567"), "אפס שלוש אחת שתיים שלוש ארבע חמש שש שבע");
        assert_eq!(normalize_hebrew("ב-08:30"), "ב-שמונה וחצי");
        assert_eq!(normalize_hebrew("ב15/10"), "בחמישה עשר באוקטובר");
    }

    #[test]
    fn identifiers_are_spelled() {
        assert_eq!(normalize_hebrew("מספר ההזמנה הוא AB-20491."), "מספר ההזמנה הוא A B, שתיים אפס ארבע תשע אחת.");
        assert!(!normalize_hebrew("תיק השירות שלך הוא 2026-0919-44.").contains("אלפיים"));
    }

    /// The legacy evaluation corpus: no digit may reach the TTS engine.
    #[test]
    fn evaluation_corpus_leaves_no_digits() {
        let corpus: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_str(include_str!("../../../evaluation/hebrew-utterances.json")).unwrap();
        for (category, utterances) in corpus {
            for u in utterances {
                let out = normalize_hebrew(&u);
                assert!(!out.chars().any(|c| c.is_ascii_digit()), "{category}: {u} -> {out}");
            }
        }
        assert_eq!(
            normalize_hebrew("הסכום לתשלום הוא ₪249.90."),
            "הסכום לתשלום הוא מאתיים ארבעים ותשעה שקלים ותשעים אגורות."
        );
        assert_eq!(
            normalize_hebrew("מספר הטלפון הוא 050-123-4567."),
            "מספר הטלפון הוא אפס חמש אפס אחת שתיים שלוש ארבע חמש שש שבע."
        );
    }

    #[test]
    fn pronunciation_dictionary() {
        let mut d = BTreeMap::new();
        d.insert("Waze".to_string(), "וֵייז".to_string());
        d.insert("SMS".to_string(), "אס אם אס".to_string());
        assert_eq!(Pronouncer::new(&d).unwrap().apply("שלחתי SMS עם קישור ל-waze"), "שלחתי אס אם אס עם קישור ל-וֵייז");
    }
}
