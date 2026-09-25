//! Israel's localities and streets, from the Population and Immigration Authority's official
//! list (data.gov.il, "רשימת רחובות בישראל - קובץ עם סינונימים": 1,318 localities, ~152,000
//! street names with their alternative spellings).
//!
//! Speech recognition garbles place names ("רמת גאן", "זבוטינסקי", "מיל״ד" for "מאלעד"), and
//! neither the recognizer (50 hint words) nor the agent's prompt (tokens per minute) can hold a
//! whole country's streets. So places are checked here, after the agent: a known locality and
//! street is stored in its official spelling, a near miss is corrected, and anything not found
//! comes back to the agent with the closest names, so it asks instead of guessing.
//!
//! Pure data and string matching: loading the file is the binary's job ([`Gazetteer::from_tsv`]).

use std::collections::HashMap;

/// A place found in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// The locality as the caller said it ("תל אביב"), for speech.
    pub city_said: String,
    /// The locality's official name ("תל אביב - יפו"), for dispatch.
    pub city: String,
    /// The street's official name, when a street was given.
    pub street: Option<String>,
    pub number: Option<String>,
}

impl Address {
    /// "ז'בוטינסקי 5, רמת גן" — how the place is read back.
    pub fn spoken(&self) -> String {
        self.format(&self.city_said)
    }

    /// "ז'בוטינסקי 5, רמת גן" with the official locality name — what dispatch receives.
    pub fn official(&self) -> String {
        self.format(&self.city)
    }

    fn format(&self, city: &str) -> String {
        match (&self.street, &self.number) {
            (Some(s), Some(n)) => format!("{s} {n}, {city}"),
            (Some(s), None) => format!("{s}, {city}"),
            (None, _) => city.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(Address),
    /// No locality in the text. `closest` are localities that sound near a word of it.
    NoCity {
        closest: Vec<String>,
    },
    /// The locality exists, the street is not one of its streets.
    NoStreet {
        city: String,
        heard: String,
        closest: Vec<String>,
    },
}

#[derive(Debug)]
struct City {
    name: String,
    streets: Vec<String>,
    /// Normalized street name (official or alternative) → index in `streets`.
    street_keys: HashMap<String, usize>,
}

#[derive(Debug, Default)]
pub struct Gazetteer {
    cities: Vec<City>,
    /// Normalized locality name or alias → (city index, the alias as written).
    city_keys: HashMap<String, (usize, String)>,
}

/// Letters and digits only, final letters folded, single spaces: "ז'בוטינסקי" and
/// "זבוטינסקי", "תל-אביב" and "תל אביב" become the same key.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = match c {
            'ך' => 'כ',
            'ם' => 'מ',
            'ן' => 'נ',
            'ף' => 'פ',
            'ץ' => 'צ',
            c => c,
        };
        if c.is_alphanumeric() {
            out.push(c);
        } else if (c.is_whitespace() || c == '-' || c == '־' || c == ',') && !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    out.trim_end().to_string()
}

/// Single-letter Hebrew prefixes a place can carry in speech: מתל אביב, לרמת גן, בבאר שבע.
fn strip_prefix(word: &str) -> Option<&str> {
    let first = word.chars().next()?;
    if matches!(first, 'מ' | 'ל' | 'ב' | 'ו' | 'ה') && word.chars().count() > 2 {
        Some(&word[first.len_utf8()..])
    } else {
        None
    }
}

fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The consonants a word keeps when spoken fast: without the letters speech recognition
/// drops or swaps (א ע ה ו י), and with the sound-alike pairs folded (כ/ק, ט/ת, ס/ש).
fn sound(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, 'א' | 'ע' | 'ה' | 'ו' | 'י' | ' '))
        .map(|c| match c {
            'כ' => 'ק',
            'ט' => 'ת',
            'ש' => 'ס',
            c => c,
        })
        .collect()
}

/// Words of a place the caller never said, not even garbled: the agent made them up. A live
/// call booked "רחוב אהרונוביץ' 42" when the caller had said "אההה, 42": the street was real,
/// so nothing else caught it. A word counts as heard when it is close to a word the caller
/// said, or to a stretch of their speech with the spaces removed (recognition splits names:
/// "בני ינאי אומה" is "בנייני האומה").
pub fn unheard_words(value: &str, heard: &str) -> Vec<String> {
    let heard = norm(heard);
    let heard_words: Vec<&str> = heard.split(' ').filter(|w| !w.is_empty()).collect();
    let compact: Vec<char> = heard.chars().filter(|c| *c != ' ').collect();
    let near = |w: &str| {
        let len = w.chars().count();
        let limit = len.div_ceil(3).max(1);
        if heard_words.iter().any(|h| distance(w, h) <= limit) {
            return true;
        }
        for size in len.saturating_sub(1).max(1)..=len + 1 {
            for start in 0..compact.len().saturating_sub(size - 1) {
                let piece: String = compact[start..start + size].iter().collect();
                if distance(w, &piece) <= limit {
                    return true;
                }
            }
        }
        false
    };
    norm(value)
        .split(' ')
        .filter(|w| w.chars().count() >= 2 && !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !matches!(*w, "רחוב" | "רח" | "שדרות" | "שד" | "דרכ" | "סמטת" | "כיכר"))
        .filter(|w| !near(w) && !strip_prefix(w).is_some_and(near))
        .map(str::to_string)
        .collect()
}

/// A misspelling small enough to correct without asking.
fn close_enough(heard: &str, key: &str) -> bool {
    let len = key.chars().count();
    len >= 4 && distance(heard, key) <= if len >= 8 { 2 } else { 1 }
}

impl Gazetteer {
    /// Rows of `city_code, city_name, street_code, street_name, official|synonym`, tab
    /// separated; `#` lines are comments.
    pub fn from_tsv(text: &str) -> Self {
        let mut g = Gazetteer::default();
        let mut by_code: HashMap<&str, usize> = HashMap::new();
        let mut street_ids: Vec<HashMap<&str, usize>> = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let cols: Vec<&str> = line.split('\t').collect();
            let [city_code, city_name, street_code, street_name, kind] = cols[..] else { continue };
            let ci = *by_code.entry(city_code).or_insert_with(|| {
                g.cities.push(City { name: city_name.to_string(), streets: Vec::new(), street_keys: HashMap::new() });
                street_ids.push(HashMap::new());
                g.cities.len() - 1
            });
            let city = &mut g.cities[ci];
            let ids = &mut street_ids[ci];
            let si = *ids.entry(street_code).or_insert_with(|| {
                city.streets.push(street_name.to_string());
                city.streets.len() - 1
            });
            if kind == "official" {
                city.streets[si] = street_name.to_string();
            }
            city.street_keys.entry(norm(street_name)).or_insert(si);
        }
        for (ci, city) in g.cities.iter().enumerate() {
            // "תל אביב - יפו" is also "תל אביב" and "יפו"; "אבו גוש" is itself.
            let name = city.name.split('(').next().unwrap_or(&city.name).trim();
            let mut aliases = vec![name.to_string()];
            if name.contains(" - ") {
                aliases.extend(name.split(" - ").map(|p| p.trim().to_string()));
            }
            for alias in aliases {
                let key = norm(&alias);
                if key.chars().count() >= 2 {
                    g.city_keys.entry(key).or_insert((ci, alias));
                }
            }
        }
        g
    }

    pub fn is_empty(&self) -> bool {
        self.cities.is_empty()
    }

    pub fn localities(&self) -> usize {
        self.cities.len()
    }

    /// Find the locality, street and house number in a place as the caller gave it.
    pub fn resolve(&self, text: &str) -> Lookup {
        let words: Vec<String> = norm(text).split(' ').filter(|w| !w.is_empty()).map(str::to_string).collect();
        let candidates = self.find_cities(&words);
        if candidates.is_empty() {
            return Lookup::NoCity { closest: self.closest_cities(&words) };
        }
        // "אלעד בן זכאי 45": both "אלעד" and "בן זכאי" (a moshav) are localities. The reading
        // in which the other words are a real street of that locality is the right one.
        let readings: Vec<Lookup> = candidates.into_iter().map(|c| self.resolve_in(&words, c)).collect();
        let score = |l: &Lookup| match l {
            Lookup::Found(a) if a.street.is_some() => 0,
            Lookup::Found(_) => 1,
            _ => 2,
        };
        let best = readings.iter().enumerate().min_by_key(|(i, l)| (score(l), *i)).map(|(i, _)| i).unwrap_or(0);
        readings.into_iter().nth(best).unwrap_or(Lookup::NoCity { closest: Vec::new() })
    }

    fn resolve_in(&self, words: &[String], (ci, alias, start, len): (usize, String, usize, usize)) -> Lookup {
        let city = &self.cities[ci];
        let rest: Vec<&String> =
            words.iter().enumerate().filter(|(i, _)| *i < start || *i >= start + len).map(|(_, w)| w).collect();
        let number = rest.iter().find(|w| w.chars().all(|c| c.is_ascii_digit())).map(|w| w.to_string());
        let mut street_words: Vec<&str> =
            rest.iter().filter(|w| !w.chars().all(|c| c.is_ascii_digit())).map(|w| w.as_str()).collect();
        if matches!(street_words.first(), Some(&"רחוב") | Some(&"רח")) {
            street_words.remove(0);
        }
        // "מושב בן זכאי", "העיר אלעד": words that say what the locality is, not a street.
        street_words.retain(|w| !matches!(*w, "מושב" | "קיבוצ" | "קבוצ" | "העיר" | "עיר" | "יישוב" | "ישוב"));
        let found = |street: Option<String>| {
            Lookup::Found(Address { city_said: alias.clone(), city: city.name.clone(), street, number: number.clone() })
        };
        if street_words.is_empty() {
            return found(None);
        }
        let heard = street_words.join(" ");
        let mut candidates = vec![heard.clone()];
        if let Some(stripped) = strip_prefix(street_words[0]) {
            candidates
                .push(std::iter::once(stripped).chain(street_words[1..].iter().copied()).collect::<Vec<_>>().join(" "));
        }
        for c in &candidates {
            if let Some(&si) = city.street_keys.get(c.as_str()) {
                return found(Some(city.streets[si].clone()));
            }
        }
        for c in &candidates {
            if let Some((&_, &si)) = city.street_keys.iter().find(|(k, _)| close_enough(c, k)) {
                return found(Some(city.streets[si].clone()));
            }
        }
        let mut near: Vec<(usize, &String)> =
            city.street_keys.iter().map(|(k, &si)| (distance(&candidates[0], k), &city.streets[si])).collect();
        near.sort();
        let mut closest: Vec<String> = Vec::new();
        for (d, s) in near {
            if d > 3 || closest.len() == 3 {
                break;
            }
            if !closest.contains(s) {
                closest.push(s.clone());
            }
        }
        Lookup::NoStreet { city: city.name.clone(), heard: street_words.join(" "), closest }
    }

    /// Every run of words that names a locality exactly (or after a prefix letter), longest
    /// and latest first (callers end with the city); failing that, one unambiguous small
    /// misspelling.
    fn find_cities(&self, words: &[String]) -> Vec<(usize, String, usize, usize)> {
        let mut all = Vec::new();
        for len in (1..=4.min(words.len())).rev() {
            let mut best: Option<(usize, String, usize, usize)> = None;
            for start in (0..=words.len() - len).rev() {
                let phrase = words[start..start + len].join(" ");
                let stripped = strip_prefix(&words[start]).map(|s| {
                    std::iter::once(s)
                        .chain(words[start + 1..start + len].iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                for key in std::iter::once(phrase).chain(stripped) {
                    if let Some((ci, alias)) = self.city_keys.get(&key) {
                        best = Some((*ci, alias.clone(), start, len));
                        all.push((*ci, alias.clone(), start, len));
                    }
                }
            }
            let _ = best;
        }
        if !all.is_empty() {
            return all;
        }
        // A misspelled locality ("רמת גאן"): only unambiguous, small differences.
        for len in (1..=3.min(words.len())).rev() {
            for start in (0..=words.len() - len).rev() {
                let phrase = words[start..start + len].join(" ");
                let stripped = strip_prefix(&words[start]).map(|s| {
                    std::iter::once(s)
                        .chain(words[start + 1..start + len].iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                for key in std::iter::once(phrase).chain(stripped) {
                    let hits: Vec<_> = self.city_keys.iter().filter(|(k, _)| close_enough(&key, k)).collect();
                    if let [(_, (ci, alias))] = hits[..] {
                        return vec![(*ci, alias.clone(), start, len)];
                    }
                }
            }
        }
        Vec::new()
    }

    fn closest_cities(&self, words: &[String]) -> Vec<String> {
        let mut scored: Vec<(usize, &String)> = Vec::new();
        for len in 1..=2.min(words.len()) {
            for start in 0..=words.len() - len {
                let phrase = words[start..start + len].join(" ");
                let stripped = strip_prefix(&words[start]).map(|s| {
                    std::iter::once(s)
                        .chain(words[start + 1..start + len].iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                for key in std::iter::once(phrase).chain(stripped) {
                    if key.chars().count() < 3 {
                        continue;
                    }
                    let sound = sound(&key);
                    for (k, (_, alias)) in &self.city_keys {
                        // Same consonants first ("מיל״ד" and "אלעד" are both ל-ד), then spelling.
                        if !sound.is_empty() && sound == self::sound(k) {
                            scored.push((0, alias));
                            continue;
                        }
                        let d = distance(&key, k);
                        if d <= 2 {
                            scored.push((d + 1, alias));
                        }
                    }
                }
            }
        }
        scored.sort();
        let mut out: Vec<String> = Vec::new();
        for (_, a) in scored {
            if !out.contains(a) {
                out.push(a.clone());
            }
            if out.len() == 3 {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# test\n\
        8600\tרמת גן\t205\tז'בוטינסקי\tofficial\n\
        8600\tרמת גן\t205\tזבוטינסקי\tsynonym\n\
        8600\tרמת גן\t310\tביאליק\tofficial\n\
        5000\tתל אביב - יפו\t101\tדיזנגוף\tofficial\n\
        5000\tתל אביב - יפו\t102\tרוטשילד\tofficial\n\
        5000\tתל אביב - יפו\t102\tשד רוטשילד\tsynonym\n\
        1309\tאלעד\t110\tרבי עקיבא\tofficial\n\
        9000\tבאר שבע\t120\tרגר\tofficial\n";

    fn g() -> Gazetteer {
        Gazetteer::from_tsv(SAMPLE)
    }

    fn found(l: Lookup) -> Address {
        match l {
            Lookup::Found(a) => a,
            other => panic!("expected an address, got {other:?}"),
        }
    }

    #[test]
    fn official_spelling_from_an_alternative_one() {
        let a = found(g().resolve("זבוטינסקי 5 ברמת גן"));
        assert_eq!(a.spoken(), "ז'בוטינסקי 5, רמת גן");
    }

    #[test]
    fn a_locality_alias_is_spoken_as_said_and_sent_in_full() {
        let a = found(g().resolve("דיזנגוף 50, תל אביב"));
        assert_eq!(a.spoken(), "דיזנגוף 50, תל אביב");
        assert_eq!(a.official(), "דיזנגוף 50, תל אביב - יפו");
    }

    #[test]
    fn small_misspellings_are_corrected() {
        assert_eq!(found(g().resolve("מרמת גאן")).spoken(), "רמת גן");
        assert_eq!(found(g().resolve("רוטשילט 3 תל אביב")).street.as_deref(), Some("רוטשילד"));
    }

    #[test]
    fn a_locality_alone_and_with_a_prefix() {
        assert_eq!(found(g().resolve("מאלעד")).city, "אלעד");
        assert_eq!(found(g().resolve("לבאר שבע")).city, "באר שבע");
    }

    #[test]
    fn a_street_named_like_a_locality_is_read_as_a_street() {
        // "בן זכאי" is a moshav and a street of אלעד; the street reading wins.
        let g = Gazetteer::from_tsv(
            "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
             2066\tבן זכאי\t9000\tבן זכאי\tofficial\n",
        );
        assert_eq!(found(g.resolve("אלעד בן זכאי 45")).spoken(), "רבן יוחנן בן זכאי 45, אלעד");
        assert_eq!(found(g.resolve("בן זכאי 45, אלעד")).spoken(), "רבן יוחנן בן זכאי 45, אלעד");
        assert_eq!(found(g.resolve("מושב בן זכאי")).city, "בן זכאי");
    }

    #[test]
    fn words_nobody_said_are_caught() {
        // The live call: "אההה, 42" became "רחוב אהרונוביץ' 42".
        let heard = "אלעד. בן זכר, 45. אה, בני וורק. אההה, 42.";
        assert_eq!(unheard_words("רחוב אהרונוביץ' 42, בני ברק", heard), vec!["אהרונוביצ"]);
        assert!(unheard_words("בן זכאי 45, אלעד", heard).is_empty(), "a garbled word is still heard");
        // Recognition split the name; the agent's knowledge put it together.
        assert!(unheard_words("בנייני האומה, ירושלים", "נוסעים לירושלים. בני ינאי, אומה.").is_empty());
    }

    #[test]
    fn unknown_places_come_back_with_the_closest_names() {
        match g().resolve("מיל״ד") {
            Lookup::NoCity { closest } => assert!(closest.contains(&"אלעד".to_string()), "{closest:?}"),
            other => panic!("{other:?}"),
        }
        match g().resolve("הרצל 99 רמת גן") {
            Lookup::NoStreet { city, closest, .. } => {
                assert_eq!(city, "רמת גן");
                assert!(closest.len() <= 3);
            }
            other => panic!("{other:?}"),
        }
    }
}
