//! End-to-end conversation behaviour against the real taxi business config, driven
//! through the same path the runtime uses: fast-path understanding → engine → directives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use callora_core::agent::{AgentAction, AgentTurn};
use callora_core::business::{validate, Business, BusinessRegistry};
use callora_core::config::BusinessConfig;
use callora_core::customer::{Customer, CustomerPlace};
use callora_core::engine::{Directive, Engine};
use callora_core::llm::parse_response;
use callora_core::render::{library_entries, SegmentOrigin};
use callora_core::state::Step;
use callora_core::understanding::{fast_path, merge};
use callora_core::values::SlotValue;

const TAXI: &str = include_str!("../../../businesses/taxi.json");

fn business(env: &[(&str, &str)]) -> Arc<Business> {
    let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Arc::new(Business::from_json(TAXI, "taxi.json", &|k| env.get(k).cloned()).expect("taxi config is valid"))
}

fn with_desk() -> Arc<Business> {
    business(&[("TAXI_HANDOFF_NUMBER", "+972500000001"), ("TAXI_PHONE_NUMBERS", "+972500000000")])
}

struct Call {
    engine: Engine,
}

impl Call {
    fn new(b: Arc<Business>) -> (Self, Vec<Directive>) {
        let mut engine = Engine::new(b, 7);
        let greeting = engine.start();
        (Self { engine }, greeting)
    }

    fn say(&mut self, text: &str) -> Vec<Directive> {
        let b = self.engine.business().clone();
        let (u, _needs_llm) = fast_path(&b, &self.engine.context(), text);
        self.engine.on_utterance(u)
    }

    fn slot(&self, id: &str) -> Option<SlotValue> {
        self.engine.state.run.as_ref()?.slots.get(id).map(|s| s.value.clone())
    }

    fn step(&self) -> Option<Step> {
        self.engine.state.run.as_ref().map(|r| r.step.clone())
    }
}

fn spoken(directives: &[Directive]) -> String {
    directives
        .iter()
        .filter_map(|d| match d {
            Directive::Speak { plan, .. } => Some(plan.text()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn action(directives: &[Directive]) -> Option<(u64, String, serde_json::Value)> {
    directives.iter().find_map(|d| match d {
        Directive::RunAction { run_id, action, input } => Some((*run_id, action.clone(), input.clone())),
        _ => None,
    })
}

fn place(v: Option<SlotValue>) -> String {
    match v {
        Some(SlotValue::Place { spoken, .. }) => spoken,
        other => panic!("expected a place, got {other:?}"),
    }
}

#[test]
fn config_is_valid_and_routes_by_number() {
    let b = with_desk();
    assert!(validate(&b.config).is_empty());
    let reg = BusinessRegistry::new(vec![Business::from_json(TAXI, "taxi.json", &|k| {
        (k == "TAXI_PHONE_NUMBERS").then(|| "+972500000000, +972500000009".to_string())
    })
    .unwrap()])
    .unwrap();
    assert_eq!(reg.by_number("+972500000009").unwrap().config.id, "taxi");
    assert!(reg.by_number("+972599999999").is_none());
}

#[test]
fn md_example_29_full_booking_in_one_sentence() {
    let (mut call, greeting) = Call::new(with_desk());
    assert_eq!(spoken(&greeting), "אהלן, איך אפשר לעזור?");

    let d = call.say("צריך מונית עכשיו מרבי עקיבא 12 לנתב\"ג, אנחנו ארבעה.");
    assert_eq!(place(call.slot("pickup")), "רבי עקיבא 12");
    assert_eq!(place(call.slot("destination")), "נתב״ג");
    assert_eq!(call.slot("passengers"), Some(SlotValue::Integer { value: 4 }));
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation));
    let text = spoken(&d);
    assert!(text.contains("ארבעה נוסעים מרבי עקיבא 12 לנתב״ג, עכשיו. לשלוח?"), "{text}");

    let d = call.say("כן");
    assert!(matches!(&d[0], Directive::Speak { filler: true, .. }), "filler first: {d:?}");
    let (run_id, name, input) = action(&d).expect("create_ride runs");
    assert_eq!(name, "create_ride");
    assert_eq!(input["slots"]["passengers"], 4);
    assert_eq!(input["slots"]["destination"]["address"], "נמל התעופה בן גוריון, טרמינל 3");

    let d = call.engine.on_action_result(run_id, Ok(serde_json::json!({ "eta_minutes": 4 })));
    let text = spoken(&d);
    assert!(text.contains("ארבע דקות"), "{text}");
    assert!(call.engine.state.run.is_none());
    // The ETA sentence is a pre-generated template, not dynamic TTS.
    let Directive::Speak { plan, .. } = &d[0] else { panic!() };
    assert_eq!(plan.segments[0].origin, SegmentOrigin::Template);
}

#[test]
fn md_example_30_meta_intent_keeps_the_pipeline() {
    let (mut call, _) = Call::new(with_desk());
    let d = call.say("צריך מונית");
    assert!(spoken(&d).contains("לאסוף") || spoken(&d).contains("אוספים"), "{}", spoken(&d));
    let asked = spoken(&d);

    let d = call.say("מה?");
    assert_eq!(spoken(&d), asked, "repeat the exact question");
    assert_eq!(call.step(), Some(Step::Collecting { awaiting: Some("pickup".into()) }));

    let d = call.say("לא הבנתי");
    assert_eq!(spoken(&d), asked);
    let Directive::Speak { plan, .. } = &d[0] else { panic!() };
    assert_eq!(plan.segments[0].delivery, "slow", "did-not-understand repeats slower");

    let d = call.say("מעזרא 7");
    assert_eq!(place(call.slot("pickup")), "עזרא 7");
    let text = spoken(&d);
    assert!(text.contains("לאן"), "asks only for what is missing: {text}");
    assert!(!text.contains("מאיפה"), "{text}");
}

#[test]
fn md_example_31_lost_item() {
    let (mut call, _) = Call::new(with_desk());
    let d = call.say("השארתי תיק במונית שהייתה אצלי לפני שעה");
    assert_eq!(call.engine.state.run.as_ref().unwrap().pipeline, "lost_item");
    assert_eq!(call.slot("item"), Some(SlotValue::Text { text: "תיק".into() }));
    let (_, name, _) = action(&d).expect("nothing is missing, so the report is filed");
    assert_eq!(name, "report_lost_item");
}

#[test]
fn asks_only_for_missing_information() {
    let (mut call, _) = Call::new(with_desk());
    let d = call.say("תשלח לי מונית לתל השומר בעוד עשר דקות");
    assert_eq!(place(call.slot("destination")), "תל השומר");
    assert!(matches!(call.slot("pickup_time"), Some(SlotValue::Time { .. })));
    assert_eq!(call.step(), Some(Step::Collecting { awaiting: Some("pickup".into()) }));
    assert!(!spoken(&d).contains("לאן"));

    call.say("מז'בוטינסקי 5 רמת גן");
    assert_eq!(place(call.slot("pickup")), "ז'בוטינסקי 5 רמת גן");
    let d = call.say("שלושה");
    assert_eq!(call.slot("passengers"), Some(SlotValue::Integer { value: 3 }));
    assert!(spoken(&d).contains("בעוד עשר דקות"), "{}", spoken(&d));
}

#[test]
fn correction_during_read_back_updates_and_reconfirms() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג, אנחנו שניים");
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation));
    let d = call.say("לא, לעזריאלי");
    assert_eq!(place(call.slot("destination")), "עזריאלי");
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation));
    assert!(spoken(&d).contains("לעזריאלי"), "{}", spoken(&d));
    assert_eq!(place(call.slot("pickup")), "רבי עקיבא 12", "other values survive");
}

#[test]
fn saying_no_to_the_read_back_asks_what_to_change() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג, אנחנו שניים");
    let d = call.say("לא");
    assert!(spoken(&d).contains("לתקן") || spoken(&d).contains("לשנות"));
    let d = call.say("אנחנו שלושה");
    assert_eq!(call.slot("passengers"), Some(SlotValue::Integer { value: 3 }));
    assert!(spoken(&d).contains("שלושה נוסעים"));
}

#[test]
fn go_back_undoes_the_last_value() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית");
    call.say("מרבי עקיבא 12");
    assert_eq!(call.step(), Some(Step::Collecting { awaiting: Some("destination".into()) }));
    let d = call.say("רגע טעיתי");
    assert_eq!(call.slot("pickup"), None);
    assert_eq!(call.step(), Some(Step::Collecting { awaiting: Some("pickup".into()) }));
    assert!(spoken(&d).contains("אין בעיה"));
}

#[test]
fn cancel_then_no_more_ends_the_call() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12");
    let d = call.say("עזוב, תבטל");
    assert!(call.engine.state.run.is_none());
    assert!(spoken(&d).contains("ביטלתי"));
    let d = call.say("לא, תודה");
    assert!(d.iter().any(|d| matches!(d, Directive::Hangup)), "{d:?}");
}

#[test]
fn wait_says_nothing_and_changes_nothing() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית");
    let before = call.engine.state.run.clone();
    assert!(call.say("רגע").is_empty());
    assert_eq!(call.engine.state.run, before);
}

#[test]
fn filler_only_transcripts_are_noise() {
    let (mut call, _) = Call::new(business(&[]));
    call.say("צריך מונית");
    let before = call.engine.state.clone();
    for text in ["תודה.", "תודה", "תודה רבה.", "אה", "הלו"] {
        let (u, needs_llm) = fast_path(&call.engine.business().clone(), &call.engine.context(), text);
        assert!(u.noise && !needs_llm, "{text}");
        assert!(call.say(text).is_empty(), "{text}");
    }
    assert_eq!(call.engine.state, before, "noise changes nothing, not even the fallback ladder");
    for text in ["לא, תודה", "תודה ביי", "כן תודה"] {
        let (u, _) = fast_path(&call.engine.business().clone(), &call.engine.context(), text);
        assert!(!u.noise, "{text}");
    }
}

#[test]
fn the_llm_decides_whether_an_ununderstood_utterance_was_meant_for_the_agent() {
    // From a real call: garbage the rules cannot read must not trigger an automatic
    // "didn't catch that"; the LLM says whether it was noise or an unclear request.
    let (call, _) = Call::new(business(&[]));
    let b = call.engine.business().clone();
    let text = "אני לא רגעתיים. שי, בי, בי, בי";
    let (fast, needs_llm) = fast_path(&b, &call.engine.context(), text);
    assert!(needs_llm, "garbage goes to the LLM");
    let (empty, needs_llm) = fast_path(&b, &call.engine.context(), "שי, בי, בי, בי");
    assert!(empty.is_empty() && needs_llm, "nothing understood goes to the LLM");

    let reply = |speech: &str, slots: serde_json::Value| {
        serde_json::json!({ "speech": speech, "meta_intent": null, "intent": null, "intent_confidence": 0.0,
            "affirm": null, "frustrated": false, "slots": slots })
    };
    let noise = parse_response(&b, &call.engine.context(), text, &reply("not_for_agent", serde_json::json!([])));
    assert!(noise.noise);
    assert!(merge(fast.clone(), noise).noise, "noise when the rules found nothing either");

    let unclear = parse_response(&b, &call.engine.context(), text, &reply("unclear", serde_json::json!([])));
    assert!(!merge(fast.clone(), unclear).noise, "an unclear request still gets a reply");

    let with_value =
        reply("not_for_agent", serde_json::json!([{ "slot": "destination", "value": "תל אביב", "confidence": 0.9 }]));
    assert!(!parse_response(&b, &call.engine.context(), text, &with_value).noise, "a value is never noise");
}

// The agent decides; the engine enforces.

fn decide(action: AgentAction, say: &str, task: Option<&str>, fields: &[(&str, &str)]) -> AgentTurn {
    AgentTurn {
        say: say.into(),
        action,
        task: task.map(Into::into),
        fields: fields.iter().map(|(s, v)| (s.to_string(), v.to_string())).collect(),
    }
}

fn hangs_up(d: &[Directive]) -> bool {
    d.iter().any(|d| matches!(d, Directive::Hangup))
}

#[test]
fn agent_turns_fill_the_booking_through_the_parsers() {
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn(
        "אני רוצה מונית מבאר שבע",
        decide(AgentAction::None, "לאן נוסעים?", Some("book_ride"), &[("pickup", "מבאר שבע")]),
        "",
    );
    assert_eq!(spoken(&d), "לאן נוסעים?");
    assert_eq!(call.engine.state.run.as_ref().unwrap().pipeline, "book_ride");
    assert_eq!(place(call.slot("pickup")), "באר שבע", "the leading preposition is stripped by the parser");
    let history: Vec<_> = call.engine.state.history.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(history[history.len() - 2..], ["אני רוצה מונית מבאר שבע", "לאן נוסעים?"]);
}

#[test]
fn nothing_is_sent_without_a_read_back_and_a_yes() {
    let (mut call, _) = Call::new(business(&[]));
    let fields = [("pickup", "רבי עקיבא 12"), ("destination", "תל אביב"), ("passengers", "אחד")];
    // A submit straight away becomes the read-back.
    let d = call.engine.on_agent_turn(
        "מרבי עקיבא 12 לתל אביב, תשלח",
        decide(AgentAction::Submit, "סגור.", Some("book_ride"), &fields),
        "",
    );
    assert!(action(&d).is_none(), "no booking before the read-back: {d:?}");
    assert!(spoken(&d).contains("לשלוח?"), "{}", spoken(&d));
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation));

    // A correction instead of a yes reads everything back again.
    let d = call.engine.on_agent_turn(
        "לא, לרמת גן",
        decide(AgentAction::Submit, "", None, &[("destination", "לרמת גן")]),
        "",
    );
    assert!(action(&d).is_none(), "{d:?}");
    assert_eq!(place(call.slot("destination")), "רמת גן");
    assert!(spoken(&d).contains("לרמת גן") && spoken(&d).contains("לשלוח?"), "{}", spoken(&d));

    // Now the yes: the booking goes out.
    let d = call.engine.on_agent_turn("כן תשלח", decide(AgentAction::Submit, "סגור.", None, &[]), "");
    let (_, name, input) = action(&d).expect("create_ride after the confirmed read-back");
    assert_eq!(name, "create_ride");
    assert_eq!(input["slots"]["destination"]["spoken"], "רמת גן");
}

#[test]
fn a_read_back_with_a_detail_missing_asks_for_it() {
    let (mut call, _) = Call::new(business(&[]));
    let fields = [("pickup", "רבי עקיבא 12"), ("destination", "תל אביב")];
    let d = call.engine.on_agent_turn(
        "מרבי עקיבא 12 לתל אביב",
        decide(AgentAction::ReadBack, "סגור.", Some("book_ride"), &fields),
        "",
    );
    assert!(spoken(&d).contains("סגור.") && spoken(&d).contains("כמה"), "asks for the passengers: {}", spoken(&d));
    assert_ne!(call.step(), Some(Step::AwaitingConfirmation));
}

#[test]
fn a_question_before_the_read_back_is_dropped() {
    let (mut call, _) = Call::new(business(&[]));
    let fields = [("pickup", "רבי עקיבא 12"), ("destination", "תל אביב"), ("passengers", "שניים")];
    let d = call.engine.on_agent_turn(
        "אנחנו שניים",
        decide(AgentAction::ReadBack, "סבבה, יש מזוודות?", Some("book_ride"), &fields),
        "",
    );
    let text = spoken(&d);
    assert!(!text.contains("מזוודות"), "{text}");
    assert_eq!(text.matches('?').count(), 1, "one question, the read-back's: {text}");
}

#[test]
fn an_impossible_value_is_rejected_and_the_agent_hears_about_it() {
    // From a live call: "42 נוסעים" went into the booking although the most is 20.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "42 נוסעים",
        decide(AgentAction::None, "הבנתי.", Some("book_ride"), &[("passengers", "42")]),
        "",
    );
    assert_eq!(call.slot("passengers"), None, "not in the booking");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "ארבעה");
    assert!(next.user.contains("passengers \"42\" was not accepted (allowed 1-20)"), "{}", next.user);
    call.engine.on_agent_turn("ארבעה", decide(AgentAction::None, "כמה נוסעים?", None, &[("passengers", "ארבעה")]), "");
    assert_eq!(call.slot("passengers"), Some(SlotValue::Integer { value: 4 }));
    assert!(call.engine.state.agent_notes.is_empty(), "the note was delivered once");
}

#[test]
fn drawn_out_hesitations_are_noise() {
    let (call, _) = Call::new(business(&[]));
    let b = call.engine.business().clone();
    for text in ["אההה...", "אממממ", "המממ", "אהה אממ"] {
        let (u, needs_llm) = fast_path(&b, &call.engine.context(), text);
        assert!(u.noise && !needs_llm, "{text}");
    }
    let (u, _) = fast_path(&b, &call.engine.context(), "אה, לתל אביב");
    assert!(!u.noise);
}

#[test]
fn the_agent_cannot_hang_up_on_garbled_speech() {
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn("אהה, מה חטאת?", decide(AgentAction::EndCall, "יאללה ביי!", None, &[]), "");
    assert!(!hangs_up(&d), "{d:?}");
    assert_eq!(call.engine.state.phase, callora_core::state::Phase::Active);

    let d =
        call.engine.on_agent_turn("לא, זהו, תודה", decide(AgentAction::EndCall, "יאללה, נסיעה טובה!", None, &[]), "");
    assert!(hangs_up(&d), "a real goodbye ends the call: {d:?}");
}

#[test]
fn speech_already_streamed_is_recorded_not_repeated() {
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn(
        "מה המצב?",
        decide(AgentAction::None, "הכל טוב, תודה! איך אפשר לעזור?", None, &[]),
        "הכל טוב, תודה! איך אפשר לעזור?",
    );
    assert!(spoken(&d).is_empty(), "already played: {d:?}");
    assert_eq!(
        call.engine.state.last_plan.as_ref().map(|p| p.text()).as_deref(),
        Some("הכל טוב, תודה! איך אפשר לעזור?")
    );
    let d = call.say("מה?");
    assert_eq!(spoken(&d), "הכל טוב, תודה! איך אפשר לעזור?", "a repeat says it again");
}

#[test]
fn an_empty_decision_never_leaves_the_caller_in_silence() {
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn("...", decide(AgentAction::None, "", None, &[]), "");
    assert!(spoken(&d).contains("לא בטוח שהבנתי"), "{}", spoken(&d));
}

#[test]
fn the_agent_prompt_carries_the_business_and_its_instant_phrases() {
    let b = business(&[]);
    let system = callora_core::agent::system_prompt(&b);
    for needle in [
        "מוניות קלורה",
        "book_ride",
        "pickup (required)",
        "\"מאיפה אוספים?\"",
        "\"לאן נוסעים?\"",
        "Needs read_back then submit",
    ] {
        assert!(system.contains(needle), "{needle} missing from the prompt");
    }
    let request = callora_core::agent::build_request(&b, &Call::new(business(&[])).0.engine.state, "היי");
    let order: Vec<&str> = request.schema["properties"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(order, ["action", "say", "task", "fields"], "the action, then the words, stream first");
    assert!(request.user.ends_with("CALLER NOW: \"היי\""), "{}", request.user);
}

// The next three come from one live call, turn by turn.

#[test]
fn small_talk_gets_a_friendly_answer_not_silence() {
    let (mut call, _) = Call::new(business(&[]));
    let d = call.say("מה המצב?");
    let text = spoken(&d);
    assert!(text.contains("אפשר לעזור"), "{text}");
    assert!(call.engine.state.run.is_none(), "small talk starts no flow");
}

#[test]
fn a_verb_after_a_preposition_is_checked_by_the_llm_not_read_back() {
    let (call, _) = Call::new(business(&[]));
    let b = call.engine.business().clone();
    let text = "אני רוצה לשים מונית.";
    let (fast, needs_llm) = fast_path(&b, &call.engine.context(), text);
    assert!(needs_llm, "a doubtful value goes to the LLM: {fast:?}");
    // The LLM sees the booking and no destination; the rules' guess is dropped.
    let reply = serde_json::json!({ "speech": "clear", "meta_intent": null, "intent": "book_ride",
        "intent_confidence": 0.9, "affirm": null, "frustrated": false, "slots": [] });
    let u = merge(fast, parse_response(&b, &call.engine.context(), text, &reply));
    assert!(u.slots.is_empty(), "{:?}", u.slots);
    assert_eq!(u.intent.map(|i| i.id).as_deref(), Some("book_ride"));
}

#[test]
fn asking_what_was_said_repeats_it_instead_of_becoming_a_value() {
    let (mut call, _) = Call::new(business(&[]));
    call.say("צריך מונית");
    call.say("מרבי עקיבא 12");
    let asked = spoken(&call.say("תלאבי"));
    let d = call.say("מה? מה שאמרת לי?");
    assert_eq!(spoken(&d), asked, "repeats the question");
    assert_eq!(call.step(), Some(Step::ConfirmingSlot { slot: "destination".into() }));
}

#[test]
fn a_misheard_correction_asks_again_instead_of_reading_it_back() {
    // From a real call: "תל אביב" heard as "תלאבי", then every correction misheard too.
    let (mut call, _) = Call::new(business(&[]));
    call.say("צריך מונית");
    call.say("מרבי עקיבא 12");
    let d = call.say("תלאבי");
    assert_eq!(call.step(), Some(Step::ConfirmingSlot { slot: "destination".into() }));
    assert!(spoken(&d).contains("תלאבי, נכון?"), "{}", spoken(&d));

    let b = call.engine.business().clone();
    let (_, needs_llm) = fast_path(&b, &call.engine.context(), "בנלחב");
    assert!(needs_llm, "a doubtful correction goes to the LLM");
    let d = call.say("בנלחב");
    assert!(!spoken(&d).contains("בנלחב"), "{}", spoken(&d));
    assert!(spoken(&d).contains("לא בטוח שהבנתי") && spoken(&d).contains("לאן"), "{}", spoken(&d));
    assert_eq!(call.slot("destination"), None);
    assert_eq!(call.step(), Some(Step::Collecting { awaiting: Some("destination".into()) }));

    call.say("תל אביב");
    assert_eq!(place(call.slot("destination")), "תל אביב");
    assert_eq!(place(call.slot("pickup")), "רבי עקיבא 12", "other values survive");
}

#[test]
fn a_clear_correction_while_reading_back_a_value_is_taken() {
    let (mut call, _) = Call::new(business(&[]));
    call.say("צריך מונית");
    call.say("מרבי עקיבא 12");
    call.say("תלאבי");
    call.say("לא, לעזריאלי");
    assert_eq!(place(call.slot("destination")), "עזריאלי");
}

#[test]
fn fallback_ladder_then_handoff_with_context() {
    let (mut call, _) = Call::new(with_desk());
    let d1 = call.say("בלה בלה בלה");
    assert!(spoken(&d1).contains("לא בטוח שהבנתי"));
    let d2 = call.say("גלגל ענק ירוק");
    assert!(spoken(&d2).contains("להזמין מונית"));
    let d3 = call.say("פלפל שחור");
    assert!(spoken(&d3).contains("מעביר"));
    assert!(d3.iter().any(|d| matches!(d, Directive::Handoff { .. })));
}

#[test]
fn without_a_desk_a_bad_line_restarts_the_ladder_once_before_hanging_up() {
    let (mut call, _) = Call::new(business(&[]));
    let hangs_up = |d: &[Directive]| d.iter().any(|d| matches!(d, Directive::Hangup));
    call.say("בלה בלה בלה");
    call.say("גלגל ענק ירוק");
    let d = call.say("פלפל שחור");
    assert!(spoken(&d).contains("הקו קצת לא ברור"), "{}", spoken(&d));
    assert!(!hangs_up(&d), "the first time, the call goes on");
    assert_eq!(call.engine.state.fallback_level, 0);

    call.say("בלה בלה בלה");
    call.say("גלגל ענק ירוק");
    let d = call.say("פלפל שחור");
    assert!(spoken(&d).contains("אין מוקדן פנוי"), "{}", spoken(&d));
    assert!(hangs_up(&d), "the second time, it ends politely");
}

#[test]
fn recognizer_keyterms_cover_configured_words_and_known_places() {
    let terms = business(&[]).stt_keyterms();
    let unique: std::collections::HashSet<_> = terms.iter().collect();
    assert_eq!(unique.len(), terms.len(), "no duplicates");
    // Scribe takes the first 50: every configured word and every place name must be in them.
    let first = &terms[..terms.len().min(50)];
    for t in ["באר שבע", "בני ברק", "ז'בוטינסקי", "נתב״ג", "עזריאלי", "תחנה מרכזית"]
    {
        assert!(first.iter().any(|x| x == t), "{t} in the first 50: {first:?}");
    }
    assert!(terms.iter().any(|x| x == "שיבא"), "aliases come after");
}

#[test]
fn handoff_carries_collected_context() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג");
    let d = call.say("אני רוצה לדבר עם נציג");
    let summary = d
        .iter()
        .find_map(|d| match d {
            Directive::Handoff { summary } => Some(summary.clone()),
            _ => None,
        })
        .expect("handoff");
    assert_eq!(summary.reason, "caller_requested");
    assert!(summary.text.contains("כתובת איסוף: רבי עקיבא 12"), "{}", summary.text);
    assert!(summary.text.contains("יעד: נתב״ג"), "{}", summary.text);
}

#[test]
fn no_desk_means_no_transfer() {
    let (mut call, _) = Call::new(business(&[]));
    let d = call.say("נציג בבקשה");
    assert!(spoken(&d).contains("אין מוקדן פנוי"));
    assert!(!d.iter().any(|d| matches!(d, Directive::Handoff { .. })));
}

#[test]
fn action_failure_speaks_and_eventually_hands_off() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג, אנחנו שניים");
    let (run_id, ..) = action(&call.say("כן")).unwrap();
    let d = call.engine.on_action_result(run_id, Err("timeout".into()));
    assert!(spoken(&d).contains("אין נהג פנוי"));
    assert_eq!(call.engine.state.action_failures, 1);

    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג, אנחנו שניים");
    let (run_id, ..) = action(&call.say("כן")).unwrap();
    let d = call.engine.on_action_result(run_id, Err("timeout".into()));
    assert!(d.iter().any(|d| matches!(d, Directive::Handoff { .. })), "second failure hands off: {d:?}");
}

#[test]
fn known_customer_home_alias_and_default_pickup() {
    let b = with_desk();
    let mut engine = Engine::new(b.clone(), 3);
    let mut customer = Customer { name: Some("בניהו".into()), ..Default::default() };
    customer
        .places
        .insert("home".into(), CustomerPlace { spoken: "הבית".into(), address: Some("הרצל 10, בני ברק".into()) });
    engine.set_customer(Some(customer));
    let greeting = engine.start();
    assert_eq!(spoken(&greeting), "אהלן בניהו, איך אפשר לעזור?");

    let mut call = Call { engine };
    let d = call.say("צריך מונית לנתב\"ג, אני לבד");
    // Pickup comes from the customer record; the read-back lets the caller correct it.
    assert!(spoken(&d).contains("נוסע אחד מהבית לנתב״ג"), "{}", spoken(&d));
}

#[test]
fn price_question_then_booking_carries_the_destination() {
    let (mut call, _) = Call::new(with_desk());
    call.say("כמה עולה נסיעה לנתב\"ג?");
    let (run_id, name, _) = action(&call.say("מרבי עקיבא 12")).expect("estimate runs once both places are known");
    assert_eq!(name, "estimate_price");
    let d = call.engine.on_action_result(run_id, Ok(serde_json::json!({ "price": 82 })));
    assert!(spoken(&d).contains("שמונים ושניים שקלים"), "{}", spoken(&d));

    call.say("אוקיי תזמין לי מונית");
    assert_eq!(place(call.slot("destination")), "נתב״ג");
    assert_eq!(place(call.slot("pickup")), "רבי עקיבא 12");
}

#[test]
fn business_rule_sets_a_van_for_large_groups() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג, אנחנו שישה");
    assert_eq!(call.slot("vehicle"), Some(SlotValue::Enum { value: "van".into() }));
}

#[test]
fn faq_mid_flow_answers_then_resumes() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית");
    let d = call.say("רגע, אתם עובדים בשבת? מה שעות הפעילות?");
    let text = spoken(&d);
    assert!(text.contains("עשרים וארבע"), "{text}");
    assert!(text.contains("לאסוף") || text.contains("אוספים"), "resumes the pending question: {text}");
}

#[test]
fn voice_library_is_mostly_pregenerated() {
    let b = with_desk();
    let entries = library_entries(&b);
    assert!(entries.iter().any(|e| e.text == "מצאתי. יש נהג בערך ארבע דקות ממך."));
    assert!(entries.iter().any(|e| e.text == "זה יוצא בערך שמונים ושניים שקלים."));
    assert!(entries.iter().any(|e| e.text == "מאיפה לאסוף אותך?" && e.delivery == "slow"));
    assert!(!entries.iter().any(|e| e.text.contains('{')));
}

#[test]
fn validation_reports_broken_references() {
    let mut config: BusinessConfig = serde_json::from_str(TAXI).unwrap();
    config.greeting = "nope".into();
    config.pipelines.get_mut("book_ride").unwrap().action = Some("missing_action".into());
    config.intents[0].pipeline = Some("ghost".into());
    let issues = validate(&config);
    let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    assert!(paths.contains(&"greeting"), "{paths:?}");
    assert!(paths.contains(&"pipelines.book_ride.action"), "{paths:?}");
    assert!(paths.iter().any(|p| p.starts_with("intents[0]")), "{paths:?}");
}
