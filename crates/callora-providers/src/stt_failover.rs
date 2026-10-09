//! A recognizer backed by another, for outages that start after the session opened.
//!
//! On 2026-10-08 OpenAI's credit ran out: the realtime session still opened, and every
//! transcription then failed (`insufficient_quota`), so the caller was heard by no one. A
//! fallback that only covered a session failing to open never took over.
//!
//! Here the primary is watched while the call runs. An error that says the service cannot
//! serve (no credit, a rejected key, rate limits) or two failed transcriptions in a row take
//! it out for a while: the session is closed for the call to reconnect, and every session that
//! opens until then (this call's and the next calls') is the backup's.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use callora_runtime::ports::{SpeechToText, SttEvent, SttInput, SttSession};

/// How long the primary stays out after it failed; then it is tried again.
pub const COOL_DOWN: Duration = Duration::from_secs(300);

pub struct SttFailover {
    primary: Arc<dyn SpeechToText>,
    backup: Arc<dyn SpeechToText>,
    down_until: Arc<Mutex<Option<Instant>>>,
    cool_down: Duration,
}

impl SttFailover {
    pub fn new(primary: Arc<dyn SpeechToText>, backup: Arc<dyn SpeechToText>) -> Self {
        Self::with_cool_down(primary, backup, COOL_DOWN)
    }

    pub fn with_cool_down(primary: Arc<dyn SpeechToText>, backup: Arc<dyn SpeechToText>, cool_down: Duration) -> Self {
        Self { primary, backup, down_until: Arc::new(Mutex::new(None)), cool_down }
    }

    fn primary_down(&self) -> bool {
        self.down_until.lock().is_some_and(|t| Instant::now() < t)
    }

    fn take_down(down_until: &Mutex<Option<Instant>>, cool_down: Duration) {
        *down_until.lock() = Some(Instant::now() + cool_down);
    }
}

/// An error that says the service cannot serve, not that one utterance went wrong.
pub fn is_outage(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "insufficient_quota",
        "quota",
        "billing",
        "credit",
        "payment",
        "402",
        "rate_limit",
        "429",
        "invalid_api_key",
        "unauthorized",
        "401",
        "403",
    ]
    .iter()
    .any(|k| e.contains(k))
}

#[async_trait]
impl SpeechToText for SttFailover {
    async fn open(&self, language: &str, keyterms: &[String]) -> anyhow::Result<SttSession> {
        if self.primary_down() {
            return self.backup.open(language, keyterms).await;
        }
        let session = match self.primary.open(language, keyterms).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(primary = self.primary.name(), error = %format!("{e:#}"), "speech recognition failed to open; using the backup");
                Self::take_down(&self.down_until, self.cool_down);
                return self.backup.open(language, keyterms).await;
            }
        };
        // Watch the primary's events on their way to the call.
        let SttSession { input, mut events } = session;
        let (tx, rx) = mpsc::channel(64);
        let down_until = self.down_until.clone();
        let cool_down = self.cool_down;
        let name = self.primary.name();
        let close = input.clone();
        tokio::spawn(async move {
            let mut failed_in_a_row = 0;
            while let Some(event) = events.recv().await {
                if let SttEvent::Error(e) = &event {
                    failed_in_a_row += 1;
                    if is_outage(e) || failed_in_a_row >= 2 {
                        tracing::error!(primary = name, error = %e, "speech recognition is failing; the backup takes over");
                        Self::take_down(&down_until, cool_down);
                        let _ = close.try_send(SttInput::Close);
                        let _ = tx.send(SttEvent::Closed).await;
                        return;
                    }
                } else if matches!(event, SttEvent::Final(_)) {
                    failed_in_a_row = 0;
                }
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(SttEvent::Closed).await;
        });
        Ok(SttSession { input, events: rx })
    }

    fn wants_business_words(&self) -> bool {
        self.primary.wants_business_words()
    }

    fn name(&self) -> &'static str {
        self.primary.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A recognizer that answers every finalize with its scripted events, and counts its opens.
    struct Fake {
        name: &'static str,
        opens: AtomicUsize,
        script: Vec<SttEvent>,
        refuse: bool,
    }

    impl Fake {
        fn new(name: &'static str, script: Vec<SttEvent>) -> Arc<Self> {
            Arc::new(Self { name, opens: AtomicUsize::new(0), script, refuse: false })
        }
    }

    #[async_trait]
    impl SpeechToText for Fake {
        async fn open(&self, _language: &str, _keyterms: &[String]) -> anyhow::Result<SttSession> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.refuse {
                anyhow::bail!("refused");
            }
            let (in_tx, mut in_rx) = mpsc::channel::<SttInput>(16);
            let (ev_tx, ev_rx) = mpsc::channel(16);
            let script = self.script.clone();
            tokio::spawn(async move {
                let mut events = script.into_iter();
                while let Some(i) = in_rx.recv().await {
                    match i {
                        SttInput::Finalize => {
                            if let Some(e) = events.next() {
                                if ev_tx.send(e).await.is_err() {
                                    return;
                                }
                            }
                        }
                        SttInput::Close => return,
                        SttInput::Audio(_) => {}
                    }
                }
            });
            Ok(SttSession { input: in_tx, events: ev_rx })
        }
        fn name(&self) -> &'static str {
            self.name
        }
    }

    async fn heard(s: &mut SttSession) -> SttEvent {
        s.input.send(SttInput::Finalize).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), s.events.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn out_of_credit_after_opening_hands_the_call_to_the_backup() {
        let primary = Fake::new(
            "openai",
            vec![
                SttEvent::Final("שלום".into()),
                SttEvent::Error("transcription failed: {\"code\":\"insufficient_quota\"}".into()),
            ],
        );
        let backup = Fake::new("deepgram", vec![SttEvent::Final("מאלעד".into())]);
        let stt = SttFailover::new(primary.clone(), backup.clone());
        let mut s = stt.open("he-IL", &[]).await.unwrap();
        assert!(matches!(heard(&mut s).await, SttEvent::Final(t) if t == "שלום"));
        assert!(matches!(heard(&mut s).await, SttEvent::Closed), "the call reconnects");
        // The reconnect, and the next calls, are the backup's.
        let mut s = stt.open("he-IL", &[]).await.unwrap();
        assert!(matches!(heard(&mut s).await, SttEvent::Final(t) if t == "מאלעד"));
        assert_eq!(primary.opens.load(Ordering::SeqCst), 1);
        assert_eq!(backup.opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn two_failures_in_a_row_are_an_outage_one_is_not() {
        let primary = Fake::new(
            "openai",
            vec![
                SttEvent::Error("transcription failed: server error".into()),
                SttEvent::Final("כן".into()),
                SttEvent::Error("transcription failed: server error".into()),
                SttEvent::Error("transcription failed: server error".into()),
            ],
        );
        let backup = Fake::new("deepgram", vec![]);
        let stt = SttFailover::new(primary, backup);
        let mut s = stt.open("he-IL", &[]).await.unwrap();
        assert!(matches!(heard(&mut s).await, SttEvent::Error(_)), "one failure passes through");
        assert!(matches!(heard(&mut s).await, SttEvent::Final(_)));
        assert!(matches!(heard(&mut s).await, SttEvent::Error(_)));
        assert!(matches!(heard(&mut s).await, SttEvent::Closed), "the second in a row closes");
        assert!(stt.primary_down());
    }

    #[tokio::test]
    async fn the_primary_is_tried_again_after_the_cool_down() {
        let mut refusing = Fake::new("openai", vec![]);
        Arc::get_mut(&mut refusing).unwrap().refuse = true;
        let backup = Fake::new("deepgram", vec![]);
        let stt = SttFailover::with_cool_down(refusing.clone(), backup.clone(), Duration::from_millis(50));
        stt.open("he-IL", &[]).await.unwrap();
        stt.open("he-IL", &[]).await.unwrap();
        assert_eq!(refusing.opens.load(Ordering::SeqCst), 1, "down: not asked again at once");
        tokio::time::sleep(Duration::from_millis(80)).await;
        stt.open("he-IL", &[]).await.unwrap();
        assert_eq!(refusing.opens.load(Ordering::SeqCst), 2, "tried again after the cool-down");
        assert_eq!(backup.opens.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn outages_are_told_apart_from_one_bad_utterance() {
        assert!(is_outage("transcription failed: {\"code\":\"insufficient_quota\"}"));
        assert!(is_outage("HTTP 429 Too Many Requests"));
        assert!(is_outage("invalid_api_key"));
        assert!(!is_outage("transcription failed: audio too short"));
    }
}
