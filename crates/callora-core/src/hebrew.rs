//! Hebrew numbers: reading them out of transcripts, and saying them with the right gender.

use crate::config::Gender;

const UNITS_M: [&str; 10] = ["אפס", "אחד", "שניים", "שלושה", "ארבעה", "חמישה", "שישה", "שבעה", "שמונה", "תשעה"];
const UNITS_F: [&str; 10] = ["אפס", "אחת", "שתיים", "שלוש", "ארבע", "חמש", "שש", "שבע", "שמונה", "תשע"];
const TEENS_M: [&str; 10] = [
    "עשרה",
    "אחד עשר",
    "שנים עשר",
    "שלושה עשר",
    "ארבעה עשר",
    "חמישה עשר",
    "שישה עשר",
    "שבעה עשר",
    "שמונה עשר",
    "תשעה עשר",
];
const TEENS_F: [&str; 10] = [
    "עשר",
    "אחת עשרה",
    "שתים עשרה",
    "שלוש עשרה",
    "ארבע עשרה",
    "חמש עשרה",
    "שש עשרה",
    "שבע עשרה",
    "שמונה עשרה",
    "תשע עשרה",
];
const TENS: [&str; 10] = ["", "", "עשרים", "שלושים", "ארבעים", "חמישים", "שישים", "שבעים", "שמונים", "תשעים"];
const HUNDREDS: [&str; 10] =
    ["", "מאה", "מאתיים", "שלוש מאות", "ארבע מאות", "חמש מאות", "שש מאות", "שבע מאות", "שמונה מאות", "תשע מאות"];
const THOUSANDS: [&str; 10] = [
    "",
    "אלף",
    "אלפיים",
    "שלושת אלפים",
    "ארבעת אלפים",
    "חמשת אלפים",
    "ששת אלפים",
    "שבעת אלפים",
    "שמונת אלפים",
    "תשעת אלפים",
];

/// A number in words, 0..=9999. Larger numbers fall back to digits.
pub fn number_words(n: i64, gender: Gender) -> String {
    if !(0..=9999).contains(&n) {
        return n.to_string();
    }
    if n == 0 {
        return "אפס".into();
    }
    let (units, teens) = match gender {
        Gender::Masculine => (&UNITS_M, &TEENS_M),
        Gender::Feminine => (&UNITS_F, &TEENS_F),
    };
    let n = n as usize;
    let mut parts: Vec<String> = Vec::new();
    if n >= 1000 {
        parts.push(THOUSANDS[n / 1000].into());
    }
    let rest = n % 1000;
    if rest >= 100 {
        parts.push(HUNDREDS[rest / 100].into());
    }
    let below = rest % 100;
    if below >= 20 {
        parts.push(TENS[below / 10].into());
        if below % 10 != 0 {
            parts.push(units[below % 10].into());
        }
    } else if below >= 10 {
        parts.push(teens[below - 10].into());
    } else if below > 0 {
        parts.push(units[below].into());
    }
    if parts.len() > 1 {
        let last = parts.len() - 1;
        parts[last] = format!("ו{}", parts[last]);
    }
    parts.join(" ")
}

/// A counted noun: 1 → the singular phrase ("נוסע אחד", "דקה"), 2 → construct form ("שני
/// נוסעים", "שתי דקות"), otherwise "<number> <plural>".
pub fn count_phrase(n: i64, gender: Gender, singular: &str, plural: &str) -> String {
    match n {
        1 => singular.to_string(),
        2 => match gender {
            Gender::Masculine => format!("שני {plural}"),
            Gender::Feminine => format!("שתי {plural}"),
        },
        _ => format!("{} {plural}", number_words(n, gender)),
    }
}

/// A clock time as said in Israel: 12-hour, feminine hour, "וחצי"/"ורבע".
pub fn clock_words(hour: u32, minute: u32) -> String {
    let h12 = match hour % 12 {
        0 => 12,
        h => h,
    };
    let hour_words = number_words(h12 as i64, Gender::Feminine);
    match minute {
        0 => hour_words,
        15 => format!("{hour_words} ורבע"),
        30 => format!("{hour_words} וחצי"),
        m => format!("{hour_words} ו{}", number_words(m as i64, Gender::Feminine)),
    }
}

fn unit_value(word: &str) -> Option<i64> {
    Some(match word {
        "אחד" | "אחת" => 1,
        "שניים" | "שתיים" | "שני" | "שתי" | "שתים" | "שנים" | "שנַיִם" => 2,
        "שלוש" | "שלושה" | "שלושת" | "שלש" => 3,
        "ארבע" | "ארבעה" | "ארבעת" => 4,
        "חמש" | "חמישה" | "חמשת" | "חמשה" => 5,
        "שש" | "שישה" | "ששת" | "ששה" => 6,
        "שבע" | "שבעה" | "שבעת" => 7,
        "שמונה" | "שמונת" => 8,
        "תשע" | "תשעה" | "תשעת" => 9,
        _ => return None,
    })
}

fn tens_value(word: &str) -> Option<i64> {
    TENS.iter().position(|t| !t.is_empty() && *t == word).map(|i| i as i64 * 10)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    Unit(i64),
    Ten,
    Tens(i64),
    Hundred(i64),
    Hundreds,
    Digits(i64),
}

fn piece(word: &str) -> Option<Piece> {
    if let Ok(v) = word.parse::<i64>() {
        return Some(Piece::Digits(v));
    }
    if let Some(u) = unit_value(word) {
        return Some(Piece::Unit(u));
    }
    if matches!(word, "עשר" | "עשרה" | "עשרת") {
        return Some(Piece::Ten);
    }
    if let Some(t) = tens_value(word) {
        return Some(Piece::Tens(t));
    }
    match word {
        "מאה" => Some(Piece::Hundred(100)),
        "מאתיים" => Some(Piece::Hundred(200)),
        "מאות" => Some(Piece::Hundreds),
        _ => None,
    }
}

/// A number found in a token list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundNumber {
    pub value: i64,
    /// Token index range `[start, end)`.
    pub start: usize,
    pub end: usize,
}

/// Every number in `tokens`, written in digits or in words ("עשרים ושלוש", "שתים עשרה").
pub fn find_numbers(tokens: &[&str]) -> Vec<FoundNumber> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let Some(first) = piece(tokens[i]) else {
            i += 1;
            continue;
        };
        if let Piece::Digits(v) = first {
            found.push(FoundNumber { value: v, start: i, end: i + 1 });
            i += 1;
            continue;
        }
        if first == Piece::Hundreds {
            i += 1;
            continue;
        }
        let start = i;
        let mut total = 0i64;
        let mut current = 0i64;
        let mut j = i;
        while j < tokens.len() {
            let tok = tokens[j];
            // A leading ו joins the parts of a compound number ("עשרים ושלוש").
            let p = if j > start { piece(tok).or_else(|| tok.strip_prefix('ו').and_then(piece)) } else { piece(tok) };
            match p {
                Some(Piece::Unit(u)) if current % 10 == 0 => current += u,
                Some(Piece::Ten) if (1..=9).contains(&(current % 100)) && current % 100 < 10 => current += 10,
                Some(Piece::Ten) if current == 0 => current = 10,
                Some(Piece::Tens(t)) if current % 100 == 0 => current += t,
                Some(Piece::Hundred(h)) if current == 0 => current = h,
                Some(Piece::Hundreds) if (1..=9).contains(&current) => {
                    total += current * 100;
                    current = 0;
                }
                _ => break,
            }
            j += 1;
        }
        if j == start {
            i += 1;
            continue;
        }
        found.push(FoundNumber { value: total + current, start, end: j });
        i = j;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Vec<i64> {
        let toks: Vec<&str> = s.split(' ').collect();
        find_numbers(&toks).into_iter().map(|n| n.value).collect()
    }

    #[test]
    fn words_to_numbers() {
        assert_eq!(parse("אנחנו ארבעה עם שתי מזוודות"), vec![4, 2]);
        assert_eq!(parse("עשרים ושלוש"), vec![23]);
        assert_eq!(parse("שתים עשרה"), vec![12]);
        assert_eq!(parse("רבי עקיבא 12"), vec![12]);
        assert_eq!(parse("מאה עשרים וחמש"), vec![125]);
        assert_eq!(parse("שלוש מאות"), vec![300]);
        assert_eq!(parse("אין פה מספרים"), Vec::<i64>::new());
    }

    #[test]
    fn numbers_to_words_with_gender() {
        assert_eq!(number_words(4, Gender::Masculine), "ארבעה");
        assert_eq!(number_words(4, Gender::Feminine), "ארבע");
        assert_eq!(number_words(21, Gender::Feminine), "עשרים ואחת");
        assert_eq!(number_words(12, Gender::Masculine), "שנים עשר");
        assert_eq!(number_words(82, Gender::Masculine), "שמונים ושניים");
        assert_eq!(number_words(125, Gender::Masculine), "מאה עשרים וחמישה");
        assert_eq!(number_words(2000, Gender::Masculine), "אלפיים");
    }

    #[test]
    fn counted_nouns() {
        assert_eq!(count_phrase(1, Gender::Feminine, "דקה", "דקות"), "דקה");
        assert_eq!(count_phrase(2, Gender::Feminine, "דקה", "דקות"), "שתי דקות");
        assert_eq!(count_phrase(4, Gender::Feminine, "דקה", "דקות"), "ארבע דקות");
        assert_eq!(count_phrase(4, Gender::Masculine, "נוסע אחד", "נוסעים"), "ארבעה נוסעים");
        assert_eq!(count_phrase(2, Gender::Masculine, "נוסע אחד", "נוסעים"), "שני נוסעים");
    }

    #[test]
    fn clock() {
        assert_eq!(clock_words(8, 30), "שמונה וחצי");
        assert_eq!(clock_words(20, 15), "שמונה ורבע");
        assert_eq!(clock_words(0, 0), "שתים עשרה");
        assert_eq!(clock_words(7, 40), "שבע וארבעים");
    }
}
