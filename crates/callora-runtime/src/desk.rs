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
    winner: Option<String>,
    connected: bool,
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
            if t.values().any(|x| x.caller == call.call_sid) {
                return true;
            }
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
                    winner: None,
                    connected: false,
                    at: Instant::now(),
                },
            );
        }
        let conference_url = format!("{}{}?t={token}", self.base, twilio::DESK_CONFERENCE_PATH);
        let wait = twilio::twiml_conference_wait_status(&conference, &desk.music_url(), Some(&conference_url));
        if let Err(e) = self.telephony.redirect(&call.call_sid, &wait).await {
            self.transfers.lock().remove(&token);
            tracing::error!(call = %call.call_sid, error = %e, "could not move the caller to the desk");
            return false;
        }
        self.arm_timeout(&token, desk.max_wait_seconds);
        let url = format!("{}{}?t={token}", self.base, twilio::DESK_PATH);
        let status_url = format!("{}{}?t={token}", self.base, twilio::DESK_STATUS_PATH);
        let ring = desk.max_wait_seconds.clamp(15, 60);
        let mut attempted = std::collections::HashSet::new();
        for (index, number) in desk.numbers.iter().enumerate() {
            if !attempted.insert(number) {
                continue;
            }
            if self.transfers.lock().get(&token).is_none_or(|x| x.taken) {
                break;
            }
            let from = desk.caller_id.as_deref().unwrap_or(&call.to);
            // Log the configured index, rather than the customer's or operator's phone number.
            tracing::info!(call = %call.call_sid, destination = index + 1, "ringing a desk number");
            match self.telephony.dial(number, from, &url, &status_url, ring).await {
                Ok(sid) => {
                    tracing::info!(call = %call.call_sid, leg = %sid, destination = index + 1, "desk call created");
                    let cancel = {
                        let mut transfers = self.transfers.lock();
                        match transfers.get_mut(&token) {
                            Some(t) => {
                                let cancel = t.taken && t.winner.as_deref() != Some(&sid);
                                t.legs.push(sid.clone());
                                cancel
                            }
                            None => true,
                        }
                    };
                    if cancel {
                        if let Err(e) = self.telephony.cancel(&sid).await {
                            tracing::warn!(leg = %sid, error = %e, "could not cancel a late desk call");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(call = %call.call_sid, destination = index + 1, error = %e, "could not ring a desk number")
                }
            }
        }
        let rung = {
            let mut t = self.transfers.lock();
            match t.get_mut(&token) {
                Some(x) => {
                    x.dialed = true;
                    if x.taken {
                        return true;
                    }
                    !x.legs.is_empty()
                }
                None => false,
            }
        };
        if !rung {
            if let Some(x) = self.transfers.lock().get_mut(&token) {
                x.taken = true;
            }
            tracing::info!(call = %call.call_sid, "no desk call could be created; telling the caller");
            self.fallback(&call.call_sid, &give_up).await;
            return true;
        }
        // A leg may have been refused while the others were still being dialed.
        self.give_up_if_all_failed(&token).await;
        true
    }

    fn arm_timeout(&self, token: &str, max_wait_seconds: u32) {
        // No one answered in time: tell the caller, and stop ringing.
        let (transfers, telephony, token) = (self.transfers.clone(), self.telephony.clone(), token.to_string());
        let wait_for = Duration::from_secs(u64::from(max_wait_seconds));
        tokio::spawn(async move {
            tokio::time::sleep(wait_for).await;
            let (caller, give_up, legs) = {
                let mut t = transfers.lock();
                match t.get_mut(&token) {
                    Some(x) if !x.taken || (x.winner.is_some() && !x.connected) => {
                        x.taken = true;
                        x.winner = None;
                        (x.caller.clone(), x.give_up.clone(), std::mem::take(&mut x.legs))
                    }
                    _ => return,
                }
            };
            tracing::info!(call = %caller, "no one at the desk answered");
            fallback(telephony.as_ref(), &caller, &give_up).await;
            for leg in legs {
                if let Err(e) = telephony.cancel(&leg).await {
                    tracing::warn!(%leg, error = %e, "could not stop desk call after timeout");
                }
            }
        });
    }

    /// A desk call ended without being answered (`status` is Twilio's: busy, failed,
    /// no-answer, canceled). When that was the last number, the caller is told at once.
    pub async fn leg_ended(&self, token: &str, leg: &str, status: &str) {
        // A completed call before a winner joined is also a failed transfer (e.g. an empty
        // answer webhook). Once a winner joined, all terminal callbacks leave it alone.
        if leg.is_empty() || !matches!(status, "busy" | "failed" | "no-answer" | "canceled" | "rejected" | "completed")
        {
            return;
        }
        let abandoned = {
            let mut t = self.transfers.lock();
            let Some(x) = t.get_mut(token) else { return };
            tracing::info!(call = %x.caller, %leg, %status, "desk call ended");
            if x.taken {
                if x.winner.as_deref() == Some(leg) && x.connected {
                    x.winner = None;
                    return;
                }
                if x.winner.as_deref() == Some(leg) && !x.connected {
                    x.winner = None;
                    Some((x.caller.clone(), x.give_up.clone()))
                } else {
                    return;
                }
            } else {
                if !x.ended.iter().any(|l| l == leg) {
                    x.ended.push(leg.to_string());
                }
                None
            }
        };
        if let Some((caller, twiml)) = abandoned {
            tracing::info!(call = %caller, "operator left before joining; telling the caller");
            self.fallback(&caller, &twiml).await;
            return;
        }
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
        self.fallback(&caller, &twiml).await;
    }

    async fn fallback(&self, caller: &str, twiml: &str) {
        fallback(self.telephony.as_ref(), caller, twiml).await;
    }

    /// Signed caller termination callback: stop ringing and invalidate late answer callbacks.
    pub async fn caller_ended(&self, caller: &str) {
        let legs: Vec<String> = {
            let mut transfers = self.transfers.lock();
            transfers
                .values_mut()
                .filter(|x| x.caller == caller)
                .flat_map(|x| {
                    x.taken = true;
                    x.winner = None;
                    std::mem::take(&mut x.legs)
                })
                .collect()
        };
        for leg in legs {
            if let Err(e) = self.telephony.cancel(&leg).await {
                tracing::warn!(%leg, error = %e, "could not stop desk call after caller ended");
            }
        }
    }

    /// A desk number answered (`leg` is its call). The first one gets the summary and the
    /// caller; the others are cancelled.
    pub fn answered(&self, token: &str, leg: &str) -> String {
        let status_url = format!("{}{}?t={token}", self.base, twilio::DESK_CONFERENCE_PATH);
        let (twiml, others) = {
            let mut t = self.transfers.lock();
            let Some(x) = t.get_mut(token) else { return twilio::twiml_hangup() };
            if x.winner.as_deref() == Some(leg) {
                return twilio::twiml_desk_join(&x.summary, &x.language, &x.conference, &status_url);
            }
            if x.taken || leg.is_empty() || x.ended.iter().any(|l| l == leg) {
                return twilio::twiml_hangup();
            }
            x.taken = true;
            x.winner = Some(leg.to_string());
            tracing::info!(call = %x.caller, %leg, "the desk answered");
            let others: Vec<String> = x.legs.iter().filter(|l| *l != leg).cloned().collect();
            (twilio::twiml_desk_join(&x.summary, &x.language, &x.conference, &status_url), others)
        };
        let telephony = self.telephony.clone();
        tokio::spawn(async move {
            for other in others {
                let _ = telephony.cancel(&other).await;
            }
        });
        twiml
    }

    pub async fn conference_event(&self, token: &str, event: &str) {
        let caller = {
            let mut transfers = self.transfers.lock();
            let Some(x) = transfers.get_mut(token) else { return };
            match event {
                "conference-start" if x.winner.is_some() => {
                    x.connected = true;
                    tracing::info!(call = %x.caller, "operator and caller connected");
                    return;
                }
                "conference-end" => x.caller.clone(),
                _ => return,
            }
        };
        self.caller_ended(&caller).await;
    }
}

async fn fallback(telephony: &dyn Telephony, caller: &str, twiml: &str) {
    if let Err(e) = telephony.redirect(caller, twiml).await {
        tracing::error!(call = %caller, error = %e, "desk fallback redirect failed; ending the hold");
        if let Err(e) = telephony.hangup(caller).await {
            tracing::error!(call = %caller, error = %e, "could not end failed desk transfer");
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        fail_dial: bool,
        fail_fallback: bool,
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
            if self.fail_fallback && twiml.contains("<Say") && twiml.contains("<Hangup/>") {
                anyhow::bail!("redirect unavailable");
            }
            Ok(())
        }
        async fn dial(&self, to: &str, from: &str, url: &str, _status: &str, _: u32) -> anyhow::Result<String> {
            self.log.lock().push(format!("dial {to} from {from} {url}"));
            if self.fail_dial {
                anyhow::bail!("call create refused");
            }
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

    fn call() -> CallInfo {
        CallInfo {
            call_id: uuid::Uuid::new_v4(),
            call_sid: "CAcaller".into(),
            business_id: "taxi".into(),
            from: None,
            to: "+972500000000".into(),
        }
    }

    #[tokio::test]
    async fn operator_hangup_during_the_whisper_falls_back_once() {
        let fake = Arc::new(Fake::default());
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let settings = DeskSettings { numbers: vec!["+972501234567".into()], ..Default::default() };
        desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await;
        let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
        let join = desk.answered(&token, "CA-+972501234567");
        assert_eq!(desk.answered(&token, "CA-+972501234567"), join, "a retry returns the same join, not a hangup");
        desk.leg_ended(&token, "CA-+972501234567", "completed").await;
        desk.leg_ended(&token, "CA-+972501234567", "completed").await;
        assert_eq!(fake.log.lock().iter().filter(|l| l.contains("אין מוקדן")).count(), 1);
        assert!(desk.answered(&token, "CA-+972501234567").contains("<Hangup/>"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_waits_for_a_conference_start_not_merely_an_answer() {
        for connected in [false, true] {
            let fake = Arc::new(Fake::default());
            let desk = Desk::new("https://calls.example".into(), fake.clone());
            let settings =
                DeskSettings { numbers: vec!["+972501234567".into()], max_wait_seconds: 15, ..Default::default() };
            desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await;
            let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
            assert!(fake.log.lock()[0].contains("desk-conference?t="));
            assert!(desk.answered(&token, "CA-+972501234567").contains("desk-conference?t="));
            if connected {
                desk.conference_event(&token, "conference-start").await;
            }
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(16)).await;
            tokio::task::yield_now().await;
            assert_eq!(fake.log.lock().iter().filter(|l| l.contains("אין מוקדן")).count(), usize::from(!connected));
            if connected {
                desk.conference_event(&token, "conference-end").await;
                assert!(desk.answered(&token, "CA-+972501234567").contains("<Hangup/>"));
            }
        }
    }

    #[tokio::test]
    async fn every_terminal_status_falls_back_once_and_a_late_answer_cannot_join() {
        for status in ["busy", "no-answer", "failed", "rejected", "canceled", "completed"] {
            let fake = Arc::new(Fake::default());
            let desk = Desk::new("https://calls.example".into(), fake.clone());
            let settings = DeskSettings { numbers: vec!["+972501234567".into()], ..Default::default() };
            desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await;
            let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
            desk.leg_ended(&token, "CA-+972501234567", status).await;
            desk.leg_ended(&token, "CA-+972501234567", status).await;
            assert_eq!(fake.log.lock().iter().filter(|l| l.contains("אין מוקדן")).count(), 1, "{status}");
            assert!(desk.answered(&token, "CA-+972501234567").contains("<Hangup/>"));
        }
    }

    #[tokio::test]
    async fn create_failures_fall_back_and_a_failed_redirect_hangs_up() {
        let fake = Arc::new(Fake { fail_dial: true, fail_fallback: true, ..Default::default() });
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let settings =
            DeskSettings { numbers: vec!["+972501234567".into(), "+972509876543".into()], ..Default::default() };
        assert!(desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await);
        let log = fake.log.lock();
        assert_eq!(log.iter().filter(|l| l.starts_with("dial ")).count(), 2);
        assert_eq!(log.iter().filter(|l| l.contains("אין מוקדן")).count(), 1);
        assert!(log.iter().any(|l| l == "hangup CAcaller"));
    }

    #[tokio::test]
    async fn caller_id_is_configurable_duplicate_transfers_do_not_dial_and_hangup_cancels() {
        let fake = Arc::new(Fake::default());
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let settings = DeskSettings {
            numbers: vec!["+972501234567".into()],
            caller_id: Some("+972509876543".into()),
            ..Default::default()
        };
        let call = call();
        desk.transfer(&call, &summary(), &settings, "he-IL", "אין מוקדן").await;
        desk.transfer(&call, &summary(), &settings, "he-IL", "אין מוקדן").await;
        assert_eq!(fake.log.lock().iter().filter(|l| l.starts_with("dial ")).count(), 1);
        assert!(fake.log.lock()[1].contains("from +972509876543"));
        let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
        desk.caller_ended(&call.call_sid).await;
        assert!(fake.log.lock().iter().any(|l| l == "cancel CA-+972501234567"));
        assert!(desk.answered(&token, "CA-+972501234567").contains("<Hangup/>"));
        desk.leg_ended(&token, "CA-+972501234567", "canceled").await;
        assert!(!fake.log.lock().iter().any(|l| l.contains("אין מוקדן")));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_falls_back_once_cancels_ringing_and_rejects_late_answers() {
        let fake = Arc::new(Fake::default());
        let desk = Desk::new("https://calls.example".into(), fake.clone());
        let settings =
            DeskSettings { numbers: vec!["+972501234567".into()], max_wait_seconds: 15, ..Default::default() };
        desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await;
        let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(16)).await;
        tokio::task::yield_now().await;
        assert_eq!(fake.log.lock().iter().filter(|l| l.contains("אין מוקדן")).count(), 1);
        assert!(fake.log.lock().iter().any(|l| l.starts_with("cancel ")));
        assert!(desk.answered(&token, "CA-+972501234567").contains("<Hangup/>"));
    }

    struct AnswerDuringDial {
        desk: Mutex<std::sync::Weak<Desk>>,
        dials: Mutex<usize>,
        canceled: Mutex<Vec<String>>,
        redirects: Mutex<Vec<String>>,
        stall: bool,
    }

    #[async_trait]
    impl Telephony for AnswerDuringDial {
        async fn hangup(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn transfer(&self, _: &str, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn redirect(&self, _: &str, twiml: &str) -> anyhow::Result<()> {
            self.redirects.lock().push(twiml.to_string());
            Ok(())
        }
        async fn dial(&self, _: &str, _: &str, url: &str, _: &str, _: u32) -> anyhow::Result<String> {
            let n = {
                let mut n = self.dials.lock();
                *n += 1;
                *n
            };
            if n == 2 {
                if self.stall {
                    tokio::time::sleep(Duration::from_secs(16)).await;
                } else {
                    let desk = self.desk.lock().upgrade().unwrap();
                    let token = url.split("?t=").nth(1).unwrap();
                    assert!(desk.answered(token, "CA1").contains("<Conference"));
                    assert!(desk.answered(token, "CA2").contains("<Hangup/>"));
                }
            }
            Ok(format!("CA{n}"))
        }
        async fn cancel(&self, sid: &str) -> anyhow::Result<()> {
            self.canceled.lock().push(sid.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_answer_during_call_creation_cancels_the_late_leg_and_stops_more_attempts() {
        let fake = Arc::new(AnswerDuringDial {
            desk: Mutex::new(std::sync::Weak::new()),
            dials: Mutex::new(0),
            canceled: Mutex::new(vec![]),
            redirects: Mutex::new(vec![]),
            stall: false,
        });
        let desk = Arc::new(Desk::new("https://calls.example".into(), fake.clone()));
        *fake.desk.lock() = Arc::downgrade(&desk);
        let settings = DeskSettings {
            numbers: vec!["+972501111111".into(), "+972502222222".into(), "+972503333333".into()],
            ..Default::default()
        };
        assert!(desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await);
        assert_eq!(*fake.dials.lock(), 2, "no third call after answer");
        assert_eq!(*fake.canceled.lock(), ["CA2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_includes_call_creation_and_cancels_a_leg_returned_after_the_deadline() {
        let fake = Arc::new(AnswerDuringDial {
            desk: Mutex::new(std::sync::Weak::new()),
            dials: Mutex::new(0),
            canceled: Mutex::new(vec![]),
            redirects: Mutex::new(vec![]),
            stall: true,
        });
        let desk = Arc::new(Desk::new("https://calls.example".into(), fake.clone()));
        *fake.desk.lock() = Arc::downgrade(&desk);
        let settings = DeskSettings {
            numbers: vec!["+972501111111".into(), "+972502222222".into(), "+972503333333".into()],
            max_wait_seconds: 15,
            ..Default::default()
        };
        let started = tokio::time::Instant::now();
        assert!(desk.transfer(&call(), &summary(), &settings, "he-IL", "אין מוקדן").await);
        assert_eq!(started.elapsed(), Duration::from_secs(16));
        assert_eq!(
            fake.redirects.lock().iter().filter(|x| x.contains("אין מוקדן")).count(),
            1,
            "fallback already ran during the pending request"
        );
        assert_eq!(*fake.dials.lock(), 2, "the third number is not dialed after timeout");
        assert_eq!(*fake.canceled.lock(), ["CA1", "CA2"]);
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
        let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
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
        let token = fake.log.lock()[1].split("?t=").nth(1).unwrap().to_string();
        desk.answered(&token, "CA-+972502222222");
        desk.conference_event(&token, "conference-start").await;
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
