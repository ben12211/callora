//! Handing a caller to the dispatch desk, with music while they wait.
//!
//! The caller is moved into a Twilio conference of their own, where the hold music plays
//! (`startConferenceOnEnter=false`). Every desk number is rung at once; the first one who
//! answers hears the call's summary and joins, which starts the conference and stops the
//! music. The others are cancelled. If every number refuses or fails (busy, no route), or
//! no one answers within the configured wait, the caller hears that no one is available
//! and the call ends: a live call's desk number refused at once and the caller still
//! waited a minute with the music. The caller leaving ends the conference
//! (`endConferenceOnExit`), so a dispatcher is never left alone in it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use callora_core::engine::HandoffSummary;

use crate::ports::{CallInfo, Telephony};
use crate::settings::DeskSettings;
use crate::twilio;

struct Transfer {
    conference: String,
    caller: String,
    summary: String,
    language: String,
    legs: Vec<String>,
    /// Legs that ended without anyone answering (busy, failed, no answer, cancelled).
    ended: Vec<String>,
    /// Every leg has been created (a leg can fail before the last one is dialed).
    dialed: bool,
    /// What the caller hears when no one can take the call.
    give_up: String,
    taken: bool,
    at: Instant,
}

pub struct Desk {
    base: String,
    telephony: Arc<dyn Telephony>,
    transfers: Arc<Mutex<HashMap<String, Transfer>>>,
}

impl Desk {
    pub fn new(base: String, telephony: Arc<dyn Telephony>) -> Self {
        Self { base, telephony, transfers: Arc::default() }
    }

    /// Move the caller to the desk. `unavailable` is said when no one answers. Returns
    /// false when the call could not be moved at all (the caller is then hung up).
    pub async fn transfer(
        &self,
        call: &CallInfo,
        summary: &HandoffSummary,
        desk: &DeskSettings,
        language: &str,
        unavailable: &str,
    ) -> bool {
        if desk.numbers.is_empty() || self.base.is_empty() {
            return false;
        }
        let token = hex::encode(rand::random::<[u8; 16]>());
        let conference = format!("desk-{}", call.call_id.simple());
        let give_up = twilio::twiml_say_hangup(unavailable, language);
        {
            let mut t = self.transfers.lock();
            t.retain(|_, x| x.at.elapsed() < Duration::from_secs(3600));
            t.insert(
                token.clone(),
                Transfer {
                    conference: conference.clone(),
                    caller: call.call_sid.clone(),
                    summary: summary.text.clone(),
                    language: language.to_string(),
                    legs: Vec::new(),
                    ended: Vec::new(),
                    dialed: false,
                    give_up: give_up.clone(),
                    taken: false,
                    at: Instant::now(),
                },
            );
        }
        let wait = twilio::twiml_conference_wait(&conference, &desk.music_url());
        if let Err(e) = self.telephony.redirect(&call.call_sid, &wait).await {
            tracing::error!(call = %call.call_sid, error = %e, "could not move the caller to the desk");
            return false;
        }
        let url = format!("{}{}?t={token}", self.base, twilio::DESK_PATH);
        let status_url = format!("{}{}?t={token}", self.base, twilio::DESK_STATUS_PATH);
        let ring = desk.max_wait_seconds.clamp(15, 60);
        for number in &desk.numbers {
            match self.telephony.dial(number, &call.to, &url, &status_url, ring).await {
                Ok(sid) => {
                    if let Some(t) = self.transfers.lock().get_mut(&token) {
                        t.legs.push(sid);
                    }
                }
                Err(e) => tracing::warn!(call = %call.call_sid, %number, error = %e, "could not ring a desk number"),
            }
        }
        let rung = {
            let mut t = self.transfers.lock();
            match t.get_mut(&token) {
                Some(x) => {
                    x.dialed = true;
                    !x.legs.is_empty()
                }
                None => false,
            }
        };
        if !rung {
            let _ = self.telephony.redirect(&call.call_sid, &give_up).await;
            return true;
        }
        // A leg may have been refused while the others were still being dialed.
        self.give_up_if_all_failed(&token).await;
        // No one answered in time: tell the caller, and stop ringing.
        let (transfers, telephony, caller) = (self.transfers.clone(), self.telephony.clone(), call.call_sid.clone());
        let wait_for = Duration::from_secs(u64::from(desk.max_wait_seconds));
        tokio::spawn(async move {
            tokio::time::sleep(wait_for).await;
            let legs = {
                let mut t = transfers.lock();
                match t.get_mut(&token) {
                    Some(x) if !x.taken => {
                        x.taken = true;
                        std::mem::take(&mut x.legs)
                    }
                    _ => return,
                }
            };
            tracing::info!(call = %caller, "no one at the desk answered");
            let _ = telephony.redirect(&caller, &give_up).await;
            for leg in legs {
                let _ = telephony.cancel(&leg).await;
            }
        });
        true
    }

    /// A desk call ended without being answered (`status` is Twilio's: busy, failed,
    /// no-answer, canceled). When that was the last number, the caller is told at once.
    pub async fn leg_ended(&self, token: &str, leg: &str, status: &str) {
        // A call that was answered and then ended is the conversation itself.
        if !matches!(status, "busy" | "failed" | "no-answer" | "canceled") {
            return;
        }
        {
            let mut t = self.transfers.lock();
            let Some(x) = t.get_mut(token) else { return };
            if !x.ended.iter().any(|l| l == leg) {
                x.ended.push(leg.to_string());
            }
        }
        tracing::info!(leg, status, "a desk number did not answer");
        self.give_up_if_all_failed(token).await;
    }

    async fn give_up_if_all_failed(&self, token: &str) {
        let (caller, twiml) = {
            let mut t = self.transfers.lock();
            let Some(x) = t.get_mut(token) else { return };
            if x.taken || !x.dialed || x.legs.is_empty() || !x.legs.iter().all(|l| x.ended.contains(l)) {
                return;
            }
            x.taken = true;
            (x.caller.clone(), x.give_up.clone())
        };
        tracing::info!(call = %caller, "every desk number refused or failed; telling the caller");
        let _ = self.telephony.redirect(&caller, &twiml).await;
    }

    /// A desk number answered (`leg` is its call). The first one gets the summary and the
    /// caller; the others are cancelled.
    pub fn answered(&self, token: &str, leg: &str) -> String {
        let (twiml, others) = {
            let mut t = self.transfers.lock();
            let Some(x) = t.get_mut(token) else { return twilio::twiml_hangup() };
            if x.taken {
                return twilio::twiml_hangup();
            }
            x.taken = true;
            tracing::info!(call = %x.caller, "the desk answered");
            let others: Vec<String> = x.legs.iter().filter(|l| *l != leg).cloned().collect();
            (twilio::twiml_desk_join(&x.summary, &x.language, &x.conference), others)
        };
        let telephony = self.telephony.clone();
        tokio::spawn(async move {
            for other in others {
                let _ = telephony.cancel(&other).await;
            }
        });
        twiml
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Telephony for Fake {
        async fn hangup(&self, sid: &str) -> anyhow::Result<()> {
            self.log.lock().push(format!("hangup {sid}"));
            Ok(())
        }
        async fn transfer(&self, _: &str, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn redirect(&self, sid: &str, twiml: &str) -> anyhow::Result<()> {
            self.log.lock().push(format!("redirect {sid} {twiml}"));
            Ok(())
        }
        async fn dial(&self, to: &str, from: &str, url: &str, _status: &str, _: u32) -> anyhow::Result<String> {
            self.log.lock().push(format!("dial {to} from {from} {url}"));
            Ok(format!("CA-{to}"))
        }
        async fn cancel(&self, sid: &str) -> anyhow::Result<()> {
            self.log.lock().push(format!("cancel {sid}"));
            Ok(())
        }
    }

    fn summary() -> HandoffSummary {
        HandoffSummary {
            reason: "caller_requested".into(),
            business_id: "taxi".into(),
            intent: None,
            pipeline: None,
            slots: vec![],
            customer_name: None,
            recent: vec![],
            text: "שיחה מועברת ממוניות קלורה.".into(),
        }
    }

    #[tokio::test]
    async fn the_caller_waits_with_music_and_the_first_desk_to_answer_takes_them() {
        let fake = Arc::new(Fake::default());
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let call = CallInfo {
            call_id: uuid::Uuid::from_u128(9),
            call_sid: "CAcaller".into(),
            business_id: "taxi".into(),
            from: Some("+972501111111".into()),
            to: "+972500000000".into(),
        };
        let settings =
            DeskSettings { numbers: vec!["+972502222222".into(), "+972503333333".into()], ..DeskSettings::default() };
        assert!(desk.transfer(&call, &summary(), &settings, "he-IL", "אין מוקדן").await);
        let log = fake.log.lock().clone();
        assert!(
            log[0].starts_with("redirect CAcaller")
                && log[0].contains("holdmusic")
                && log[0].contains("startConferenceOnEnter=\"false\""),
            "{}",
            log[0]
        );
        assert!(
            log[1].starts_with("dial +972502222222 from +972500000000 https://calls.example/webhooks/twilio/desk?t="),
            "{}",
            log[1]
        );
        assert!(log[2].starts_with("dial +972503333333"), "{}", log[2]);
        let token = log[1].split("?t=").nth(1).unwrap().to_string();

        let join = desk.answered(&token, "CA-+972503333333");
        assert!(
            join.contains("<Say language=\"he-IL\">שיחה מועברת") && join.contains("startConferenceOnEnter=\"true\""),
            "{join}"
        );
        assert!(desk.answered(&token, "CA-+972502222222").contains("<Hangup/>"), "the second one is too late");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(fake.log.lock().iter().any(|l| l == "cancel CA-+972502222222"), "the other number stops ringing");
    }

    #[tokio::test]
    async fn when_every_number_refuses_the_caller_is_told_at_once_not_after_the_wait() {
        let fake = Arc::new(Fake::default());
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let call = CallInfo {
            call_id: uuid::Uuid::from_u128(11),
            call_sid: "CAcaller".into(),
            business_id: "taxi".into(),
            from: None,
            to: "+972500000000".into(),
        };
        let settings =
            DeskSettings { numbers: vec!["+972502222222".into(), "+972503333333".into()], ..DeskSettings::default() };
        assert!(desk.transfer(&call, &summary(), &settings, "he-IL", "אין מוקדן פנוי").await);
        let token = fake.log.lock().iter().find_map(|l| l.split("?t=").nth(1).map(str::to_string)).unwrap();
        let told = || fake.log.lock().iter().filter(|l| l.contains("אין מוקדן פנוי")).count();

        desk.leg_ended(&token, "CA-+972502222222", "busy").await;
        assert_eq!(told(), 0, "one number is still ringing");
        desk.leg_ended(&token, "CA-+972502222222", "busy").await;
        assert_eq!(told(), 0, "the same number twice is still one");
        desk.leg_ended(&token, "CA-+972503333333", "failed").await;
        assert_eq!(told(), 1, "the last one refused: the caller is told now");
        desk.leg_ended(&token, "CA-+972503333333", "failed").await;
        assert_eq!(told(), 1, "and only once");
    }

    #[tokio::test]
    async fn a_refusal_after_someone_answered_changes_nothing() {
        let fake = Arc::new(Fake::default());
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let call = CallInfo {
            call_id: uuid::Uuid::from_u128(12),
            call_sid: "CAcaller".into(),
            business_id: "taxi".into(),
            from: None,
            to: "+972500000000".into(),
        };
        let settings =
            DeskSettings { numbers: vec!["+972502222222".into(), "+972503333333".into()], ..DeskSettings::default() };
        assert!(desk.transfer(&call, &summary(), &settings, "he-IL", "אין מוקדן פנוי").await);
        let token = fake.log.lock().iter().find_map(|l| l.split("?t=").nth(1).map(str::to_string)).unwrap();
        desk.answered(&token, "CA-+972502222222");
        desk.leg_ended(&token, "CA-+972503333333", "canceled").await;
        desk.leg_ended(&token, "CA-+972502222222", "completed").await;
        desk.leg_ended("unknown-token", "CA-x", "busy").await;
        assert!(!fake.log.lock().iter().any(|l| l.contains("אין מוקדן פנוי")), "the answered call is left alone");
    }

    #[tokio::test]
    async fn no_number_means_no_transfer() {
        let desk = Desk::new("https://calls.example".into(), Arc::new(Fake::default()));
        let call = CallInfo {
            call_id: uuid::Uuid::nil(),
            call_sid: "CA".into(),
            business_id: "taxi".into(),
            from: None,
            to: "+972500000000".into(),
        };
        assert!(!desk.transfer(&call, &summary(), &DeskSettings::default(), "he-IL", "").await);
    }
}
