//! The engine is generic and a business is data: a clinic, written only as JSON, runs
//! through the same engine and agent as the taxi business, and nothing of the taxi
//! business leaks into its prompt.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use callora_core::agent::{build_request, system_prompt, AgentAction, AgentTurn};
use callora_core::business::Business;
use callora_core::engine::{Directive, Engine};

const CLINIC: &str = include_str!("fixtures/clinic.json");

fn clinic() -> Arc<Business> {
    Arc::new(Business::from_json(CLINIC, "clinic.json", &|_| None).expect("the clinic is a valid business"))
}

fn turn(action: AgentAction, fields: &[(&str, &str)], phrase: Option<&str>) -> AgentTurn {
    AgentTurn {
        phrase: phrase.map(Into::into),
        say: String::new(),
        action,
        task: Some("book_appointment".into()),
        fields: fields.iter().map(|(s, v)| (s.to_string(), v.to_string())).collect(),
    }
}

fn spoken(d: &[Directive]) -> String {
    d.iter()
        .filter_map(|d| match d {
            Directive::Speak { plan, .. } => Some(plan.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_prompt_is_the_clinics_own() {
    let b = clinic();
    let system = system_prompt(&b);
    assert!(system.contains("מרפאת השכונה"));
    assert!(system.contains("experienced human receptionist"), "{system}");
    assert!(system.contains("- ask_day: \"לאיזה יום?\""), "{system}");
    assert!(system.contains("(insurance)"), "the optional detail is named: {system}");
    for taxi in ["מונית", "נוסע", "נהג", "dispatcher", "street", "רחוב", "דיזנגוף", "luggage"] {
        assert!(!system.contains(taxi), "`{taxi}` leaked into the clinic's prompt:\n{system}");
    }
    let request = build_request(&b, &Engine::new(b.clone(), 1).state, "שלום");
    assert!(!request.user.contains("ADDRESS FORM"), "no address forms configured: {}", request.user);
}

#[test]
fn an_appointment_is_booked_through_the_same_engine() {
    let b = clinic();
    let mut engine = Engine::new(b, 3);
    engine.start();
    let d = engine.on_agent_turn(
        "אני צריכה תור לרופא ילדים",
        turn(AgentAction::None, &[("doctor", "רופא ילדים")], Some("ask_day")),
        "",
    );
    assert_eq!(spoken(&d), "לאיזה יום?");
    engine.on_agent_turn(
        "ליום שלישי, על שם נועה לוי",
        turn(AgentAction::None, &[("day", "שלישי"), ("patient_name", "נועה לוי")], None),
        "",
    );
    let d = engine.on_agent_turn("זהו", turn(AgentAction::ReadBack, &[], None), "");
    let text = spoken(&d);
    assert!(text.contains("לקבוע?") && text.contains("נועה לוי"), "{text}");

    // Only a real yes books it.
    let d = engine.on_agent_turn("כן", turn(AgentAction::Submit, &[], None), "");
    assert!(
        d.iter().any(|d| matches!(d, Directive::RunAction { action, .. } if action == "book_appointment")),
        "{d:?}"
    );
}
