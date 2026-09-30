//! A taxi price list as the Israeli price bots write it, and the quote for one ride out of it.
//!
//! The bot answers "מ בני ברק לירושלים" with lines like "🚗 4 מק' - ₪220" (one way) followed by
//! "♾️ צדדים - ₪400" (there and back), one block per vehicle size, and sometimes a second set
//! of blocks under "תוספת שכונות - ירושלים" for the neighbourhoods listed under it. Only the
//! numbers are read; the emoji and wording around them may change.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::text::normalize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PriceList {
    pub distance_km: Option<u32>,
    /// "שעה ו6 דקות", as the bot wrote it.
    pub duration: Option<String>,
    /// "נמוך", as the bot wrote it: prices change with the hour.
    pub tier: Option<String>,
    /// The route's prices first, then those of listed neighbourhoods.
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Section {
    /// The neighbourhoods these prices are for; empty for the route itself.
    pub neighborhoods: Vec<String>,
    pub vehicles: Vec<Vehicle>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Vehicle {
    /// "6 מק' קטן", as the bot wrote it.
    pub label: String,
    pub seats: u32,
    /// "מרווח": the roomier car of the same size, dearer.
    pub roomy: bool,
    pub one_way: u32,
    /// "צדדים": there and back.
    pub round_trip: Option<u32>,
}

static SEATS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*מק").expect("valid regex"));
static PRICE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"₪\s*(\d[\d,]*)|(\d[\d,]*)\s*(?:₪|ש״ח|ש\x22ח|שח|שקל)").expect("valid regex"));
static KM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*ק").expect("valid regex"));

fn price_in(line: &str) -> Option<u32> {
    let c = PRICE.captures(line)?;
    let digits: String = c.get(1).or_else(|| c.get(2))?.as_str().chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// What follows the " - " of "סוג מחירון - נמוך 🥉", without the emoji.
fn value_after_dash(line: &str) -> Option<String> {
    let (_, v) = line.split_once(" - ").or_else(|| line.split_once('-'))?;
    let v: String = v.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace() || "'״\"׳".contains(*c)).collect();
    let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
    (!v.is_empty()).then_some(v)
}

/// The label of a vehicle line: "🚙 6 מק' קטן - ₪300" → "6 מק' קטן".
fn label_of(line: &str) -> String {
    let before = line.split('₪').next().unwrap_or(line);
    let before = before.trim_end().trim_end_matches(['-', '–', ':']).trim();
    let start = before.find(|c: char| c.is_ascii_digit()).unwrap_or(0);
    before[start..].trim().to_string()
}

/// The price list in a bot's reply, or `None` when it has no vehicle prices (an error, an ad).
pub fn parse(text: &str) -> Option<PriceList> {
    let mut list = PriceList { distance_km: None, duration: None, tier: None, sections: Vec::new() };
    let mut current = Section { neighborhoods: Vec::new(), vehicles: Vec::new() };
    let mut expect_neighborhoods = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if expect_neighborhoods {
            expect_neighborhoods = false;
            if line.contains('/') || !SEATS.is_match(line) && price_in(line).is_none() {
                current.neighborhoods = line
                    .split(['/', ','])
                    .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|n| !n.is_empty())
                    .collect();
                continue;
            }
        }
        if line.contains("שכונות") {
            if !current.vehicles.is_empty() {
                list.sections
                    .push(std::mem::replace(&mut current, Section { neighborhoods: Vec::new(), vehicles: Vec::new() }));
            }
            expect_neighborhoods = true;
            continue;
        }
        if line.contains("מרחק") {
            list.distance_km = KM.captures(line).and_then(|c| c[1].parse().ok());
            continue;
        }
        if line.contains("זמן משוער") {
            list.duration = value_after_dash(line);
            continue;
        }
        if line.contains("מחירון") && line.contains("סוג") {
            list.tier = value_after_dash(line);
            continue;
        }
        let Some(price) = price_in(line) else { continue };
        if line.contains("צדדים") || line.contains("הלוך חזור") {
            if let Some(v) = current.vehicles.last_mut() {
                v.round_trip.get_or_insert(price);
            }
            continue;
        }
        if let Some(seats) = SEATS.captures(line).and_then(|c| c[1].parse().ok()) {
            current.vehicles.push(Vehicle {
                label: label_of(line),
                seats,
                roomy: line.contains("מרווח"),
                one_way: price,
                round_trip: None,
            });
        }
    }
    if !current.vehicles.is_empty() {
        list.sections.push(current);
    }
    (!list.sections.is_empty()).then_some(list)
}

impl PriceList {
    /// The prices of the route, or of a listed neighbourhood named in one of `places`
    /// ("גילה, ירושלים").
    fn section_for(&self, places: &[&str]) -> &Section {
        let named = |n: &str| {
            let n = normalize(n);
            !n.is_empty() && places.iter().any(|p| format!(" {} ", normalize(p)).contains(&format!(" {n} ")))
        };
        self.sections
            .iter()
            .find(|s| s.neighborhoods.iter().any(|n| named(n)))
            .or_else(|| self.sections.iter().find(|s| s.neighborhoods.is_empty()))
            .unwrap_or(&self.sections[0])
    }

    /// The smallest car that seats `people`, the ordinary one before the roomy one.
    fn vehicle_for(section: &Section, people: u32) -> Option<&Vehicle> {
        section.vehicles.iter().filter(|v| v.seats >= people).min_by_key(|v| (v.seats, v.roomy, v.one_way))
    }

    /// The largest car, the ordinary one before the roomy one.
    fn largest(section: &Section) -> Option<&Vehicle> {
        section.vehicles.iter().max_by_key(|v| (v.seats, std::cmp::Reverse((v.roomy, v.one_way))))
    }

    /// The quote for a ride: `passengers` when known, there and back when asked. The result
    /// names the response that says it (`response`): one price, the price by car size when the
    /// passengers are not known, one way and there and back, or, for a group no car seats, the
    /// largest car's price and that it takes more than one. `None` only for a list with no cars.
    pub fn quote(&self, passengers: Option<u32>, round_trip: bool, places: &[&str]) -> Option<Value> {
        let section = self.section_for(places);
        let neighborhood = !section.neighborhoods.is_empty();
        let people = passengers.unwrap_or(1).max(1);
        let Some(car) = Self::vehicle_for(section, people) else {
            // Twelve people: no car in the list seats them all.
            let big = Self::largest(section)?;
            return Some(json!({
                "price": big.one_way,
                "vehicle": big.label,
                "seats": big.seats,
                "neighborhood": neighborhood,
                "distance_km": self.distance_km,
                "tier": self.tier,
                "more_than_one_car": true,
                "response": "price_answer_big_group",
            }));
        };
        let mut quote = json!({
            "price": car.one_way,
            "vehicle": car.label,
            "seats": car.seats,
            "neighborhood": neighborhood,
            "distance_km": self.distance_km,
            "duration": self.duration,
            "tier": self.tier,
        });
        if round_trip {
            if let Some(both) = car.round_trip {
                quote["round_trip"] = json!(both);
                quote["response"] = json!("price_answer_round_trip");
                return Some(quote);
            }
        }
        if passengers.is_none() {
            // Who is coming is not known: the price for up to four, and for up to six.
            if let Some(six) = Self::vehicle_for(section, 6).filter(|six| six.seats > car.seats) {
                quote["price_6"] = json!(six.one_way);
                quote["response"] = json!("price_answer_sizes");
                return Some(quote);
            }
        }
        quote["response"] = json!("price_answer");
        Some(quote)
    }
}

/// The city of a place as the caller or the system wrote it: "אהרונוביץ 32, בני ברק" → "בני ברק".
pub fn city_of(place: &str) -> String {
    let city = place.rsplit(',').next().unwrap_or(place).trim();
    city.split(" (").next().unwrap_or(city).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = include_str!("../tests/fixtures/price_list_bnei_brak_jerusalem.txt");

    #[test]
    fn a_bots_reply_is_read_into_prices() {
        let list = parse(REPLY).expect("a price list");
        assert_eq!(list.distance_km, Some(67));
        assert_eq!(list.duration.as_deref(), Some("שעה ו6 דקות"));
        assert_eq!(list.tier.as_deref(), Some("נמוך"));
        assert_eq!(list.sections.len(), 2);
        let route = &list.sections[0];
        assert!(route.neighborhoods.is_empty());
        let seen: Vec<(String, u32, bool, u32, Option<u32>)> =
            route.vehicles.iter().map(|v| (v.label.clone(), v.seats, v.roomy, v.one_way, v.round_trip)).collect();
        assert_eq!(
            seen,
            vec![
                ("4 מק'".to_string(), 4, false, 220, Some(400)),
                ("6 מק' קטן".to_string(), 6, false, 300, Some(550)),
                ("6 מק' מרווח".to_string(), 6, true, 350, Some(600)),
                ("7 מק' (סיינה)".to_string(), 7, false, 370, Some(640)),
            ]
        );
        let extra = &list.sections[1];
        assert!(
            extra.neighborhoods.contains(&"גילה".to_string()) && extra.neighborhoods.contains(&"הר הזיתים".to_string())
        );
        assert_eq!(extra.vehicles[0].one_way, 240);
        assert_eq!(extra.vehicles[3].round_trip, Some(690));
    }

    #[test]
    fn the_quote_follows_the_ride() {
        let list = parse(REPLY).expect("a price list");
        let q = list.quote(None, false, &["בני ברק", "ירושלים"]).expect("a quote");
        assert_eq!(
            (q["response"].as_str(), q["price"].as_u64(), q["price_6"].as_u64()),
            (Some("price_answer_sizes"), Some(220), Some(300))
        );
        let q = list.quote(Some(3), false, &[]).expect("a quote");
        assert_eq!((q["response"].as_str(), q["price"].as_u64()), (Some("price_answer"), Some(220)));
        let q = list.quote(Some(5), false, &[]).expect("a quote");
        assert_eq!(q["price"].as_u64(), Some(300), "the smaller six-seater, not the roomy one");
        let q = list.quote(Some(7), false, &[]).expect("a quote");
        assert_eq!(q["price"].as_u64(), Some(370));
        let q = list.quote(Some(12), false, &[]).expect("a quote");
        assert_eq!(
            (q["response"].as_str(), q["price"].as_u64(), q["seats"].as_u64()),
            (Some("price_answer_big_group"), Some(370), Some(7)),
            "twelve: the largest car, and that it takes more than one"
        );
        let q = list.quote(Some(2), true, &[]).expect("a quote");
        assert_eq!((q["response"].as_str(), q["round_trip"].as_u64()), (Some("price_answer_round_trip"), Some(400)));
        let q = list.quote(Some(2), false, &["בני ברק", "גילה, ירושלים"]).expect("a quote");
        assert_eq!((q["price"].as_u64(), q["neighborhood"].as_bool()), (Some(240), Some(true)));
    }

    #[test]
    fn a_reply_without_prices_is_no_price_list() {
        assert!(parse("👇 מצטרפים כאן:\nhttps://t.me/+x").is_none());
        assert!(parse("לא נמצא מסלול").is_none());
    }

    #[test]
    fn the_city_of_a_place() {
        assert_eq!(city_of("אהרונוביץ 32, בני ברק"), "בני ברק");
        assert_eq!(city_of("ירושלים"), "ירושלים");
        assert_eq!(city_of("בן זה קיץ 46, אלעד (מקום לא מאומת: לתאם עם הנוסע)"), "אלעד");
    }
}
