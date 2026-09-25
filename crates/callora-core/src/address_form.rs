//! How to address the caller in Hebrew: masculine, feminine, or not known yet.
//!
//! Only from what the caller says about themselves ("אני צריכה מונית"), never from the
//! voice. Plain word rules over the transcript: microseconds, no model. Until a clear cue,
//! the agent speaks in neutral forms ("לאן נוסעים?").

use serde::{Deserialize, Serialize};

use crate::text::normalize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddressForm {
    #[default]
    Unknown,
    Masculine,
    Feminine,
}

/// First-person present forms (masculine, feminine) that say who is speaking.
const FORMS: &[(&str, &str)] = &[
    ("צריך", "צריכה"),
    ("מעוניין", "מעוניינת"),
    ("נמצא", "נמצאת"),
    ("יכול", "יכולה"),
    ("עומד", "עומדת"),
    ("יושב", "יושבת"),
    ("חייב", "חייבת"),
    ("בטוח", "בטוחה"),
    ("מזמין", "מזמינה"),
    ("מתקשר", "מתקשרת"),
    ("מבקש", "מבקשת"),
    ("נוסע", "נוסעת"),
    ("יוצא", "יוצאת"),
    ("מגיע", "מגיעה"),
    ("הולך", "הולכת"),
    ("חושב", "חושבת"),
    ("יודע", "יודעת"),
    ("זוכר", "זוכרת"),
    ("מבין", "מבינה"),
    ("שומע", "שומעת"),
    ("מתכוון", "מתכוונת"),
    ("גר", "גרה"),
    ("עובד", "עובדת"),
    ("ממהר", "ממהרת"),
    ("מאחר", "מאחרת"),
    ("מוכן", "מוכנה"),
    ("זקוק", "זקוקה"),
    ("מצטער", "מצטערת"),
    ("מרגיש", "מרגישה"),
    ("תקוע", "תקועה"),
    ("שמח", "שמחה"),
];

/// Words between "אני" and the form that say nothing about it ("אני לא צריכה").
const SKIP: &[&str] = &[
    "לא",
    "כבר",
    "ממש",
    "עדיין",
    "רק",
    "גם",
    "פשוט",
    "עכשיו",
    "באמת",
    "די",
    "קצת",
    "בדיוק",
    "מאוד",
    "כן",
    "בעצם",
    "אה",
    "אמ",
];

/// What the caller says outright ("אני אישה", "תדבר אליי בלשון נקבה").
const SELF: &[(&str, AddressForm)] = &[
    ("לשון נקבה", AddressForm::Feminine),
    ("לשון זכר", AddressForm::Masculine),
    ("אני אישה", AddressForm::Feminine),
    ("אני בחורה", AddressForm::Feminine),
    ("אני גבר", AddressForm::Masculine),
    ("אני בחור", AddressForm::Masculine),
];

fn form_of(word: &str) -> Option<AddressForm> {
    FORMS.iter().find_map(|(m, f)| {
        if word == *m {
            Some(AddressForm::Masculine)
        } else if word == *f {
            Some(AddressForm::Feminine)
        } else {
            None
        }
    })
}

fn is_self(word: &str) -> bool {
    matches!(word, "אני" | "ואני" | "שאני" | "כשאני" | "אנוכי")
}

/// The form a caller's words clearly show, if any. Masculine counts only after "אני":
/// alone, "צריך מונית" is also impersonal ("a taxi is needed"), while "צריכה מונית" at
/// the start is a woman speaking. Cues of both forms in one utterance show nothing.
pub fn detect(text: &str) -> Option<AddressForm> {
    let norm = normalize(text);
    let spaced = format!(" {norm} ");
    let stated: Vec<AddressForm> =
        SELF.iter().filter(|(p, _)| spaced.contains(&format!(" {p} "))).map(|(_, f)| *f).collect();
    if let [only] = stated.as_slice() {
        return Some(*only);
    }
    if !stated.is_empty() {
        return None;
    }
    let words: Vec<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
    let mut found: Option<AddressForm> = None;
    let mut note = |f: AddressForm| -> bool {
        if found.is_some_and(|g| g != f) {
            return false;
        }
        found = Some(f);
        true
    };
    for (i, w) in words.iter().enumerate() {
        if is_self(w) {
            let next = words[i + 1..].iter().take(4).find(|w| !SKIP.contains(w));
            if let Some(f) = next.and_then(|n| form_of(n)) {
                if !note(f) {
                    return None;
                }
            }
        }
    }
    let first = words.iter().find(|w| !SKIP.contains(w));
    if let Some(AddressForm::Feminine) = first.and_then(|w| form_of(w)) {
        if !note(AddressForm::Feminine) {
            return None;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use AddressForm::*;

    #[test]
    fn first_person_forms_show_the_form() {
        for (said, want) in [
            ("אני צריך מונית", Some(Masculine)),
            ("אני צריכה מונית", Some(Feminine)),
            ("אני מעוניין להזמין", Some(Masculine)),
            ("אני מעוניינת להזמין", Some(Feminine)),
            ("אה, אני לא יודעת את הרחוב", Some(Feminine)),
            ("אני כבר נמצא למטה", Some(Masculine)),
            ("כן, ואני צריכה גם תא מטען", Some(Feminine)),
            ("צריכה מונית לרעננה", Some(Feminine)),
            ("תגיד, אני יכולה להזמין לעוד שעה?", Some(Feminine)),
            ("אני אישה, תדבר אליי בלשון נקבה", Some(Feminine)),
            ("אני גבר", Some(Masculine)),
        ] {
            assert_eq!(detect(said), want, "{said}");
        }
    }

    #[test]
    fn no_clear_cue_leaves_it_unknown() {
        for said in [
            "צריך מונית לתל אביב",
            "רוצה מונית",
            "אני רוצה מונית",
            "מרבי עקיבא 12 בבני ברק",
            "שלושה נוסעים",
            "היא צריכה מונית",
            "אני צריך, כלומר אני צריכה",
            "אני דנה",
            "כן",
        ] {
            assert_eq!(detect(said), None, "{said}");
        }
    }

    #[test]
    fn serialized_as_a_plain_word() {
        assert_eq!(serde_json::to_value(Unknown).unwrap(), "unknown");
        assert_eq!(serde_json::to_value(Feminine).unwrap(), "feminine");
    }
}
