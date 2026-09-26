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
//! Places that are not streets ("בנייני האומה", a mall, a hospital) come from OpenStreetMap
//! (© OpenStreetMap contributors, ODbL): up to 200 per locality, with an address when mapped
//! and a point, so dispatch gets somewhere to drive to ([`Gazetteer::add_places`]).
//!
//! Pure data and string matching: loading the files is the binary's job ([`Gazetteer::from_tsv`]).

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
    /// For a place that is not a street (`street` holds its name): its street address when
    /// mapped, and where it is ("31.78570,35.20160").
    pub place: Option<PlaceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceInfo {
    pub address: Option<String>,
    pub point: String,
}

impl Address {
    /// "ז'בוטינסקי 5, רמת גן" — how the place is read back.
    pub fn spoken(&self) -> String {
        self.format(&self.city_said)
    }

    /// "ז'בוטינסקי 5, רמת גן" with the official locality name — what dispatch receives. For
    /// a place: "בנייני האומה, שדרות שזר 1, ירושלים (31.78570,35.20160)".
    pub fn official(&self) -> String {
        match (&self.place, &self.street) {
            (Some(p), Some(name)) => {
                let at = p.address.as_ref().map(|a| format!(", {a}")).unwrap_or_default();
                format!("{name}{at}, {} ({})", self.city, p.point)
            }
            _ => self.format(&self.city),
        }
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
    /// Each street's shortest name as written ("אהרונוביץ" of "אהרונוביץ ראובן"), for
    /// recognition hints.
    short: Vec<String>,
    places: Vec<Place>,
    /// Normalized place name (or alias) → index in `places`.
    place_keys: HashMap<String, usize>,
}

#[derive(Debug)]
struct Place {
    name: String,
    address: Option<String>,
    point: String,
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

/// A street's consonants as an Ashkenazi speaker's name for it would be written: ת and ס one
/// sound, "טש" the same as "ץ".
fn ashkenazi(s: &str) -> String {
    sound(s).replace('ת', "ס").replace("סס", "צ")
}

/// Words of a place the caller never said, not even garbled: the agent made them up. A live
/// call booked "רחוב אהרונוביץ' 42" when the caller had said "אההה, 42": the street was real,
/// so nothing else caught it. A word counts as heard when it is close to a word the caller
/// said, or to a stretch of their speech with the spaces removed (recognition splits names:
/// "בני ינאי אומה" is "בנייני האומה").
pub fn unheard_words(value: &str, heard: &str) -> Vec<String> {
    // Recognition writes some Hebrew in Latin letters ("בניין ה-Human" for "בנייני האומה").
    let heard = norm(&format!("{heard} {}", hebrew_letters(heard)));
    let heard_words: Vec<&str> = heard.split(' ').filter(|w| !w.is_empty()).collect();
    let compact: Vec<char> = heard.chars().filter(|c| *c != ' ').collect();
    let near = |w: &str| {
        let len = w.chars().count();
        let limit = len.div_ceil(3).max(1);
        if heard_words.iter().any(|h| distance(w, h) <= limit) {
            return true;
        }
        // Across word boundaries almost anything is two letters from something: one here.
        let glued = if len >= 9 { 2 } else { 1 };
        for size in len.saturating_sub(1).max(1)..=len + 1 {
            for start in 0..compact.len().saturating_sub(size - 1) {
                let piece: String = compact[start..start + size].iter().collect();
                if distance(w, &piece) <= glued {
                    return true;
                }
            }
        }
        false
    };
    let value = norm(value);
    let words: Vec<&str> = value
        .split(' ')
        .filter(|w| w.chars().count() >= 2 && !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !matches!(*w, "רחוב" | "רח" | "שדרות" | "שד" | "דרכ" | "סמטת" | "כיכר"))
        .collect();
    let unheard: Vec<bool> = words.iter().map(|w| !near(w) && !strip_prefix(w).is_some_and(near)).collect();
    // A name recognition garbled across words ("בניהול אומה" for "בנייני האומה"): the words
    // miss, the sounds of the whole name match. Each unheard word with its neighbours, one
    // consonant off at most, and only when the name has four consonants or more (three
    // match something in any sentence).
    // The caller's consonants, digits out, and where each word (or the word after a prefix
    // letter) starts: a name matches from the start of a word, not from the middle of one
    // ("זכאי 45 אני נמצאת" holds "קננמ", one off "בננמ").
    let mut heard_sound: Vec<char> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    for w in &heard_words {
        let w: String = w.chars().filter(|c| !c.is_ascii_digit()).collect();
        let s: Vec<char> = sound(&w).chars().collect();
        if s.is_empty() {
            continue;
        }
        starts.push(heard_sound.len());
        if strip_prefix(&w).is_some() && s.len() > 1 {
            starts.push(heard_sound.len() + 1);
        }
        heard_sound.extend(s);
    }
    let sounds_heard = |from: usize, to: usize| {
        let name = sound(&words[from..=to].concat());
        let len = name.chars().count();
        len >= 4
            && starts.iter().any(|&start| {
                (len..=len + 1).any(|size| {
                    let end = (start + size).min(heard_sound.len());
                    let piece: String = heard_sound[start..end].iter().collect();
                    distance(&name, &piece) <= 1
                })
            })
    };
    words
        .iter()
        .enumerate()
        .filter(|(i, _)| unheard[*i])
        .filter(|(i, _)| {
            let (from, to) = (i.saturating_sub(1), (i + 1).min(words.len().saturating_sub(1)));
            !(sounds_heard(*i, to) || sounds_heard(from, *i) || sounds_heard(from, to))
        })
        .map(|(_, w)| w.to_string())
        .collect()
}

/// The Latin words of a text in rough Hebrew letters, vowels a/e dropped: "Human" → "הומנ".
fn hebrew_letters(text: &str) -> String {
    text.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 2)
        .map(|w| {
            w.to_ascii_lowercase()
                .chars()
                .filter_map(|c| {
                    Some(match c {
                        'b' => 'ב',
                        'g' | 'j' => 'ג',
                        'd' => 'ד',
                        'h' => 'ה',
                        'u' | 'o' | 'v' | 'w' => 'ו',
                        'z' => 'ז',
                        't' => 'ט',
                        'i' | 'y' => 'י',
                        'k' | 'c' | 'q' => 'ק',
                        'l' => 'ל',
                        'm' => 'מ',
                        'n' => 'נ',
                        's' | 'x' => 'ס',
                        'p' | 'f' => 'פ',
                        'r' => 'ר',
                        _ => return None,
                    })
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A misspelling small enough to correct without asking.
fn close_enough(heard: &str, key: &str) -> bool {
    // Five letters at least: "חיפה" is one letter from "חיבה" (a spelling of החיד"א in
    // ירושלים) and a live call booked the wrong street.
    let len = key.chars().count();
    len >= 5 && heard.chars().count() >= 5 && distance(heard, key) <= if len >= 8 { 2 } else { 1 }
}

impl Gazetteer {
    /// Places that are not streets, rows of `locality, name, aliases (|-separated), street,
    /// house number, lat, lon`, tab separated; `#` lines are comments. Rows of a locality the
    /// streets list does not have are skipped. Returns how many were added.
    pub fn add_places(&mut self, text: &str) -> usize {
        let mut added = 0;
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let cols: Vec<&str> = line.split('\t').collect();
            let [city, name, aliases, street, number, lat, lon] = cols[..] else { continue };
            let Some(&(ci, _)) = self.city_keys.get(&norm(city)) else { continue };
            let address = match (street.trim(), number.trim()) {
                ("", _) => None,
                (s, "") => Some(s.to_string()),
                (s, n) => Some(format!("{s} {n}")),
            };
            let c = &mut self.cities[ci];
            let pi = c.places.len();
            c.places.push(Place { name: name.to_string(), address, point: format!("{lat},{lon}") });
            for key in std::iter::once(name).chain(aliases.split('|')).map(norm).filter(|k| !k.is_empty()) {
                // A street of the same name wins: "הרצל" is the street, not a school on it.
                if !c.street_keys.contains_key(&key) {
                    c.place_keys.entry(key).or_insert(pi);
                }
            }
            added += 1;
        }
        added
    }

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
                g.cities.push(City {
                    name: city_name.to_string(),
                    streets: Vec::new(),
                    street_keys: HashMap::new(),
                    short: Vec::new(),
                    places: Vec::new(),
                    place_keys: HashMap::new(),
                });
                street_ids.push(HashMap::new());
                g.cities.len() - 1
            });
            let city = &mut g.cities[ci];
            let ids = &mut street_ids[ci];
            let si = *ids.entry(street_code).or_insert_with(|| {
                city.streets.push(street_name.to_string());
                city.short.push(street_name.to_string());
                city.streets.len() - 1
            });
            let len = street_name.chars().count();
            if len >= 4 && !street_name.chars().any(|c| c.is_ascii_digit()) && len < city.short[si].chars().count() {
                city.short[si] = street_name.to_string();
            }
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
        // "ביתר" is ביתר עילית (and not מיתר, one letter off): the short name of an "עילית"
        // locality, when no locality has it as its own name.
        for (ci, city) in g.cities.iter().enumerate() {
            for suffix in [" עילית", " עלית"] {
                if let Some(base) = city.name.strip_suffix(suffix) {
                    let key = norm(base);
                    if key.chars().count() >= 3 {
                        g.city_keys.entry(key).or_insert((ci, city.name.clone()));
                    }
                }
            }
        }
        g
    }

    pub fn is_empty(&self) -> bool {
        self.cities.is_empty()
    }

    /// Street names of a locality to bias speech recognition with, the hardest to guess
    /// first: surnames recognition has no word for ("אהרונוביץ", "ז'בוטינסקי", "רוזנברג"),
    /// then streets with many spellings (the well-known ones).
    pub fn street_keyterms(&self, city: &str, limit: usize) -> Vec<String> {
        let Some(&(ci, _)) = self.city_keys.get(&norm(city)) else { return Vec::new() };
        let c = &self.cities[ci];
        let mut spellings = vec![0usize; c.streets.len()];
        for &si in c.street_keys.values() {
            spellings[si] += 1;
        }
        let foreign = |s: &str| {
            let n = norm(s);
            ["וביצ", "ביצ", "וויצ", "סקי", "צקי", "ברג", "שטיינ", "בוימ", "מאנ", "פלד", "ורג", "הויז"]
                .iter()
                .any(|end| n.split(' ').any(|w| w.ends_with(end)))
        };
        // Not hints: numbered streets, abbreviations ("ש הפומז", "רח 287"), short names.
        let useful = |s: &str| {
            s.chars().count() >= 4
                && !s.chars().any(|c| c.is_ascii_digit())
                && !["ש ", "שכ ", "רח ", "שד "].iter().any(|p| s.starts_with(p))
        };
        let mut ranked: Vec<usize> = (0..c.streets.len()).filter(|&si| useful(&c.short[si])).collect();
        ranked.sort_by_key(|&si| (!foreign(&c.short[si]), std::cmp::Reverse(spellings[si]), c.short[si].clone()));
        ranked.into_iter().map(|si| c.short[si].clone()).take(limit).collect()
    }

    /// Towns and cities (localities with at least `min_streets` streets), for hinting
    /// recognition when the caller is about to say one.
    pub fn town_names(&self, min_streets: usize) -> Vec<String> {
        self.cities.iter().filter(|c| c.streets.len() >= min_streets).map(|c| c.name.clone()).collect()
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
            Lookup::Found(Address {
                city_said: alias.clone(),
                city: city.name.clone(),
                street,
                number: number.clone(),
                place: None,
            })
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
        let place = |pi: usize| {
            let p = &city.places[pi];
            Lookup::Found(Address {
                city_said: alias.clone(),
                city: city.name.clone(),
                street: Some(p.name.clone()),
                number: None,
                place: Some(PlaceInfo { address: p.address.clone(), point: p.point.clone() }),
            })
        };
        // "בנייני האומה": a place of the locality. A house number means a street.
        if number.is_none() {
            for c in &candidates {
                if let Some(&pi) = city.place_keys.get(c.as_str()) {
                    return place(pi);
                }
            }
        }
        for c in &candidates {
            if let Some((&_, &si)) = city.street_keys.iter().find(|(k, _)| close_enough(c, k)) {
                return found(Some(city.streets[si].clone()));
            }
        }
        if number.is_none() {
            for c in &candidates {
                if let Some((&_, &pi)) = city.place_keys.iter().find(|(k, _)| close_enough(c, k)) {
                    return place(pi);
                }
            }
        }
        // Ashkenazi speech ("אהרוינוביטש" for "אהרונוביץ", "שבעס" for "שבת"): the same
        // consonants once ת/ס and טש/ץ are one sound. Only when one street of the city fits.
        let heard_key = ashkenazi(&candidates[0]);
        // Four consonants at least: "שוויצר" and "יוסי הצייר" are both ס-צ-ר.
        if heard_key.chars().count() >= 4 {
            let mut fits = city.street_keys.iter().filter(|(k, _)| ashkenazi(k) == heard_key).map(|(_, &si)| si);
            if let Some(si) = fits.next() {
                if fits.all(|other| other == si) {
                    return found(Some(city.streets[si].clone()));
                }
            }
        }
        // "בן זכאי 45, עדי": עדי has no such street, but אלעד, which recognition turns into
        // "עדי" or "עדו", has. A real street and number pin the city better than the garbled
        // name; the read-back confirms it with the caller.
        if let Some((ci2, si)) = self.sound_alike_with_street(ci, &alias, &candidates) {
            let other = &self.cities[ci2];
            return Lookup::Found(Address {
                city_said: other.name.clone(),
                city: other.name.clone(),
                street: Some(other.streets[si].clone()),
                number,
                place: None,
            });
        }
        let mut near: Vec<(usize, &String)> = city
            .street_keys
            .iter()
            .map(|(k, &si)| (distance(&candidates[0], k), &city.streets[si]))
            .chain(city.place_keys.iter().map(|(k, &pi)| (distance(&candidates[0], k), &city.places[pi].name)))
            .collect();
        near.sort();
        let mut closest: Vec<String> = Vec::new();
        for (d, s) in near {
            // Only names a third off at most: "עונה" is three letters from anything short.
            if d > (candidates[0].chars().count() / 3).max(1) || closest.len() == 3 {
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

    /// The one other locality that sounds like `city_said` (one consonant more or less) and
    /// has this street. `None` when none or several do.
    fn sound_alike_with_street(&self, not: usize, city_said: &str, streets: &[String]) -> Option<(usize, usize)> {
        let said = sound(&norm(city_said));
        if said.is_empty() {
            return None;
        }
        let mut hits: Vec<(usize, usize, usize)> = Vec::new();
        for (key, (ci, _)) in &self.city_keys {
            if *ci == not || hits.iter().any(|h| h.1 == *ci) {
                continue;
            }
            // A consonant dropped or added ("עדו" for "אלעד"), never another one in its place
            // ("יפו", an alias of תל אביב, is not "עדי").
            let other = sound(key);
            let d = distance(&said, &other);
            if d > 1 || (d == 1 && said.chars().count() == other.chars().count()) {
                continue;
            }
            if let Some(&si) = streets.iter().find_map(|s| self.cities[*ci].street_keys.get(s.as_str())) {
                hits.push((d, *ci, si));
            }
        }
        hits.sort();
        match hits.as_slice() {
            [(_, ci, si)] => Some((*ci, *si)),
            [(d0, ci, si), (d1, ..), ..] if d0 < d1 => Some((*ci, *si)),
            _ => None,
        }
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
    fn a_place_of_the_city_is_found_with_its_address_and_point() {
        let mut g = g();
        g.add_places(
            "# test\n\
             אלעד\tבנייני העירייה\tעירייה|העירייה\tרבי עקיבא\t1\t32.05\t34.95\n\
             אלעד\tרבי עקיבא\t\t\t\t32.06\t34.96\n\
             עיר שאינה ברשימה\tמקום\t\t\t\t31\t35\n",
        );
        let a = found(g.resolve("מבנייני העירייה באלעד"));
        assert_eq!(a.spoken(), "בנייני העירייה, אלעד");
        assert_eq!(a.official(), "בנייני העירייה, רבי עקיבא 1, אלעד (32.05,34.95)");
        assert_eq!(found(g.resolve("העירייה, אלעד")).spoken(), "בנייני העירייה, אלעד", "an alias");
        // A street of the same name stays the street.
        assert_eq!(found(g.resolve("רבי עקיבא, אלעד")).place, None);
        assert!(matches!(g.resolve("קניון הזהב, אלעד"), Lookup::NoStreet { .. }));
    }

    #[test]
    fn a_city_s_hard_street_names_come_first_as_recognition_hints() {
        let g = Gazetteer::from_tsv(
            "6100\tבני ברק\t301\tאהרונוביץ ראובן\tofficial\n6100\tבני ברק\t301\tאהרונוביץ\tsynonym\n\
             6100\tבני ברק\t302\tרבי עקיבא\tofficial\n6100\tבני ברק\t303\tז'בוטינסקי\tofficial\n",
        );
        let terms = g.street_keyterms("בני ברק", 2);
        assert_eq!(terms.len(), 2);
        assert!(terms.contains(&"אהרונוביץ".to_string()), "{terms:?}");
        assert!(g.street_keyterms("עיר שאין", 5).is_empty());
    }

    #[test]
    fn the_short_name_of_an_illit_town_is_that_town() {
        let g = Gazetteer::from_tsv(
            "3780	ביתר עילית	1	הרמב\"ן	official
1100	מיתר	2	רימון	official
             1200	מודיעין-מכבים-רעות	3	עמק החולה	official
3797	מודיעין עילית	4	אבני נזר	official
",
        );
        assert_eq!(found(g.resolve("הרמב\"ן 16, ביתר")).official(), "הרמב\"ן 16, ביתר עילית");
        assert_eq!(found(g.resolve("אבני נזר 3, מודיעין עילית")).city, "מודיעין עילית");
    }

    #[test]
    fn an_ashkenazi_pronunciation_finds_the_street() {
        let g = Gazetteer::from_tsv(
            "6100	בני ברק	301	אהרונוביץ ראובן	official
6100	בני ברק	301	אהרונוביץ	synonym
             6100	בני ברק	302	רבי עקיבא	official
",
        );
        assert_eq!(found(g.resolve("אהרוינוביטש 22, בני ברק")).spoken(), "אהרונוביץ ראובן 22, בני ברק");
        assert!(matches!(g.resolve("עונה 32, בני ברק"), Lookup::NoStreet { .. }));
        // Three consonants are not enough to call it the same street.
        let g = Gazetteer::from_tsv(
            "3000	ירושלים	10	יוסי הצייר	official
",
        );
        assert!(matches!(g.resolve("שוויצר 3, ירושלים"), Lookup::NoStreet { .. }));
    }

    #[test]
    fn a_street_of_a_sound_alike_city_names_that_city() {
        // From a live call: "לאלעד" was heard "לעדו", the agent wrote "עדי" (a moshav).
        let g = Gazetteer::from_tsv(
            "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
             1309\tאלעד\t111\tרבי עקיבא\tofficial\n199\tעדי\t9000\tעדי\tofficial\n\
             2066\tבן זכאי\t9000\tבן זכאי\tofficial\n",
        );
        assert_eq!(found(g.resolve("בן זכאי 45, עדי")).official(), "רבן יוחנן בן זכאי 45, אלעד");
        // A street no sound-alike city has stays unknown.
        assert!(matches!(g.resolve("הרצל 3, עדי"), Lookup::NoStreet { .. }));
    }

    #[test]
    fn a_landmark_from_the_prompt_is_not_heard_in_another_street() {
        // A live test: the caller said "סוכות"; the agent wrote the prompt's "בנייני האומה".
        let heard = "אני רוצה להזמין מונית. לעדו. בן זכאי 45. אני נמצאת בבן זכאי 45. ירושלים. סוכות.";
        assert!(
            unheard_words("בנייני האומה, ירושלים", heard).contains(&"בנייני".to_string()),
            "one unheard word rejects it"
        );
        assert!(unheard_words("בנייני האומה, ירושלים", "זה בני ינאי אומה בירושלים").is_empty());
        // Garbled across words by the recognizer (a live call): the whole name sounds the same.
        assert!(unheard_words("בנייני האומה, ירושלים", "אה, ירושלים. בניהול אומה.").is_empty());
        // Written in Latin letters by the recognizer: still heard.
        assert!(unheard_words("בנייני האומה, ירושלים", "ירושלים. זה בניין ה-Human, אני לא זוכר את הרחוב").is_empty());
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
