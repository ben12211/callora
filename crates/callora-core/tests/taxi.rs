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
    // No arrival time is promised: the driver calls the customer.
    assert!(text.contains("נהג מתאים") && !text.contains("דקות ממך"), "{text}");
    assert!(call.engine.state.run.is_none());
    // The confirmation is pre-recorded, not dynamic TTS.
    let Directive::Speak { plan, .. } = &d[0] else { panic!() };
    assert_ne!(plan.segments[0].origin, SegmentOrigin::Dynamic);
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
fn wait_says_so_changes_nothing_and_waits_longer() {
    // "רגע" left the caller in a silence nobody ended: no reply, no silence reprompt.
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית");
    let before = call.engine.state.run.clone();
    let d = call.say("רגע");
    assert!(spoken(&d).contains("אני מחכה"), "{}", spoken(&d));
    assert_eq!(call.engine.state.run, before);
    assert_eq!(call.engine.silence_after_ms(), 15_000, "a longer wait before \"שומעים אותי?\"");
    let d = call.engine.on_silence();
    assert!(spoken(&d).contains("שומעים אותי"), "{}", spoken(&d));
    assert_eq!(call.engine.silence_after_ms(), 5_000, "then the usual wait");
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
        phrase: None,
        say: say.into(),
        action,
        task: task.map(Into::into),
        fields: fields.iter().map(|(s, v)| (s.to_string(), v.to_string())).collect(),
        asks: Vec::new(),
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
    let fields =
        [("pickup", "רבי עקיבא 12"), ("destination", "תל אביב"), ("passengers", "אחד"), ("notes", "יש מזוודה")];
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
fn the_read_back_is_not_acknowledged_twice() {
    // A live call said "סגור. סגור. שבעה נוסעים...": the agent's lead-in plus the read-back's own.
    let (mut call, _) = Call::new(business(&[]));
    let fields = [("pickup", "אלעד"), ("destination", "תל אביב"), ("passengers", "שבע"), ("notes", "יש מזוודה")];
    let d = call.engine.on_agent_turn(
        "מאלעד לתל אביב, שבע",
        decide(AgentAction::ReadBack, "סגור.", Some("book_ride"), &fields),
        "",
    );
    let text = spoken(&d);
    assert!(text.contains("לשלוח?"), "{text}");
    let acks = ["סגור.", "אוקיי.", "מעולה.", "הבנתי.", "סבבה."].iter().map(|a| text.matches(a).count()).sum::<usize>();
    assert_eq!(acks, 1, "one acknowledgement: {text}");
}

#[test]
fn a_submit_does_not_say_checking_twice() {
    // From a live call: "שנייה, אני בודק. רגע, בודק." on a ride status check.
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn(
        "יש את המונית?",
        decide(AgentAction::Submit, "שנייה, אני בודק.", Some("ride_status"), &[]),
        "",
    );
    let text = spoken(&d);
    assert!(action(&d).is_some(), "the status check runs: {d:?}");
    assert_eq!(text.matches("בודק").count(), 1, "one filler: {text}");
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
    assert!(next.user.contains("passengers \"42\" was not accepted; ask for it again"), "{}", next.user);
    call.engine.on_agent_turn("ארבעה", decide(AgentAction::None, "כמה נוסעים?", None, &[("passengers", "ארבעה")]), "");
    assert_eq!(call.slot("passengers"), Some(SlotValue::Integer { value: 4 }));
    assert!(call.engine.state.agent_notes.is_empty(), "the note was delivered once");
}

#[test]
fn places_are_checked_against_the_list_of_israeli_streets() {
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "8600\tרמת גן\t205\tז'בוטינסקי\tofficial\n8600\tרמת גן\t205\tזבוטינסקי\tsynonym\n1309\tאלעד\t110\tרבי עקיבא\tofficial\n",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "מזבוטינסקי 5 ברמת גן למיל״ד",
        decide(
            AgentAction::None,
            "כמה נוסעים?",
            Some("book_ride"),
            &[("pickup", "זבוטינסקי 5, רמת גן"), ("destination", "מיל״ד")],
        ),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "ז'בוטינסקי 5, רמת גן", "the official spelling");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "כן");
    // "מיל״ד" is no locality: not taken as the destination, the caller is asked for its city.
    assert_eq!(call.slot("destination"), None);
    assert!(next.user.contains("is no place the system knows") && next.user.contains("אלעד"), "{}", next.user);
}

#[test]
fn a_city_alone_is_not_a_pickup() {
    // From a live call: dispatch got "אלעד" as the pickup, with no street.
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבי עקיבא\tofficial\n9000\tבאר שבע\t120\tרגר\tofficial\n",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "מאלעד לבאר שבע",
        decide(AgentAction::None, "כמה נוסעים?", Some("book_ride"), &[("pickup", "אלעד"), ("destination", "באר שבע")]),
        "",
    );
    assert_eq!(call.slot("pickup"), None, "a city alone does not fill the pickup");
    assert_eq!(call.slot("destination"), None, "the destination's street is asked for once");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "שבע");
    assert!(next.user.contains("pickup city אלעד is noted; now ask for the street"), "{}", next.user);
    assert!(next.user.contains("- destination: city באר שבע, street MISSING (ask once"), "{}", next.user);

    call.engine.on_agent_turn(
        "רבי עקיבא 12",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("pickup", "רבי עקיבא 12, אלעד")]),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "רבי עקיבא 12, אלעד");

    // "לאיזה רחוב צריך להגיע?" "לא יודע": the city is enough.
    call.engine.on_agent_turn(
        "לא יודע",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "באר שבע")]),
        "",
    );
    assert_eq!(place(call.slot("destination")), "באר שבע", "a destination may be a city, once asked");
}

#[test]
fn a_numbered_street_the_city_does_not_have_is_asked_again_once() {
    // From a live call: "בית דחה 45" in אלעד went to dispatch; אלעד has no such street.
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבינו בחיי\tofficial\n1309\tאלעד\t111\tרבי עקיבא\tofficial\n",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "אלעד",
        decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", Some("book_ride"), &[("pickup", "אלעד")]),
        "",
    );
    call.engine.on_agent_turn(
        "בית דחה 45",
        decide(AgentAction::None, "לאיזו עיר נוסעים?", None, &[("pickup", "בית דחה 45, אלעד")]),
        "",
    );
    assert_eq!(call.slot("pickup"), None, "not a street of אלעד");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "בית דחה 45");
    assert!(next.user.contains("has no street \"בית דחה\"; it was not taken"), "{}", next.user);
    assert!(next.user.contains("- pickup: city אלעד, street MISSING"), "{}", next.user);

    // Said again the same way: kept, the list may be missing it.
    call.engine.on_agent_turn(
        "בית דחה 45",
        decide(AgentAction::None, "לאיזו עיר נוסעים?", None, &[("pickup", "בית דחה 45")]),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "בית דחה 45");
}

#[test]
fn a_default_detail_is_shown_as_its_default() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn("רוצה מונית", decide(AgentAction::None, "מאיזו עיר לאסוף?", Some("book_ride"), &[]), "");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "אלעד");
    assert!(next.user.contains("- pickup_time: now (default; do not ask)"), "{}", next.user);
}

#[test]
fn city_first_then_street_builds_one_address() {
    // The order the owner asked for: "מאיזו עיר לאסוף?" "אלעד" ... "מאיזה רחוב ומספר לאסוף?" "בן זכאי 45".
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         2066\tבן זכאי\t9000\tבן זכאי\tofficial\n9000\tבאר שבע\t120\tרגר\tofficial\n",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "מאלעד",
        decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", Some("book_ride"), &[("pickup", "אלעד")]),
        "",
    );
    assert_eq!(call.slot("pickup"), None);
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "בן זכאי 45");
    assert!(next.user.contains("- pickup: city אלעד, street MISSING"), "{}", next.user);

    call.engine.on_agent_turn(
        "בן זכאי 45",
        decide(AgentAction::None, "לאיזו עיר נוסעים?", None, &[("pickup", "בן זכאי 45")]),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "בן זכאי 45, אלעד", "the street, in the city given before");

    call.engine.on_agent_turn(
        "לבאר שבע",
        decide(AgentAction::None, "לאיזה רחוב צריך להגיע?", None, &[("destination", "באר שבע")]),
        "",
    );
    call.engine.on_agent_turn(
        "רגר 10",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "רגר 10")]),
        "",
    );
    assert_eq!(place(call.slot("destination")), "רגר 10, באר שבע", "a destination street joins its city too");
}

#[test]
fn a_street_the_caller_never_said_is_not_booked() {
    // From a live call: "אההה, 42." was booked as "רחוב אהרונוביץ' 42, בני ברק".
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "לבני ברק",
        decide(AgentAction::None, "לאיזה רחוב צריך להגיע?", Some("book_ride"), &[("destination", "בני ברק")]),
        "",
    );
    call.engine.on_agent_turn(
        "אההה, 42.",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "רחוב אהרונוביץ' 42, בני ברק")]),
        "",
    );
    assert_eq!(place(call.slot("destination")), "בני ברק", "the invented street is not stored");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "שלוש");
    assert!(next.user.contains("the caller never said"), "{}", next.user);
}

#[test]
fn a_booked_ride_leaves_an_order_card_with_name_and_phone() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_caller_phone(Some("+972501234567".into()));
    let fields = [
        ("pickup", "רבי עקיבא 12"),
        ("destination", "נתב״ג"),
        ("passengers", "שניים"),
        ("customer_name", "בן"),
        ("notes", "יש מזוודה גדולה"),
    ];
    call.engine.on_agent_turn(
        "מרבי עקיבא 12 לנתב״ג, שניים, על שם בן, יש מזוודה גדולה",
        decide(AgentAction::ReadBack, "סגור.", Some("book_ride"), &fields),
        "",
    );
    let d = call.engine.on_agent_turn("כן", decide(AgentAction::Submit, "סגור.", None, &[]), "");
    let (run_id, _, input) = action(&d).expect("the booking runs");
    assert_eq!(input["caller_phone"], "+972501234567", "dispatch gets the caller's number");
    call.engine.on_action_result(run_id, Ok(serde_json::json!({ "ride_id": "R-7" })));

    let cards = callora_core::orders::order_cards(call.engine.business(), &call.engine.state);
    assert_eq!(cards.len(), 1);
    let summary = cards[0]["summary"].as_str().unwrap();
    for part in [
        "הזמנת מונית",
        "טלפון: +972501234567",
        "שם הנוסע: בן",
        "יעד: נמל התעופה בן גוריון",
        "מספר נוסעים: 2",
        "הערות לנהג: יש מזוודה גדולה",
    ] {
        assert!(summary.contains(part), "{part} in {summary}");
    }
    assert_eq!(cards[0]["result"]["ride_id"], "R-7");
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
    assert_eq!(spoken(&d), "אהלן, איך אפשר לעזור?", "the question again, not \"say it again?\"");
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
    assert_eq!(
        order,
        ["action", "fields", "asks", "phrase", "say", "task"],
        "the action, the fields and what the question asks, then the words: all are checked before a word plays"
    );
    // Phrases are offered by id, with their wording, and only those with nothing to fill in.
    assert!(system.contains("- ask_destination: \"לאן נוסעים?\""), "{system}");
    assert!(!system.contains("- ask_destination_street:"), "it names the city, so it is no instant phrase: {system}");
    let ids = request.schema["properties"]["phrase"]["enum"].as_array().unwrap();
    assert!(ids.contains(&serde_json::json!("ask_name")) && ids.contains(&serde_json::Value::Null));
    // Nothing of the taxi business is written in the generic prompt: its words come from its file.
    assert!(system.contains("experienced human dispatcher"), "the role comes from agent.prompt");
    assert!(
        system.contains("(luggage, wheelchair, child_seat, vehicle, round_trip, asks_about)"),
        "optional details come from the pipelines: {system}"
    );
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
    assert!(spoken(&d).contains("רעש בקו") && spoken(&d).contains("לאן"), "{}", spoken(&d));
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
    assert!(spoken(&d1).contains("רעש בקו"), "a reason for asking again: {}", spoken(&d1));
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
    assert!(spoken(&d).contains("איך אפשר לעזור"), "{}", spoken(&d));
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
fn a_price_is_asked_of_the_price_list_with_the_cities_alone() {
    let (mut call, _) = Call::new(with_desk());
    let d = call.engine.on_agent_turn(
        "כמה עולה מבני ברק לירושלים?",
        decide(AgentAction::Submit, "", Some("price_question"), &[("price_from", "בני ברק"), ("price_to", "ירושלים")]),
        "",
    );
    let (run_id, name, input) = action(&d).expect("the price is asked at once: no street needed");
    assert_eq!(name, "estimate_price");
    assert_eq!(input["slots"]["price_from"]["spoken"], "בני ברק");
    assert!(spoken(&d).contains("בודק"), "the filler while the list is asked: {}", spoken(&d));
    // Who is coming is not known: the price by car size, as the result names it.
    let quote = serde_json::json!({ "price": 220, "price_6": 300, "response": "price_answer_sizes" });
    let said = spoken(&call.engine.on_action_result(run_id, Ok(quote)));
    assert!(said.contains("עד ארבעה נוסעים 220₪, ועד שישה 300₪."), "{said}");
    assert!(
        said.contains("להזמין מונית?") || said.contains("לשלוח מונית?"),
        "a booking is offered, not \"anything else?\": {said}"
    );
}

#[test]
fn a_price_asked_during_a_booking_goes_back_to_the_booking() {
    let (mut call, _) = Call::new(with_desk());
    call.engine.set_gazetteer(Some(elad()));
    call.engine.on_agent_turn(
        "מבן זכאי 40 באלעד לסוכות 12 בירושלים",
        decide(
            AgentAction::None,
            "כמה נוסעים?",
            Some("book_ride"),
            &[("pickup", "בן זכאי 40, אלעד"), ("destination", "סוכות 12, ירושלים")],
        ),
        "",
    );
    let d = call.engine.on_agent_turn(
        "רגע כמה זה עולה?",
        decide(AgentAction::Submit, "", Some("price_question"), &[("price_from", "אלעד"), ("price_to", "ירושלים")]),
        "",
    );
    let (run_id, _, _) = action(&d).expect("the price is asked");
    let d = call.engine.on_action_result(run_id, Ok(serde_json::json!({ "price": 180, "response": "price_answer" })));
    let said = spoken(&d);
    assert!(
        said.contains("180₪") && (said.contains("נוסעים") || said.contains("אתם")),
        "the price, then the booking's question: {said}"
    );
    assert_eq!(place(call.slot("pickup")), "בן זכאי 40, אלעד", "the booking is kept");
}

#[test]
fn a_price_list_that_does_not_answer_sends_no_one_to_the_desk() {
    let (mut call, _) = Call::new(with_desk());
    for _ in 0..3 {
        let d = call.engine.on_agent_turn(
            "כמה עולה מבני ברק לירושלים?",
            decide(
                AgentAction::Submit,
                "",
                Some("price_question"),
                &[("price_from", "בני ברק"), ("price_to", "ירושלים")],
            ),
            "",
        );
        let (run_id, _, _) = action(&d).expect("the price is asked");
        let d = call.engine.on_action_result(run_id, Err("the price bot: no answer in time".into()));
        assert!(spoken(&d).contains("אין לי כרגע מחיר"), "{}", spoken(&d));
        assert!(!d.iter().any(|d| matches!(d, Directive::Handoff { .. } | Directive::Hangup)), "{d:?}");
    }
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
    assert!(entries.iter().any(|e| e.text == "סבבה, קיבלנו את הפרטים. נחפש נהג מתאים, והוא יתקשר בדקות הקרובות."));
    // A price is said live: any sum, from the price list.
    assert!(!entries.iter().any(|e| e.response_id == "price_answer"));
    assert!(entries.iter().any(|e| e.text == "מאיפה לאסוף?" && e.delivery == "slow"));
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

#[test]
fn the_address_form_follows_what_the_caller_says_about_themselves() {
    use callora_core::address_form::AddressForm;
    let (mut call, _) = Call::new(business(&[]));
    let b = call.engine.business().clone();
    assert_eq!(call.engine.state.address_form, AddressForm::Unknown);
    assert_eq!(serde_json::to_value(&call.engine.state).unwrap()["address_form"], "unknown");
    let neutral = callora_core::agent::build_request(&b, &call.engine.state, "צריך מונית");
    assert!(neutral.user.contains("ADDRESS FORM: unknown. Speak gender-neutral Hebrew"), "{}", neutral.user);

    // Impersonal "צריך" says nothing; "אני צריכה" does, and it stays for the rest of the call.
    call.engine.on_agent_turn("צריך מונית", decide(AgentAction::None, "מאיזו עיר לאסוף?", Some("book_ride"), &[]), "");
    assert_eq!(call.engine.state.address_form, AddressForm::Unknown);
    call.engine.on_agent_turn(
        "מרעננה, אני צריכה מונית",
        decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", None, &[]),
        "",
    );
    assert_eq!(call.engine.state.address_form, AddressForm::Feminine);
    call.engine.on_agent_turn("אחוזה 12", decide(AgentAction::None, "לאיזו עיר נוסעים?", None, &[]), "");
    assert_eq!(call.engine.state.address_form, AddressForm::Feminine, "kept when nothing new is said");
    let feminine = callora_core::agent::build_request(&b, &call.engine.state, "לתל אביב");
    assert!(feminine.user.contains("ADDRESS FORM: feminine"), "{}", feminine.user);

    // A correction wins.
    call.engine.on_agent_turn(
        "סליחה, אני מתכוון לתל אביב",
        decide(AgentAction::None, "לאיזה רחוב צריך להגיע?", None, &[]),
        "",
    );
    assert_eq!(call.engine.state.address_form, AddressForm::Masculine);
    // The rules path listens too.
    call.say("אני אישה, דברו אליי בלשון נקבה");
    assert_eq!(call.engine.state.address_form, AddressForm::Feminine);
}

#[test]
fn fixed_phrases_are_neutral_until_the_form_is_known() {
    // Every recorded sentence is said to callers of either form.
    const GENDERED: &[&str] = &[
        "אתה",
        "לך",
        "אליך",
        "אותך",
        "שלך",
        "איתך",
        "בשבילך",
        "ממך",
        "עליך",
        "תרצה",
        "תרצי",
        "תגיד",
        "תגידי",
        "תוכל",
        "תוכלי",
        "שכחת",
        "הזמנת",
        "רצית",
        "אמרת",
        "ביקשת",
    ];
    let b = business(&[]);
    for (id, r) in &b.config.responses {
        let mut texts: Vec<String> = r.variants.clone();
        for p in r.params.values() {
            if let Some(values) = serde_json::to_value(p).ok().and_then(|v| v.get("values").cloned()) {
                texts.extend(
                    values.as_object().into_iter().flatten().filter_map(|(_, v)| v.as_str().map(str::to_string)),
                );
            }
        }
        for text in texts {
            let norm = callora_core::text::normalize(&text);
            for word in norm.split(' ') {
                assert!(!GENDERED.contains(&word), "{id}: \"{text}\" says \"{word}\"");
            }
        }
    }
}

#[test]
fn live_speech_is_pronounced_in_the_callers_form() {
    use callora_core::address_form::AddressForm;
    let b = business(&[]);
    let say =
        |form| callora_core::speech::prepare_for_tts("נחפש לך נהג, והוא יתקשר אליך", "he", b.pronouncer_for(form));
    assert!(say(AddressForm::Feminine).contains("לָךְ") && say(AddressForm::Feminine).contains("אֵלַיִךְ"));
    assert!(say(AddressForm::Masculine).contains("לְךָ"));
    assert!(say(AddressForm::Unknown).contains("לְךָ"), "masculine when one slips into neutral speech");
}

fn elad() -> Arc<callora_core::gazetteer::Gazetteer> {
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         2066\tבן זכאי\t9000\tבן זכאי\tofficial\n3000\tירושלים\t120\tסוכות\tofficial\n",
    ))
}

/// A ride read back and waiting for the caller's yes.
fn read_back_ride() -> Call {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad()));
    call.engine.on_agent_turn(
        "בן זכאי 40 אלעד לסוכות ירושלים, שלושה",
        decide(
            AgentAction::ReadBack,
            "סגור.",
            Some("book_ride"),
            &[
                ("pickup", "בן זכאי 40, אלעד"),
                ("destination", "סוכות 12, ירושלים"),
                ("passengers", "שלושה"),
                ("notes", "יש מזוודה"),
            ],
        ),
        "",
    );
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation));
    call
}

#[test]
fn a_house_number_corrected_after_the_read_back_is_taken_in_the_same_city() {
    // From a live call: "לא 40, 45" was looked up in "רבן יוחנן בן זכאי 40, אלעד" as a city,
    // rejected, and the old 40 was read back again.
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn(
        "אהה, לא 40, 45",
        decide(AgentAction::ReadBack, "סגור.", None, &[("pickup", "בן זכאי 45")]),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "בן זכאי 45, אלעד");
    assert!(spoken(&d).contains("45") && !spoken(&d).contains("40"), "{}", spoken(&d));
    let d = call.engine.on_agent_turn("יאללה", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_some(), "the corrected ride is sent: {}", spoken(&d));
}

#[test]
fn a_correction_without_a_read_back_is_read_back_so_the_next_yes_sends() {
    // From a live call: the agent asked "אוקיי, בן זכאי 45, אלעד. לשלוח?" itself; "יאללה" then
    // met a second read-back ("מה יש לך? אמרתי יאללה").
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn(
        "בן זכאי 45, לא בן זכאי 40",
        decide(AgentAction::None, "אוקיי, בן זכאי 45, אלעד. לשלוח?", None, &[("pickup", "בן זכאי 45, אלעד")]),
        "",
    );
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation), "{}", spoken(&d));
    let d = call.engine.on_agent_turn("יאללה", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_some(), "sent on the first yes: {}", spoken(&d));
}

#[test]
fn a_detail_rejected_in_a_read_back_turn_is_asked_for_not_read_back() {
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn(
        "לא, מבית דחה 45",
        decide(AgentAction::ReadBack, "סגור.", None, &[("pickup", "בית דחה 45, אלעד")]),
        "",
    );
    let said = spoken(&d);
    assert!(!said.contains("לשלוח"), "no read-back of the old pickup: {said}");
    assert!(said.contains("לא מצאתי את בית דחה באלעד"), "what was not found, and where: {said}");
    assert!(!said.contains("סגור"), "{said}");
}

#[test]
fn a_known_place_is_taken_and_an_unknown_one_is_asked_about_once() {
    let mut gazetteer = callora_core::gazetteer::Gazetteer::from_tsv("3000\tירושלים\t120\tשדרות שזר\tofficial\n");
    gazetteer.add_places("ירושלים\tבנייני האומה\tבנייני אומה\tשדרות שזר\t1\t31.78570\t35.20160\n");
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "לירושלים",
        decide(AgentAction::None, "לאיזה רחוב צריך להגיע?", Some("book_ride"), &[("destination", "ירושלים")]),
        "",
    );
    call.engine.on_agent_turn(
        "לבנייני האומה",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "בנייני האומה, ירושלים")]),
        "",
    );
    match call.slot("destination") {
        Some(SlotValue::Place { spoken, address, .. }) => {
            assert_eq!(spoken, "בנייני האומה, ירושלים");
            assert_eq!(address.as_deref(), Some("בנייני האומה, שדרות שזר 1, ירושלים (31.78570,35.20160)"));
        }
        other => panic!("{other:?}"),
    }

    // Not on the list: "יש כתובת של המקום?" once, then taken as said, marked for the driver.
    call.engine.on_agent_turn(
        "לא, לקניון הזהב",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "קניון הזהב, ירושלים")]),
        "",
    );
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "לא יודע");
    assert!(next.user.contains("is not a street or a known place in ירושלים"), "{}", next.user);
    assert!(next.user.contains("Ask for its address (phrase ask_place_address)"), "{}", next.user);
    call.engine.on_agent_turn(
        "לא יודע",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "קניון הזהב, ירושלים")]),
        "",
    );
    match call.slot("destination") {
        Some(SlotValue::Place { spoken, address, .. }) => {
            assert_eq!(spoken, "קניון הזהב, ירושלים");
            assert!(address.as_deref().is_some_and(|a| a.contains("מקום לא מאומת")), "{address:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_note_for_the_driver_is_asked_before_the_read_back_when_the_agent_skips_it() {
    let (mut call, _) = Call::new(business(&[]));
    let fields =
        [("pickup", "רבי עקיבא 12"), ("destination", "נתב״ג"), ("passengers", "שניים"), ("customer_name", "שלומית")];
    let d = call.engine.on_agent_turn(
        "מרבי עקיבא 12 לנתב״ג, שניים, על שם שלומית",
        decide(AgentAction::ReadBack, "סגור.", Some("book_ride"), &fields),
        "",
    );
    assert!(spoken(&d).contains("הנהג") || spoken(&d).contains("הערה"), "the note first: {}", spoken(&d));
    assert!(!spoken(&d).contains("לשלוח"), "{}", spoken(&d));
    // Answered (or "אין"): the read-back follows, and the question is not asked again.
    let d = call.engine.on_agent_turn("אין", decide(AgentAction::ReadBack, "סגור.", None, &[]), "");
    assert!(spoken(&d).contains("לשלוח"), "{}", spoken(&d));
}

#[test]
fn a_note_question_the_agent_asked_itself_is_not_asked_again() {
    let (mut call, _) = Call::new(business(&[]));
    let fields = [("pickup", "רבי עקיבא 12"), ("destination", "נתב״ג"), ("passengers", "שניים")];
    call.engine.on_agent_turn(
        "מרבי עקיבא 12 לנתב״ג, שניים",
        decide(AgentAction::None, "יש משהו שהנהג צריך לדעת?", Some("book_ride"), &fields),
        "",
    );
    let d = call.engine.on_agent_turn("לא", decide(AgentAction::ReadBack, "סגור.", None, &[]), "");
    assert!(spoken(&d).contains("לשלוח"), "{}", spoken(&d));
}

#[test]
fn the_city_waiting_for_its_street_is_the_recognition_focus() {
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "6100\tבני ברק\t301\tאהרונוביץ\tofficial\n1309\tאלעד\t110\tרבי עקיבא\tofficial\n",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    assert_eq!(call.engine.street_focus(), None);
    call.engine.on_agent_turn(
        "מאלעד",
        asking(
            &["pickup"],
            decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", Some("book_ride"), &[("pickup", "אלעד")]),
        ),
        "",
    );
    assert_eq!(call.engine.street_focus().as_deref(), Some("אלעד"));
    call.engine.on_agent_turn(
        "רבי עקיבא 3",
        asking(
            &["destination"],
            decide(AgentAction::None, "לאיזו עיר נוסעים?", None, &[("pickup", "רבי עקיבא 3, אלעד")]),
        ),
        "",
    );
    assert_eq!(call.engine.street_focus(), None);
    call.engine.on_agent_turn(
        "בני ברק",
        asking(
            &["destination"],
            decide(AgentAction::None, "לאיזה רחוב צריך להגיע?", None, &[("destination", "בני ברק")]),
        ),
        "",
    );
    assert_eq!(call.engine.street_focus().as_deref(), Some("בני ברק"));
    // Once the question is about something else, the streets stop biasing recognition: a
    // live call heard the caller's name as a street of the city.
    call.engine.on_agent_turn(
        "אהרונוביץ 5",
        asking(
            &["passengers"],
            decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "אהרונוביץ 5, בני ברק")]),
        ),
        "",
    );
    assert_eq!(call.engine.street_focus(), None);
}

#[test]
fn an_answer_to_one_question_does_not_change_another_detail() {
    // The live call: asked for the name, "בן איוב" was heard and passed as the destination.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מבן זכאי 45 באלעד לירושלים, שניים",
        asking(
            &["customer_name"],
            decide(
                AgentAction::None,
                "על שם מי לרשום את ההזמנה?",
                Some("book_ride"),
                &[("pickup", "בן זכאי 45, אלעד"), ("destination", "ירושלים"), ("passengers", "2")],
            ),
        ),
        "",
    );
    let before = place(call.slot("destination"));
    call.engine.on_agent_turn(
        "בן איוב",
        asking(
            &["notes"],
            decide(AgentAction::None, "יש משהו שהנהג צריך לדעת?", None, &[("destination", "בן איוב, ירושלים")]),
        ),
        "",
    );
    assert_eq!(place(call.slot("destination")), before, "the destination stays");
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "בן");
    assert!(next.user.contains("destination is already"), "the agent is told: {}", next.user);

    // A correction the caller says as one is taken.
    call.engine.on_agent_turn(
        "לא, לבני ברק",
        asking(&["notes"], decide(AgentAction::None, "יש משהו שהנהג צריך לדעת?", None, &[("destination", "בני ברק")])),
        "",
    );
    assert_eq!(place(call.slot("destination")), "בני ברק");
}

#[test]
fn the_agent_is_told_the_street_question_comes_after_a_city() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מרבי עקיבא 12",
        decide(AgentAction::None, "לאיזו עיר נוסעים?", Some("book_ride"), &[("pickup", "רבי עקיבא 12")]),
        "",
    );
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "ירושלים");
    assert!(
        next.user.contains("NOW: the caller is giving the destination city")
            && next.user.contains("\"לאן ב<the city>?\""),
        "{}",
        next.user
    );
}

#[test]
fn a_made_up_word_after_the_read_back_does_not_send_the_ride() {
    // From a live call: recognition wrote "שעמות", the agent submitted a wrong ride.
    let (mut call, _) = Call::new(business(&[]));
    let fields =
        [("pickup", "רבי עקיבא 12"), ("destination", "תל אביב"), ("passengers", "אחד"), ("notes", "יש מזוודה")];
    call.engine.on_agent_turn(
        "מרבי עקיבא 12 לתל אביב",
        decide(AgentAction::ReadBack, "סגור.", Some("book_ride"), &fields),
        "",
    );
    let d = call.engine.on_agent_turn("שעמות", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_none(), "not sent: {}", spoken(&d));
    let d = call.engine.on_agent_turn("כן", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_some(), "sent on the yes");
}

#[test]
fn the_same_question_over_and_over_is_rephrased_then_handed_off() {
    // From a live call: "כמה נוסעים?" eleven times while the caller answered names.
    let (mut call, _) = Call::new(with_desk());
    call.engine.on_agent_turn("מונית", decide(AgentAction::None, "כמה נוסעים?", Some("book_ride"), &[]), "");
    call.engine.on_agent_turn("עומר", decide(AgentAction::None, "כמה נוסעים?", None, &[]), "");
    let b = call.engine.business().clone();
    assert!(!callora_core::agent::build_request(&b, &call.engine.state, "x").user.contains("STUCK"));
    call.engine.on_agent_turn("עומר", decide(AgentAction::None, "כמה נוסעים?", None, &[]), "");
    let next = callora_core::agent::build_request(&b, &call.engine.state, "שוויצר");
    assert!(next.user.contains("STUCK: you asked the same question 3 times"), "{}", next.user);
    call.engine.on_agent_turn("שוויצר", decide(AgentAction::None, "כמה נוסעים?", None, &[]), "");
    call.engine.on_agent_turn("שוויצר", decide(AgentAction::None, "כמה נוסעים?", None, &[]), "");
    let d = call.engine.on_agent_turn("74", decide(AgentAction::None, "כמה נוסעים?", None, &[]), "");
    assert!(d.iter().any(|d| matches!(d, Directive::Handoff { .. })), "{d:?}");
}

#[test]
fn a_place_rejected_as_unheard_twice_is_taken_the_third_time() {
    // A live call looped: the check kept rejecting the agent's (right) landmark, the caller
    // said "אמרתי כבר" three times and hung up.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn("מונית", decide(AgentAction::None, "מאיזו עיר לאסוף?", Some("book_ride"), &[]), "");
    for heard in ["זה ליד הבית של דודה", "אמרתי כבר"] {
        call.engine.on_agent_turn(
            heard,
            decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "מגדל שלום, תל אביב")]),
            "",
        );
        assert_eq!(call.slot("destination"), None, "{heard}");
    }
    call.engine.on_agent_turn(
        "אמרתי גם",
        decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "מגדל שלום, תל אביב")]),
        "",
    );
    assert!(call.slot("destination").is_some(), "taken, not asked for a fourth time");
}

#[test]
fn the_second_hearing_reaches_the_agent_and_counts_as_heard() {
    // The stream heard "זה יותר"; the second hearing, hinted with the towns, "זה ביתר".
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn("מונית", decide(AgentAction::None, "מאיזו עיר לאסוף?", Some("book_ride"), &[]), "");
    assert!(call.engine.awaiting_city());
    call.engine.state.second_hearing = Some("זה ביתר".into());
    let b = call.engine.business().clone();
    let next = callora_core::agent::build_request(&b, &call.engine.state, "זה יותר");
    assert!(
        next.user.contains("SECOND HEARING of the same words") && next.user.contains("\"זה ביתר\""),
        "{}",
        next.user
    );
    call.engine.on_agent_turn(
        "זה יותר",
        decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", None, &[("destination", "בית הכרם, ביתר")]),
        "",
    );
    assert!(
        !call.engine.state.agent_notes.iter().any(|n| n.contains("never said \"ביתר\"")),
        "{:?}",
        call.engine.state.agent_notes
    );
}

#[test]
fn a_goodbye_without_the_caller_s_goodbye_is_not_said() {
    // From a live call: a rude remark got "תודה, יום טוב!" although the call stayed open.
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn("מה אתה אוטיסט?", decide(AgentAction::EndCall, "תודה, יום טוב!", None, &[]), "");
    assert!(!spoken(&d).contains("יום טוב"), "{}", spoken(&d));
    assert!(spoken(&d).contains("לעזור"), "goes on: {}", spoken(&d));
    assert!(!d.iter().any(|d| matches!(d, Directive::Hangup)));
}

#[test]
fn an_impossible_value_is_asked_again_before_anything_else() {
    // From a live call: "47" passengers, the agent went on to the name, then back.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn("מונית", decide(AgentAction::None, "כמה נוסעים?", Some("book_ride"), &[]), "");
    let fields = vec![("passengers".to_string(), "47".to_string())];
    assert!(call.engine.rejects_any("ארבעים ושבע", &fields));
    assert!(!call.engine.rejects_any("שלושה", &[("passengers".to_string(), "3".to_string())]));
    // The runtime held the agent's "על שם מי לרשום את ההזמנה?": nothing was spoken.
    let d = call.engine.on_agent_turn(
        "47",
        decide(AgentAction::None, "על שם מי לרשום את ההזמנה?", None, &[("passengers", "47")]),
        "",
    );
    assert!(spoken(&d).contains("כמה נוסעים"), "{}", spoken(&d));
    assert!(!spoken(&d).contains("על שם"), "{}", spoken(&d));
}

#[test]
fn the_reply_s_fields_are_known_before_its_words() {
    let mut s = callora_core::agent::SayStream::default();
    s.push(r#"{"action":"none","fields":[{"slot":"passengers","value":"47"}],"sa"#);
    assert_eq!(s.fields(), None, "not before say begins");
    s.push(r#"y":"על שם"#);
    assert_eq!(s.fields(), Some(vec![("passengers".to_string(), "47".to_string())]));
}

#[test]
fn street_comma_city_from_the_agent_is_looked_up_in_that_city() {
    // "חיפה 32, ירושלים" was booked as "ירושלים 32, חיפה".
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "3000	ירושלים	10	חיפה	official
4000	חיפה	20	ירושלים	official
",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "רחוב חיפה 32 בירושלים",
        decide(AgentAction::None, "לאיזו עיר נוסעים?", Some("book_ride"), &[("pickup", "חיפה 32, ירושלים")]),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "חיפה 32, ירושלים");
}

#[test]
fn a_phrase_the_runtime_did_not_play_is_said_by_the_engine_and_never_before_a_read_back() {
    let (mut call, _) = Call::new(business(&[]));
    let mut turn = decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "הרצל 10, רעננה")]);
    turn.phrase = Some("ask_destination".into());
    let d = call.engine.on_agent_turn("מונית מהרצל 10 רעננה", turn, "");
    let text = spoken(&d);
    assert!(["לאן נוסעים?", "ולאן?", "לאן צריך להגיע?"].contains(&text.as_str()), "{text}");
    assert!(call.engine.state.history.last().is_some_and(|t| t.text == text), "what was said is remembered");

    // Everything known: a question phrase before the read-back would ask twice.
    call.engine.on_agent_turn(
        "לעזריאלי, שניים",
        decide(
            AgentAction::None,
            "",
            None,
            &[("destination", "עזריאלי"), ("passengers", "2"), ("customer_name", "דני")],
        ),
        "",
    );
    call.engine.on_agent_turn("אין", decide(AgentAction::None, "", None, &[("notes", "אין")]), "");
    let mut back = decide(AgentAction::ReadBack, "", None, &[]);
    back.phrase = Some("ask_passengers".into());
    let d = call.engine.on_agent_turn("זהו", back, "");
    let text = spoken(&d);
    assert!(!text.contains("כמה נוסעים"), "{text}");
    assert!(text.contains("לשלוח?"), "the read-back: {text}");
}

#[test]
fn a_ride_that_may_have_gone_through_is_checked_by_a_person_not_called_failed() {
    let (mut call, _) = Call::new(with_desk());
    call.say("צריך מונית מרבי עקיבא 12 לנתב\"ג, אנחנו שניים");
    let (run_id, _, input) = action(&call.say("כן")).unwrap();
    assert_eq!(input["run_id"], run_id, "every attempt carries its run, for the idempotency key");
    let d = call
        .engine
        .on_action_result(run_id, Err(callora_core::engine::ActionFailure::unknown("create_ride: timed out")));
    let text = spoken(&d);
    assert!(text.contains("שיוודא שההזמנה נקלטה"), "{text}");
    assert!(!text.contains("אין נהג פנוי"), "never \"failed\" for a ride that may be on its way: {text}");
    let summary = d.iter().find_map(|d| match d {
        Directive::Handoff { summary } => Some(summary.clone()),
        _ => None,
    });
    let summary = summary.expect("a person checks it");
    assert_eq!(summary.reason, "action_outcome_unknown");
    assert!(summary.text.contains("רבי עקיבא 12"), "the person gets the ride: {}", summary.text);
    assert_eq!(call.engine.state.action_failures, 0, "not counted as a failure");

    let cards = callora_core::orders::order_cards(call.engine.business(), &call.engine.state);
    assert_eq!(cards.len(), 1, "the owner sees it too");
    assert_eq!(cards[0]["verify"], true);
    assert!(cards[0]["summary"].as_str().unwrap().starts_with("לבדוק"), "{}", cards[0]["summary"]);
}

#[test]
fn a_question_the_caller_just_answered_is_not_asked_again() {
    // A live call: "בן זכאי, אה, 32." stored as the destination, and "לאיזה רחוב צריך להגיע?" again.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מונית מהרצל 10 רעננה",
        decide(AgentAction::None, "לאן נוסעים?", Some("book_ride"), &[("pickup", "הרצל 10, רעננה")]),
        "",
    );
    let mut again = decide(AgentAction::None, "", None, &[("destination", "עזריאלי")]);
    again.phrase = Some("ask_destination_street".into());
    let d = call.engine.on_agent_turn("לעזריאלי", again, "");
    let text = spoken(&d);
    assert!(!text.contains("רחוב"), "{text}");
    assert!(text.contains("נוסעים") || text.contains("כמה אתם"), "the next question: {text}");

    // A value that was not taken: the question stands.
    let mut still = decide(AgentAction::None, "", None, &[("passengers", "ארבעים ושבע")]);
    still.phrase = Some("ask_passengers".into());
    let d = call.engine.on_agent_turn("ארבעים ושבע", still, "");
    assert!(spoken(&d).contains("נוסעים"), "{}", spoken(&d));
}

fn asking(asks: &[&str], turn: AgentTurn) -> AgentTurn {
    AgentTurn { asks: asks.iter().map(|s| s.to_string()).collect(), ..turn }
}

fn elad_and_tel_aviv() -> Arc<callora_core::gazetteer::Gazetteer> {
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         5000\tתל אביב - יפו\t130\tדיזנגוף\tofficial\n",
    ))
}

#[test]
fn a_question_that_moves_on_past_an_unanswered_one_asks_it_again() {
    // Live calls: the street was not understood, the agent asked for the passengers, and the
    // street came back two questions later.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_and_tel_aviv()));
    call.engine.on_agent_turn(
        "מאלעד",
        asking(
            &["pickup"],
            decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", Some("book_ride"), &[("pickup", "אלעד")]),
        ),
        "",
    );
    let next = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "אנחנו שלושה");
    assert!(next.user.contains("OPEN QUESTION: you asked for pickup"), "{}", next.user);

    let d = call.engine.on_agent_turn(
        "אנחנו שלושה",
        asking(
            &["customer_name"],
            decide(AgentAction::None, "על שם מי לרשום את ההזמנה?", None, &[("passengers", "3")]),
        ),
        "",
    );
    let said = spoken(&d);
    assert_eq!(call.slot("passengers").map(|v| v.spoken()).as_deref(), Some("3"), "what they did say is kept");
    assert!(said.contains("איפה באלעד לאסוף"), "the street again, by its own question, in its city: {said}");
    assert!(!said.contains("על שם מי"), "not the next question: {said}");

    // The street given: the open question is closed, and the next question is the next in the
    // booking's order (the destination), not the name the agent asked for.
    let d = call.engine.on_agent_turn(
        "בן זכאי 45",
        asking(
            &["customer_name"],
            decide(AgentAction::None, "על שם מי לרשום את ההזמנה?", None, &[("pickup", "בן זכאי 45")]),
        ),
        "",
    );
    assert!(spoken(&d).contains("לאן") && !spoken(&d).contains("על שם מי"), "{}", spoken(&d));
    assert_eq!(
        callora_core::engine::open_questions(call.engine.business(), &call.engine.state),
        ["destination"],
        "the pickup's question is closed; the destination's is open"
    );
}

#[test]
fn a_question_that_keeps_one_open_detail_goes_on() {
    // "מאיפה לאן?" answered with the destination only: asking for the pickup is not moving on.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_and_tel_aviv()));
    call.engine.on_agent_turn(
        "צריך מונית",
        AgentTurn {
            phrase: Some("ask_route".into()),
            ..asking(&["pickup", "destination"], decide(AgentAction::None, "", Some("book_ride"), &[]))
        },
        "",
    );
    let d = call.engine.on_agent_turn(
        "לדיזנגוף 10 בתל אביב",
        asking(&["pickup"], decide(AgentAction::None, "מאיפה לאסוף?", None, &[("destination", "דיזנגוף 10, תל אביב")])),
        "",
    );
    assert!(spoken(&d).contains("מאיפה לאסוף"), "{}", spoken(&d));
}

#[test]
fn an_unknown_destination_street_lets_the_call_go_on() {
    // "לא יודע" to the destination's street: the city is enough, nothing stays open.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_and_tel_aviv()));
    call.engine.on_agent_turn(
        "מבן זכאי 45 באלעד לתל אביב",
        asking(
            &["destination"],
            decide(
                AgentAction::None,
                "לאיזה רחוב צריך להגיע?",
                Some("book_ride"),
                &[("pickup", "בן זכאי 45, אלעד"), ("destination", "תל אביב")],
            ),
        ),
        "",
    );
    let d = call.engine.on_agent_turn(
        "לא יודע",
        asking(&["passengers"], decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "תל אביב")])),
        "",
    );
    assert!(spoken(&d).contains("כמה נוסעים"), "{}", spoken(&d));
}

#[test]
fn a_street_its_city_does_not_have_holds_the_words_before_they_play() {
    // The live call: "מהשערה 18, אפרת" was refused only after the next question had played.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_and_tel_aviv()));
    call.engine.on_agent_turn(
        "מאלעד",
        asking(
            &["pickup"],
            decide(AgentAction::None, "מאיזה רחוב ומספר לאסוף?", Some("book_ride"), &[("pickup", "אלעד")]),
        ),
        "",
    );
    let street = |s: &str| vec![("pickup".to_string(), s.to_string())];
    assert!(call.engine.rejects_any("מהשערה 18", &street("השערה 18, אלעד")), "no such street in אלעד");
    assert!(!call.engine.rejects_any("בן זכאי 45", &street("בן זכאי 45, אלעד")));
    assert_eq!(call.slot("pickup"), None, "the check changes nothing");
}

#[test]
fn words_that_were_not_made_out_get_the_question_again_with_the_reason() {
    // The owner: the question once more, after why it is asked again ("יש רעש"), not a bare
    // "I didn't hear".
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מאלעד בן זכאי 45 לירושלים",
        decide(
            AgentAction::None,
            "סגור. כמה נוסעים?",
            Some("book_ride"),
            &[("pickup", "בן זכאי 45, אלעד"), ("destination", "ירושלים")],
        ),
        "",
    );
    let said = spoken(&call.engine.on_unheard());
    assert_eq!(said, "סליחה, יש קצת רעש בקו. כמה נוסעים?", "the question alone, without the \"סגור.\" before it");
    // Noise again in the same turn: no second apology; the silence reprompt waits.
    assert!(call.engine.on_unheard().is_empty());
    // Noise that cut the agent off: what it was saying, again.
    assert_eq!(spoken(&call.engine.on_noise(true)), "סגור. כמה נוסעים?");
}

#[test]
fn a_street_without_its_city_is_kept_while_the_city_is_asked() {
    // The live call of 18:05: "בן זכאי 45" with no city was taken for the moshav בן זכאי.
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "1309	אלעד	110	רבן יוחנן בן זכאי	official
1309	אלעד	110	בן זכאי	synonym
         2066	בן זכאי	9000	בן זכאי	official
",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    let d = call.engine.on_agent_turn(
        "בן זכאי 45",
        asking(&["destination"], decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "בן זכאי 45")])),
        "",
    );
    assert_eq!(call.slot("pickup"), None, "not the moshav");
    assert!(spoken(&d).contains("מאיזו עיר לאסוף"), "{}", spoken(&d));
    call.engine.on_agent_turn(
        "מאלעד",
        asking(&["destination"], decide(AgentAction::None, "לאן נוסעים?", None, &[("pickup", "אלעד")])),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "בן זכאי 45, אלעד", "the street kept, in the city given after it");
}

#[test]
fn a_street_its_city_lacks_is_read_back_and_taken_when_confirmed() {
    // The live call of 18:05: "ארנוביץ 32" in ירושלים (it is a street of בני ברק), asked
    // "לאיזה רחוב צריך להגיע?" right after the caller had said it; they hung up.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_and_tel_aviv()));
    let d = call.engine.on_agent_turn(
        "לתל אביב, ארנוביץ 32",
        asking(
            &["passengers"],
            decide(AgentAction::None, "", Some("book_ride"), &[("destination", "ארנוביץ 32, תל אביב")]),
        ),
        "",
    );
    let said = spoken(&d);
    assert!(said.contains("לא מצאתי את ארנוביץ") && said.contains("בתל אביב"), "said what was not found where: {said}");
    assert!(said.contains("התכוונת ל") || said.contains("בעיר אחרת"), "and asked what they meant: {said}");
    call.engine.on_agent_turn(
        "כן",
        asking(
            &["passengers"],
            decide(AgentAction::None, "כמה נוסעים?", None, &[("destination", "ארנוביץ 32, תל אביב")]),
        ),
        "",
    );
    assert!(place(call.slot("destination")).contains("ארנוביץ 32"), "kept once confirmed");
}

#[test]
fn the_street_question_names_the_city_and_a_wrong_city_is_corrected() {
    // The call of 18:30: "לבני ברק" heard as "לברקת"; "איפה בברקת?" lets the caller hear it.
    let gazetteer = callora_core::gazetteer::Gazetteer::from_tsv(
        "6100	בני ברק	301	ז'בוטינסקי	official
1302	ברקת	9001	הזית	official
",
    );
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(gazetteer)));
    call.engine.on_agent_turn(
        "מז'בוטינסקי 3 בבני ברק",
        asking(
            &["destination"],
            decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "ז'בוטינסקי 3, בני ברק")]),
        ),
        "",
    );
    call.engine.on_agent_turn(
        "לברקת",
        asking(&["destination"], decide(AgentAction::None, "", Some("book_ride"), &[("destination", "ברקת")])),
        "",
    );
    let d = call.engine.on_agent_turn(
        "כמה נוסעים? אנחנו שניים",
        asking(&["customer_name"], decide(AgentAction::None, "", None, &[("passengers", "2")])),
        "",
    );
    assert!(spoken(&d).contains("לאן בברקת?"), "the destination's street, in the city understood: {}", spoken(&d));

    call.engine.on_agent_turn(
        "לא ברקת, בני ברק",
        asking(&["destination"], decide(AgentAction::None, "לאן בבני ברק?", None, &[("destination", "בני ברק")])),
        "",
    );
    assert_eq!(call.engine.state.place_cities.get("destination").map(String::as_str), Some("בני ברק"));
    call.engine.on_agent_turn(
        "ז'בוטינסקי 22",
        asking(&["customer_name"], decide(AgentAction::None, "", None, &[("destination", "ז'בוטינסקי 22")])),
        "",
    );
    assert_eq!(place(call.slot("destination")), "ז'בוטינסקי 22, בני ברק");
}

fn elad_efrat() -> Arc<callora_core::gazetteer::Gazetteer> {
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         3650\tאפרת\t170\tהגפן\tofficial\n6100\tבני ברק\t301\tז'בוטינסקי\tofficial\n",
    ))
}

#[test]
fn a_street_is_taken_without_its_house_number() {
    // The owner does not need house numbers: a street is enough, and a number said is kept.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_efrat()));
    let d = call.engine.on_agent_turn(
        "מבן זכאי באלעד",
        asking(&["destination"], decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "בן זכאי, אלעד")])),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "בן זכאי, אלעד");
    assert!(!spoken(&d).contains("מספר בית"), "{}", spoken(&d));

    call.engine.on_agent_turn(
        "לז'בוטינסקי 3 בבני ברק",
        asking(&["passengers"], decide(AgentAction::None, "", None, &[("destination", "ז'בוטינסקי 3, בני ברק")])),
        "",
    );
    assert_eq!(place(call.slot("destination")), "ז'בוטינסקי 3, בני ברק", "a number said is kept");
}

#[test]
fn a_place_that_is_no_locality_is_not_the_pickup_and_a_new_city_replaces_the_old() {
    // The call of 23:58: "מאפרק" (Efrat misheard) was booked as the pickup.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_efrat()));
    let d = call.engine.on_agent_turn(
        "מאפרק לביתר",
        asking(&["pickup"], decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "אפרק")])),
        "",
    );
    assert_eq!(call.slot("pickup"), None);
    assert!(spoken(&d).contains("לא הכרתי את אפרק") && spoken(&d).contains("מאיזו עיר לאסוף"), "{}", spoken(&d));

    // A pickup in one city, corrected to another: the old one goes, the new city's street is asked.
    call.engine.on_agent_turn(
        "מבן זכאי 45 אלעד",
        asking(&["destination"], decide(AgentAction::None, "", None, &[("pickup", "בן זכאי 45, אלעד")])),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "בן זכאי 45, אלעד");
    call.engine.on_agent_turn(
        "לא, מאפרת",
        asking(&["pickup"], decide(AgentAction::None, "איפה באפרת לאסוף?", None, &[("pickup", "אפרת")])),
        "",
    );
    assert_eq!(call.slot("pickup"), None, "the pickup in אלעד is gone");
    assert_eq!(call.engine.state.place_cities.get("pickup").map(String::as_str), Some("אפרת"));
}

#[test]
fn a_place_missed_three_times_goes_to_a_person_or_is_taken_as_said() {
    let three = |call: &mut Call| {
        let mut last = Vec::new();
        for heard in ["אפרק", "אפרוק", "אפרקה"] {
            last = call.engine.on_agent_turn(
                heard,
                asking(&["pickup"], decide(AgentAction::None, "", Some("book_ride"), &[("pickup", heard)])),
                "",
            );
        }
        last
    };
    let (mut call, _) = Call::new(with_desk());
    call.engine.set_gazetteer(Some(elad_efrat()));
    let last = three(&mut call);
    assert!(last.iter().any(|d| matches!(d, Directive::Handoff { .. })), "{last:?}");

    // No desk (as in production today): taken as said, marked for the driver, no hangup.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_efrat()));
    let last = three(&mut call);
    assert!(!last.iter().any(|d| matches!(d, Directive::Hangup | Directive::Handoff { .. })), "{last:?}");
    assert_eq!(place(call.slot("pickup")), "אפרקה");
}

#[test]
fn yes_with_a_but_is_no_yes() {
    // The call of 23:58: "כן אבל אתה יכול רק להחזיר להזמנה" sent the ride.
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn("כן אבל תחזור על ההזמנה", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_none(), "nothing sent: {}", spoken(&d));
    assert!(spoken(&d).contains("לשלוח"), "read back again: {}", spoken(&d));
    let d = call.engine.on_agent_turn("כן", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_some(), "a plain yes sends");
}

#[test]
fn words_not_made_out_before_any_task_wait_for_the_silence_reprompt() {
    // The call of 23:58 heard its greeting twice.
    let (mut call, _) = Call::new(business(&[]));
    assert!(call.engine.on_unheard().is_empty());
}

fn beitar() -> Arc<callora_core::gazetteer::Gazetteer> {
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "3780\tביתר עילית\t118\tהרמב\"ן\tofficial\n3780\tביתר עילית\t118\tהרמבן\tsynonym\n\
         3780\tביתר עילית\t118\tרמבן\tsynonym\n3780\tביתר עילית\t191\tהרמ\"ק\tofficial\n\
         3780\tביתר עילית\t102\tרבי עקיבא\tofficial\n1309\tאלעד\t110\tבן זכאי\tofficial\n",
    ))
}

#[test]
fn a_street_said_with_a_city_not_made_out_is_kept_for_the_city() {
    // The call of 12:10: "רמבם 12 בטרדיט" (ביתר עילית misheard); after "ביתר עילית" the street
    // was asked again, though the caller had said it.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(beitar()));
    let d = call.engine.on_agent_turn(
        "רמבם 12 בטרדיט",
        asking(&["destination"], decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "רמבם 12 בטרדיט")])),
        "",
    );
    assert_eq!(call.slot("pickup"), None);
    let said = spoken(&d);
    assert!(said.contains("רמבם 12 באיזו עיר"), "the street kept, only the city asked: {said}");

    // The city: the street said before is looked up there, and the closest street offered.
    let d = call.engine.on_agent_turn(
        "ביתר אליס",
        asking(&["pickup"], decide(AgentAction::None, "", None, &[("pickup", "ביתר עילית")])),
        "",
    );
    let said = spoken(&d);
    assert!(said.contains("לא מצאתי את רמבם בביתר עילית") && said.contains("הרמב\"ן"), "{said}");
    assert!(!said.contains("איזה רחוב") && !said.contains("איפה בביתר"), "the street is not asked again: {said}");

    call.engine.on_agent_turn(
        "כן הרמבן",
        asking(&["destination"], decide(AgentAction::None, "", None, &[("pickup", "הרמב\"ן 12, ביתר עילית")])),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "הרמב\"ן 12, ביתר עילית");
}

#[test]
fn a_street_without_a_number_the_city_lacks_gets_the_closest_offered() {
    // The same call: "רמבם" alone in ביתר עילית was asked three times and then booked as said.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(beitar()));
    let d = call.engine.on_agent_turn(
        "רמבם בביתר עילית",
        asking(&["destination"], decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "רמבם, ביתר עילית")])),
        "",
    );
    assert_eq!(call.slot("pickup"), None);
    let said = spoken(&d);
    assert!(said.contains("לא מצאתי את רמבם בביתר עילית") && said.contains("הרמב\"ן"), "{said}");

    // Yes to it: the street, with no house number asked.
    let d = call.engine.on_agent_turn(
        "כן",
        asking(&["pickup"], decide(AgentAction::None, "", None, &[("pickup", "הרמב\"ן, ביתר עילית")])),
        "",
    );
    assert_eq!(place(call.slot("pickup")), "הרמב\"ן, ביתר עילית");
    assert!(!spoken(&d).contains("מספר בית"), "{}", spoken(&d));
}

#[test]
fn words_begun_before_the_last_question_are_told_to_the_agent_as_the_previous_answer() {
    // A live call: "דוד" ... "אביטבול" became the name "דוד" and the driver note "אביטבול".
    let (mut call, _) = Call::new(business(&[]));
    call.engine.state.continues_answer = true;
    let request = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "אביטבול");
    assert!(request.user.contains("OVERLAP"), "{}", request.user);
    call.engine.state.continues_answer = false;
    let request = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "אביטבול");
    assert!(!request.user.contains("OVERLAP"));
}

fn booked_ride() -> Call {
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn("כן", decide(AgentAction::Submit, "", None, &[]), "");
    let (run_id, _, _) = action(&d).expect("the ride is sent");
    let d = call.engine.on_action_result(run_id, Ok(serde_json::json!({ "ride_id": "R-1" })));
    assert!(spoken(&d).contains("משהו נוסף") || spoken(&d).contains("במשהו נוסף"), "{}", spoken(&d));
    call
}

#[test]
fn a_yes_with_a_word_that_corrects_nothing_sends() {
    // "כן סליחה", "כן, לא צריך כלום": read back again and again until the call went to a person.
    for yes in ["כן סליחה", "כן, לא צריך כלום, תשלח"] {
        let mut call = read_back_ride();
        let d = call.engine.on_agent_turn(yes, decide(AgentAction::Submit, "", None, &[]), "");
        assert!(action(&d).is_some(), "{yes}: {}", spoken(&d));
    }
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn("כן רגע", decide(AgentAction::Submit, "", None, &[]), "");
    assert!(action(&d).is_none(), "a yes with a wait is no yes");
    for cancellation in ["כן, לא צריך מונית", "כן, לא תודה", "כן, לא משנה, אל תשלח"]
    {
        let mut call = read_back_ride();
        let d = call.engine.on_agent_turn(cancellation, decide(AgentAction::Submit, "", None, &[]), "");
        assert!(action(&d).is_none(), "{cancellation}: a denial cannot be stripped into confirmation");
    }
}

#[test]
fn yes_said_over_the_read_back_is_the_caller_listening() {
    let call = read_back_ride();
    for listening in ["כן", "כן כן", "אהה", "בסדר", "טוב"] {
        assert!(call.engine.is_backchannel(listening), "{listening}");
    }
    for words in ["כן אבל רגע", "לא", "ארבעה נוסעים"] {
        assert!(!call.engine.is_backchannel(words), "{words}");
    }
    let (mut call, _) = Call::new(business(&[]));
    call.say("צריך מונית");
    assert!(!call.engine.is_backchannel("כן"), "only while details are read back");
}

#[test]
fn short_answers_the_call_waits_for_are_not_noise() {
    // "טוב", "תודה" are filler words, dropped as noise; to "משהו נוסף?" or the read-back they
    // are the answer, and the question was asked again.
    let call = read_back_ride();
    assert!(call.engine.takes_short_answer("תודה"));
    assert!(!call.engine.takes_short_answer("אממ"));
    assert!(!call.engine.takes_short_answer("הלו"));
    let (mut call, _) = Call::new(business(&[]));
    call.say("צריך מונית");
    assert!(!call.engine.takes_short_answer("תודה"), "nothing waits for it");
}

#[test]
fn thanks_to_anything_else_ends_the_call() {
    let mut call = booked_ride();
    assert!(call.engine.on_closing("אממ").is_none());
    let d = call.engine.on_closing("תודה רבה").expect("the goodbye");
    assert!(d.iter().any(|d| matches!(d, Directive::Hangup)), "{d:?}");

    // The same through the agent: its goodbye is kept.
    let mut call = booked_ride();
    let d = call.engine.on_agent_turn("טוב תודה", decide(AgentAction::EndCall, "", None, &[]), "");
    assert!(d.iter().any(|d| matches!(d, Directive::Hangup)), "{d:?}");
}

#[test]
fn hello_is_answered_with_the_question_again() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מאלעד בן זכאי 45 לירושלים",
        decide(
            AgentAction::None,
            "סגור. כמה נוסעים?",
            Some("book_ride"),
            &[("pickup", "בן זכאי 45, אלעד"), ("destination", "ירושלים")],
        ),
        "",
    );
    assert!(call.engine.is_hello("הלו"));
    assert!(call.engine.is_hello("הלו הלו?"));
    assert!(!call.engine.is_hello("הלו אני צריך מונית"));
    let said = spoken(&call.engine.on_hello());
    assert!(said.starts_with("כן, ") && said.ends_with("כמה נוסעים?"), "{said}");
}

#[test]
fn a_silent_caller_with_details_given_is_waited_for() {
    // A caller looking for the house number was hung up on after about twenty seconds.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מאלעד בן זכאי 45 לירושלים",
        decide(
            AgentAction::None,
            "סגור. כמה נוסעים?",
            Some("book_ride"),
            &[("pickup", "בן זכאי 45, אלעד"), ("destination", "ירושלים")],
        ),
        "",
    );
    for _ in 0..2 {
        assert!(spoken(&call.engine.on_silence()).contains("שומעים אותי"));
    }
    assert_eq!(call.engine.silence_after_ms(), 5_000);
    for _ in 0..2 {
        let d = call.engine.on_silence();
        assert!(spoken(&d).contains("אני") && !d.iter().any(|d| matches!(d, Directive::Hangup)), "{}", spoken(&d));
        assert_eq!(call.engine.silence_after_ms(), 15_000);
    }
    let d = call.engine.on_silence();
    assert!(d.iter().any(|d| matches!(d, Directive::Hangup)), "then the goodbye");

    // Before any detail, the usual two reprompts.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_silence();
    call.engine.on_silence();
    assert!(call.engine.on_silence().iter().any(|d| matches!(d, Directive::Hangup)));
}

// The live call of 2026-09-30 00:45: "מאלעד לבני ברק" was asked "מאיזו עיר לאסוף?", then
// "לאן צריך להגיע?"; "הנביאים 2" moved to ירושלים lost its number; "לא" became the driver's
// note; "Bye." did not end the call.

fn elad_bnei_brak_jerusalem() -> Arc<callora_core::gazetteer::Gazetteer> {
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         6100\tבני ברק\t301\tרבי עקיבא\tofficial\n3000\tירושלים\t140\tהנביאים\tofficial\n",
    ))
}

fn with_phrase(phrase: &str, turn: AgentTurn) -> AgentTurn {
    AgentTurn { phrase: Some(phrase.into()), ..turn }
}

#[test]
fn two_cities_given_are_not_asked_for_again() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_bnei_brak_jerusalem()));
    call.engine.on_agent_turn(
        "אני רוצה להזמין מונית",
        decide(AgentAction::None, "מאיפה לאן?", Some("book_ride"), &[]),
        "",
    );
    // The runtime held the phrase back: it asks for the pickup the reply passes.
    let d = call.engine.on_agent_turn(
        "מאלעד לבני ברק",
        with_phrase(
            "ask_pickup_city",
            decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "אלעד"), ("destination", "בני ברק")]),
        ),
        "",
    );
    let said = spoken(&d);
    assert!(said.contains("באלעד") && !said.contains("מאיזו עיר"), "{said}");

    // The pickup street, and the model's general "לאן צריך להגיע?": the city is known.
    call.engine.on_agent_turn(
        "בן זכאי ארבעים וחמש",
        decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "בן זכאי 45, אלעד")]),
        "",
    );
    let asked = call.engine.render_phrase("ask_destination").expect("a question").text();
    assert_eq!(asked, "לאן בבני ברק?");
}

#[test]
fn a_house_number_said_with_a_street_of_another_city_is_kept() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad_bnei_brak_jerusalem()));
    call.engine.on_agent_turn(
        "מבן זכאי 45 באלעד לבני ברק",
        decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "בן זכאי 45, אלעד"), ("destination", "בני ברק")]),
        "",
    );
    let d = call.engine.on_agent_turn(
        "הנביאים שתיים",
        decide(AgentAction::None, "", Some("book_ride"), &[("destination", "הנביאים 2, בני ברק")]),
        "",
    );
    assert!(spoken(&d).contains("הנביאים"), "not found there, said so: {}", spoken(&d));
    // The model passes the street in the other city without the number said before.
    let d = call.engine.on_agent_turn(
        "אני גר בירושלים",
        decide(AgentAction::None, "", Some("book_ride"), &[("destination", "הנביאים, ירושלים")]),
        "",
    );
    assert_eq!(place(call.slot("destination")), "הנביאים 2, ירושלים");
    assert!(!spoken(&d).contains("מספר בית"), "the number is not asked again: {}", spoken(&d));
}

#[test]
fn no_to_the_driver_question_is_no_note() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad()));
    let d = call.engine.on_agent_turn(
        "בן זכאי 40 אלעד לסוכות ירושלים, שלושה",
        decide(
            AgentAction::None,
            "יש משהו שהנהג צריך לדעת?",
            Some("book_ride"),
            &[
                ("pickup", "בן זכאי 40, אלעד"),
                ("destination", "סוכות 12, ירושלים"),
                ("passengers", "שלושה"),
                ("customer_name", "דוד"),
            ],
        ),
        "",
    );
    assert!(spoken(&d).contains("הנהג"), "{}", spoken(&d));
    let d = call.engine.on_agent_turn("לא.", decide(AgentAction::ReadBack, "", None, &[("notes", "לא")]), "");
    assert_eq!(call.slot("notes"), None, "\"לא\" is not a note");
    assert_eq!(call.step(), Some(Step::AwaitingConfirmation), "read back: {}", spoken(&d));
    assert!(!spoken(&d).contains("הנהג"), "not asked again: {}", spoken(&d));
}

#[test]
fn bye_in_english_letters_ends_the_call() {
    let mut call = booked_ride();
    let d = call.engine.on_agent_turn("Bye.", with_phrase("goodbye", decide(AgentAction::EndCall, "", None, &[])), "");
    assert!(d.iter().any(|d| matches!(d, Directive::Hangup)), "{}", spoken(&d));
}

// The live call of 2026-09-30 15:28: "כמה נוסעים?" before the pickup street; "ירושלים" to
// "לאן בירושלים?" became the street "ירושלים 4"; the driver's note "אין".

/// בני ברק, and a ירושלים with streets of its own besides its own row (code 9000).
fn bnei_brak_and_a_big_jerusalem() -> Arc<callora_core::gazetteer::Gazetteer> {
    let mut tsv = String::from("6100\tבני ברק\t301\tאהרונוביץ\tofficial\n3000\tירושלים\t9000\tירושלים\tofficial\n");
    tsv.push_str("3000\tירושלים\t348\tהנביאים\tofficial\n");
    for i in 0..40 {
        tsv.push_str(&format!("3000\tירושלים\t{}\tרחוב מספר {}\tofficial\n", 1000 + i, i));
    }
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(&tsv))
}

#[test]
fn the_head_count_waits_for_the_pickup_street() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(bnei_brak_and_a_big_jerusalem()));
    call.engine.on_agent_turn("רוצה להזמין מונית", decide(AgentAction::None, "מאיפה לאן?", Some("book_ride"), &[]), "");
    let fields = [("pickup".to_string(), "בני ברק".to_string()), ("destination".to_string(), "ירושלים".to_string())];
    assert!(call.engine.phrase_skips_a_place("מבני ברק לירושלים", &fields, "ask_passengers"), "held by the runtime");
    let d = call.engine.on_agent_turn(
        "מבני ברק לירושלים",
        with_phrase(
            "ask_passengers",
            decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "בני ברק"), ("destination", "ירושלים")]),
        ),
        "",
    );
    let said = spoken(&d);
    assert!(said.contains("בבני ברק") && !said.contains("נוסעים"), "{said}");
}

#[test]
fn a_city_said_again_to_its_street_question_is_not_a_street() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(bnei_brak_and_a_big_jerusalem()));
    call.engine.on_agent_turn(
        "מאהרונוביץ 32 בבני ברק לירושלים",
        decide(
            AgentAction::None,
            "",
            Some("book_ride"),
            &[("pickup", "אהרונוביץ 32, בני ברק"), ("destination", "ירושלים")],
        ),
        "",
    );
    let d = call.engine.on_agent_turn(
        "אני מביאים ארבע ירושלים",
        with_phrase("ask_destination", decide(AgentAction::None, "", Some("book_ride"), &[("destination", "ירושלים")])),
        "",
    );
    // The city again after its street was asked: the city is the destination (the street is
    // asked once), never a street named ירושלים waiting for its house number.
    assert_eq!(place(call.slot("destination")), "ירושלים");
    assert!(!spoken(&d).contains("מספר בית"), "{}", spoken(&d));

    // The street itself still is one.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(bnei_brak_and_a_big_jerusalem()));
    call.engine.on_agent_turn(
        "מאהרונוביץ 32 בבני ברק להנביאים ארבע בירושלים",
        decide(
            AgentAction::None,
            "",
            Some("book_ride"),
            &[("pickup", "אהרונוביץ 32, בני ברק"), ("destination", "הנביאים 4, ירושלים")],
        ),
        "",
    );
    assert_eq!(place(call.slot("destination")), "הנביאים 4, ירושלים");
}

#[test]
fn nothing_for_the_driver_is_no_note() {
    for nothing in ["אין", "לא, כלום", "שום דבר"] {
        let (mut call, _) = Call::new(business(&[]));
        call.engine.set_gazetteer(Some(elad()));
        call.engine.on_agent_turn(
            "בן זכאי 40 אלעד לסוכות ירושלים, שלושה, דוד",
            decide(
                AgentAction::None,
                "יש משהו שהנהג צריך לדעת?",
                Some("book_ride"),
                &[
                    ("pickup", "בן זכאי 40, אלעד"),
                    ("destination", "סוכות 12, ירושלים"),
                    ("passengers", "שלושה"),
                    ("customer_name", "דוד"),
                ],
            ),
            "",
        );
        call.engine.on_agent_turn(nothing, decide(AgentAction::ReadBack, "", None, &[("notes", nothing)]), "");
        assert_eq!(call.slot("notes"), None, "{nothing}");
        assert_eq!(call.step(), Some(Step::AwaitingConfirmation), "{nothing}");
    }
}

#[test]
fn hesitation_in_english_letters_is_noise() {
    let (call, _) = Call::new(business(&[]));
    let b = call.engine.business().clone();
    for um in ["Um.", "Uh", "Hmm..."] {
        assert!(fast_path(&b, &call.engine.context(), um).0.noise, "{um}");
    }
}

// The live call of 2026-09-30 16:48: "בן זכאי" came back "בן זה קיץ" and "באיזה קו"; the
// silence reprompt said "סליחה, יש קצת רעש בקו" again.

fn elad_streets() -> Arc<callora_core::gazetteer::Gazetteer> {
    Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         1309\tאלעד\t120\tרבי עקיבא\tofficial\n1309\tאלעד\t130\tשמעון הצדיק\tofficial\n\
         1309\tאלעד\t140\tהרי\"ף\tofficial\n1309\tאלעד\t150\tבעלי התוספות\tofficial\n",
    ))
}

#[test]
fn a_street_that_sounds_like_the_one_heard_is_offered() {
    for heard in ["בן זה קיץ 46, אלעד", "באיזה קו 46, אלעד"] {
        let (mut call, _) = Call::new(business(&[]));
        call.engine.set_gazetteer(Some(elad_streets()));
        call.engine.on_agent_turn(
            "מבן זכאי 45 באלעד לאלעד",
            decide(
                AgentAction::None,
                "",
                Some("book_ride"),
                &[("pickup", "בן זכאי 45, אלעד"), ("destination", "אלעד")],
            ),
            "",
        );
        let d = call.engine.on_agent_turn(
            heard,
            decide(AgentAction::None, "", Some("book_ride"), &[("destination", heard)]),
            "",
        );
        let said = spoken(&d);
        assert!(said.contains("התכוונת") && said.contains("בן זכאי"), "{heard}: {said}");
        assert_eq!(call.slot("destination"), None);
        // "כן": the street offered, with the number said.
        call.engine.on_agent_turn(
            "כן",
            decide(AgentAction::None, "", Some("book_ride"), &[("destination", "רבן יוחנן בן זכאי 46, אלעד")]),
            "",
        );
        assert_eq!(place(call.slot("destination")), "רבן יוחנן בן זכאי 46, אלעד");
    }
}

#[test]
fn the_silence_reprompt_does_not_say_the_line_is_noisy_again() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.on_agent_turn(
        "מאלעד בן זכאי 45 לירושלים",
        decide(
            AgentAction::None,
            "סגור. כמה נוסעים?",
            Some("book_ride"),
            &[("pickup", "בן זכאי 45, אלעד"), ("destination", "ירושלים")],
        ),
        "",
    );
    assert!(spoken(&call.engine.on_unheard()).contains("רעש"));
    let d = call.engine.on_silence();
    let said = spoken(&d);
    assert!(said.contains("שומעים אותי") && said.contains("כמה נוסעים") && !said.contains("רעש"), "{said}");
}

// The live call of 2026-09-30 19:16: "כן, רק כמה זה עולה?" to the read-back was answered with
// "מאיפה הנסיעה?"; the agent passed the price question without the booking's places.

#[test]
fn a_price_asked_at_the_read_back_takes_the_bookings_route_and_passengers() {
    let mut call = read_back_ride();
    let d = call.engine.on_agent_turn(
        "כן, רק כמה זה עולה?",
        decide(AgentAction::Submit, "", Some("price_question"), &[]),
        "",
    );
    let (run_id, name, input) = action(&d).expect("the price is asked at once: {d:?}");
    assert_eq!(name, "estimate_price");
    assert!(!spoken(&d).contains("מאיפה"), "{}", spoken(&d));
    assert_eq!(input["slots"]["price_from"]["address"], "רבן יוחנן בן זכאי 40, אלעד");
    assert_eq!(input["slots"]["price_to"]["spoken"], "סוכות 12, ירושלים");
    assert_eq!(input["slots"]["passengers"], 3, "the quote is for the booking's passengers");
    let d = call.engine.on_action_result(run_id, Ok(serde_json::json!({ "price": 180, "response": "price_answer" })));
    let said = spoken(&d);
    assert!(said.contains("180₪") && said.contains("לשלוח"), "the price, then the read-back again: {said}");
}

#[test]
fn a_price_asked_with_only_the_cities_known_takes_them() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad()));
    call.engine.on_agent_turn(
        "מאלעד לירושלים",
        decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "אלעד"), ("destination", "ירושלים")]),
        "",
    );
    let d = call.engine.on_agent_turn("כמה זה עולה?", decide(AgentAction::Submit, "", Some("price_question"), &[]), "");
    let (_, _, input) = action(&d).expect("the price is asked: {d:?}");
    assert_eq!(input["slots"]["price_from"]["spoken"], "אלעד");
    assert_eq!(input["slots"]["price_to"]["spoken"], "ירושלים");
}

// The live call of 2026-09-30 20:09: "מאפרת לביתר" came back "מפרט לביתר", ביתר was taken for the
// pickup, and "מפרט" was not heard as אפרת.

#[test]
fn a_place_said_with_the_destinations_preposition_is_the_destination() {
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(beitar()));
    let d = call.engine.on_agent_turn(
        "אני צריך מפרט לביתר",
        decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "ביתר")]),
        "",
    );
    assert!(
        call.engine.state.place_cities.get("destination").is_some_and(|c| c.contains("ביתר")),
        "{:?}",
        call.engine.state.place_cities
    );
    assert!(!call.engine.state.place_cities.contains_key("pickup"));
    assert!(!spoken(&d).contains("לאסוף"), "not asked where in ביתר to pick up: {}", spoken(&d));
    // Said with its own preposition, it stays.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(beitar()));
    call.engine.on_agent_turn("מביתר", decide(AgentAction::None, "", Some("book_ride"), &[("pickup", "ביתר")]), "");
    assert!(call.engine.state.place_cities.contains_key("pickup"));
}

#[test]
fn a_word_that_sounds_like_a_town_is_told_to_the_agent() {
    let mut tsv = String::new();
    for i in 0..25 {
        for (code, town) in [("3650", "אפרת"), ("3780", "ביתר עילית"), ("1111", "מענית")] {
            tsv.push_str(&format!("{code}\t{town}\t{}\tרחוב {i}\tofficial\n", 100 + i));
        }
    }
    let g = callora_core::gazetteer::Gazetteer::from_tsv(&tsv);
    assert_eq!(g.towns_sounding_like("אני צריך מפרט לביתר"), vec![("מפרט".to_string(), "אפרת".to_string())]);
    assert!(g.towns_sounding_like("מאפרת לביתר").is_empty(), "the real names need no hint");
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(Arc::new(g)));
    call.engine.hint_towns("אני צריך מפרט לביתר");
    call.engine.hint_towns("אני צריך מפרט לביתר");
    call.engine.hint_towns("צריך מונית");
    let request = callora_core::agent::build_request(call.engine.business(), &call.engine.state, "אני צריך מפרט לביתר");
    assert_eq!(request.user.matches("sounds like אפרת").count(), 1, "{}", request.user);
    assert!(!request.user.contains("מענית"), "the business's own word \"מונית\" is no town: {}", request.user);
}

#[test]
fn how_long_a_ride_takes_is_said_from_the_price_list() {
    // The call of 20:22: "כמה זמן נסיעה מביתר לירושלים?" was answered with the prices.
    let (mut call, _) = Call::new(business(&[]));
    let d = call.engine.on_agent_turn(
        "כמה זמן נסיעה מביתר לירושלים?",
        decide(
            AgentAction::Submit,
            "",
            Some("price_question"),
            &[("price_from", "ביתר"), ("price_to", "ירושלים"), ("asks_about", "time")],
        ),
        "",
    );
    let (run_id, _, input) = action(&d).expect("the list is asked");
    assert_eq!(input["slots"]["asks_about"], "time");
    let quote =
        serde_json::json!({ "price": 120, "duration": "34 דקות", "distance_km": 22, "response": "ride_time_answer" });
    let said = spoken(&call.engine.on_action_result(run_id, Ok(quote)));
    assert!(said.contains("הנסיעה בערך 34 דקות, 22 קילומטר."), "{said}");
}

#[test]
fn the_booking_asks_in_its_order_whatever_the_agent_writes() {
    // The owner: a fixed order the agent cannot change. Pickup, destination, passengers,
    // name, note; details given early are kept, and an optional question is asked once.
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad()));
    // The agent asks for the passengers first: the pickup is asked instead.
    let d = call.engine.on_agent_turn(
        "צריך מונית",
        asking(&["passengers"], decide(AgentAction::None, "", Some("book_ride"), &[])),
        "",
    );
    assert!(spoken(&d).contains("לאסוף") || spoken(&d).contains("אוספים"), "{}", spoken(&d));
    // The pickup and the passengers together: both kept; the agent's name question gives way
    // to the destination.
    let d = call.engine.on_agent_turn(
        "מבן זכאי 40 באלעד, אנחנו שלושה",
        asking(
            &["customer_name"],
            decide(AgentAction::None, "", None, &[("pickup", "בן זכאי 40, אלעד"), ("passengers", "3")]),
        ),
        "",
    );
    assert_eq!(call.slot("passengers").map(|v| v.spoken()).as_deref(), Some("3"));
    assert!(spoken(&d).contains("לאן"), "{}", spoken(&d));
    // The destination given: the passengers are known, so the name comes next.
    let d = call.engine.on_agent_turn(
        "לסוכות 12 בירושלים",
        asking(&["notes"], decide(AgentAction::None, "", None, &[("destination", "סוכות 12, ירושלים")])),
        "",
    );
    assert!(spoken(&d).contains("על שם מי"), "{}", spoken(&d));
    // No name: asked once, the note comes next and the name is not asked again.
    let d =
        call.engine.on_agent_turn("לא משנה", asking(&["customer_name"], decide(AgentAction::None, "", None, &[])), "");
    assert!(spoken(&d).contains("נהג") && !spoken(&d).contains("על שם מי"), "{}", spoken(&d));
    // In the runtime, the agent's question out of order is held before it plays.
    let fields = [("destination".to_string(), "סוכות 12, ירושלים".to_string())];
    let (mut call, _) = Call::new(business(&[]));
    call.engine.set_gazetteer(Some(elad()));
    call.engine.on_agent_turn("צריך מונית", decide(AgentAction::None, "", Some("book_ride"), &[]), "");
    assert_eq!(
        call.engine.out_of_order("לסוכות 12 בירושלים", &fields, &["passengers".into()]).as_deref(),
        Some("pickup")
    );
    assert_eq!(call.engine.out_of_order("לסוכות 12 בירושלים", &fields, &["pickup".into()]), None);
}
