//! What each call cost, for the costs page: the phone line and its media stream by the
//! minute, speech recognition by the minute, the agent's tokens, the second hearings, and the
//! live speech by character. The rates are the owner's (kept in `app_settings` under
//! `cost_rates`), with defaults from the providers' price pages in October 2026; a call is
//! priced with the rates of the moment, not those of its day.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ports::{Meter, Usage};

/// Dollars, each per the unit in its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rates {
    /// Twilio, an incoming call to an Israeli local number, by the started minute.
    pub phone_per_minute: f64,
    /// Twilio Media Streams (the call's audio to the server), by the started minute.
    pub stream_per_minute: f64,
    /// Speech recognition (Soniox real time), for the call's length.
    pub stt_per_minute: f64,
    /// The agent's model, per million tokens.
    pub llm_input_per_million: f64,
    pub llm_cached_per_million: f64,
    pub llm_output_per_million: f64,
    /// One second hearing (gpt-audio with the list of streets).
    pub second_hearing_each: f64,
    /// ElevenLabs live speech, per thousand characters (a plan's price over its credits).
    pub tts_per_thousand_chars: f64,
    /// Monthly fees whatever the calls (the phone number, the voice plan).
    pub monthly_fixed: f64,
    /// Shekels per dollar, to show both.
    pub shekels_per_dollar: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            phone_per_minute: 0.0107,
            stream_per_minute: 0.004,
            stt_per_minute: 0.002,
            llm_input_per_million: 0.10,
            llm_cached_per_million: 0.01,
            llm_output_per_million: 0.50,
            second_hearing_each: 0.01,
            tts_per_thousand_chars: 0.0965,
            monthly_fixed: 27.5,
            shekels_per_dollar: 3.7,
        }
    }
}

impl Rates {
    pub fn problems(&self) -> Vec<String> {
        let all = [
            self.phone_per_minute,
            self.stream_per_minute,
            self.stt_per_minute,
            self.llm_input_per_million,
            self.llm_cached_per_million,
            self.llm_output_per_million,
            self.second_hearing_each,
            self.tts_per_thousand_chars,
            self.monthly_fixed,
        ];
        let mut p = Vec::new();
        if all.iter().any(|v| !v.is_finite() || *v < 0.0) {
            p.push("מחיר לא יכול להיות שלילי".to_string());
        }
        if !(self.shekels_per_dollar.is_finite() && self.shekels_per_dollar > 0.0) {
            p.push("שער הדולר חייב להיות חיובי".to_string());
        }
        p
    }
}

/// One call's facts, as the database has them.
#[derive(Debug, Clone, Default)]
pub struct CallUse {
    pub seconds: Option<f64>,
    pub usage: Option<Usage>,
    /// Recorded from the day calls were metered; before, live speech is unknown.
    pub meter: Option<Meter>,
    /// Second hearings counted from the call's turns (for calls before the meter).
    pub second_hearings_in_turns: u32,
}

/// What one call cost, by part, in dollars. A part with nothing to price it is left out of
/// the total and named in `unknown`.
pub fn price(call: &CallUse, r: &Rates) -> Value {
    let minutes = call.seconds.map(|s| (s / 60.0).ceil().max(1.0));
    let exact_minutes = call.seconds.map(|s| s / 60.0);
    let mut unknown = Vec::new();
    let phone = minutes.map(|m| m * r.phone_per_minute);
    let stream = minutes.map(|m| m * r.stream_per_minute);
    let stt = exact_minutes.map(|m| m * r.stt_per_minute);
    if minutes.is_none() {
        unknown.push("minutes");
    }
    let llm = call.usage.as_ref().map(|u| {
        let uncached = u.input.saturating_sub(u.cached) as f64;
        (uncached * r.llm_input_per_million
            + u.cached as f64 * r.llm_cached_per_million
            + u.output as f64 * r.llm_output_per_million)
            / 1e6
    });
    let hearings = call.meter.as_ref().map_or(call.second_hearings_in_turns, |m| m.second_hearings);
    let second = f64::from(hearings) * r.second_hearing_each;
    let tts = call.meter.as_ref().map(|m| m.tts_chars as f64 / 1000.0 * r.tts_per_thousand_chars);
    if tts.is_none() {
        unknown.push("tts");
    }
    let parts = [phone, stream, stt, llm.or(Some(0.0)), Some(second), tts];
    let total: f64 = parts.iter().flatten().sum();
    json!({
        "phone": phone,
        "stream": stream,
        "stt": stt,
        "llm": llm.unwrap_or(0.0),
        "second_hearing": second,
        "tts": tts,
        "total": total,
        "second_hearings": hearings,
        "tts_chars": call.meter.as_ref().map(|m| m.tts_chars),
        "unknown": unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minute_and_a_half_is_priced_by_its_parts() {
        let r = Rates::default();
        let call = CallUse {
            seconds: Some(90.0),
            usage: Some(Usage { model: "gpt-6-luna".into(), input: 100_000, cached: 90_000, output: 1_000 }),
            meter: Some(Meter { tts_chars: 500, tts_requests: 4, second_hearings: 1 }),
            second_hearings_in_turns: 0,
        };
        let p = price(&call, &r);
        let near = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-9;
        assert!(near(&p["phone"], 2.0 * 0.0107), "two started minutes: {p}");
        assert!(near(&p["stt"], 1.5 * 0.002), "by the exact length: {p}");
        assert!(near(&p["llm"], (10_000.0 * 0.10 + 90_000.0 * 0.01 + 1_000.0 * 0.50) / 1e6));
        assert!(near(&p["second_hearing"], 0.01));
        assert!(near(&p["tts"], 0.5 * 0.0965));
        assert!(p["unknown"].as_array().unwrap().is_empty());
        // A call from before the meter: live speech unknown, second hearings from its turns.
        let old = CallUse { meter: None, second_hearings_in_turns: 2, ..call };
        let p = price(&old, &r);
        assert!(p["tts"].is_null());
        assert_eq!(p["second_hearings"], 2);
        assert_eq!(p["unknown"], json!(["tts"]));
    }

    #[test]
    fn saved_rates_keep_the_defaults_for_what_they_lack() {
        let r: Rates = serde_json::from_value(json!({ "phone_per_minute": 0.02 })).unwrap();
        assert_eq!(r.phone_per_minute, 0.02);
        assert_eq!(r.stt_per_minute, Rates::default().stt_per_minute);
        assert!(r.problems().is_empty());
        assert!(!Rates { shekels_per_dollar: 0.0, ..Rates::default() }.problems().is_empty());
    }
}
