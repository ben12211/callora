//! The seams between the runtime and the outside world. Providers implement these; tests
//! use in-process fakes. Nothing in a call's hot path waits on more than one of them at a
//! time, and every one of them can fail without ending the call.

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use callora_core::business::Business;
use callora_core::engine::HandoffSummary;
use callora_core::llm::LlmRequest;

/// Identity of one phone call, shared by everything that logs or stores about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallInfo {
    pub call_id: uuid::Uuid,
    pub call_sid: String,
    pub business_id: String,
    pub from: Option<String>,
    pub to: String,
}

// ---------------------------------------------------------------------------------------
// Speech to text

#[derive(Debug, Clone, PartialEq)]
pub enum SttInput {
    /// Raw μ-law 8 kHz from the caller.
    Audio(Bytes),
    /// The caller stopped talking (local endpoint): produce the final transcript now.
    Finalize,
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SttEvent {
    Partial(String),
    Final(String),
    Error(String),
    Closed,
}

pub struct SttSession {
    pub input: mpsc::Sender<SttInput>,
    pub events: mpsc::Receiver<SttEvent>,
}

#[async_trait]
pub trait SpeechToText: Send + Sync {
    /// Open a streaming recognition session for one call. `keyterms` are words the caller
    /// is likely to say (place and street names); recognizers that support biasing use them.
    async fn open(&self, language: &str, keyterms: &[String]) -> anyhow::Result<SttSession>;
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------------------
// LLM structured extraction

/// A reply as it is generated: text deltas.
pub type TextStream = futures::stream::BoxStream<'static, anyhow::Result<String>>;

#[async_trait]
pub trait LanguageModel: Send + Sync {
    /// Return JSON matching `request.schema`.
    async fn extract(&self, request: &LlmRequest) -> anyhow::Result<serde_json::Value>;

    /// The same reply, streamed as it is generated. By default it arrives all at once.
    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        let reply = self.extract(request).await?;
        Ok(Box::pin(futures::stream::once(async move { Ok(reply.to_string()) })))
    }

    /// Open the connection before the first real request needs it (a TLS handshake is
    /// ~100 ms on the reply path otherwise).
    async fn warm(&self) {}

    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------------------
// Business actions

#[async_trait]
pub trait ActionRunner: Send + Sync {
    async fn run(
        &self,
        business: &Business,
        action: &str,
        input: serde_json::Value,
        call: &CallInfo,
    ) -> Result<serde_json::Value, String>;
}

// ---------------------------------------------------------------------------------------
// Telephony control (out-of-band REST, not the media stream)

#[async_trait]
pub trait Telephony: Send + Sync {
    async fn hangup(&self, call_sid: &str) -> anyhow::Result<()>;
    /// Move the caller to a human. `whisper_url` is fetched by Twilio and played to the
    /// human before the caller is connected.
    async fn transfer(&self, call_sid: &str, to: &str, whisper_url: Option<&str>) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------------------
// Persistence (fire-and-forget; a slow database never slows a call)

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallRecord {
    Started {
        info: CallInfo,
    },
    Turn {
        call_id: uuid::Uuid,
        speaker: String,
        text: String,
        detail: serde_json::Value,
    },
    Action {
        call_id: uuid::Uuid,
        action: String,
        input: serde_json::Value,
        result: serde_json::Value,
        ok: bool,
        latency_ms: u64,
    },
    Handoff {
        call_id: uuid::Uuid,
        summary: HandoffSummary,
    },
    Ended {
        call_id: uuid::Uuid,
        outcome: String,
        state: serde_json::Value,
    },
    /// A completed task's card (see `callora_core::orders`).
    Order {
        call_id: uuid::Uuid,
        card: serde_json::Value,
    },
    Status {
        call_sid: String,
        status: String,
        duration_seconds: Option<i32>,
    },
}

pub trait CallStore: Send + Sync {
    fn record(&self, record: CallRecord);
}

/// Discards everything; used when no database is configured.
pub struct NullStore;

impl CallStore for NullStore {
    fn record(&self, _record: CallRecord) {}
}

/// Makes a handoff's context available to the human agent (as a whisper URL Twilio fetches
/// before connecting them). Returns the URL, or `None` when no public URL is configured.
pub trait WhisperRegistry: Send + Sync {
    fn register(&self, call: &CallInfo, summary: &HandoffSummary) -> Option<String>;
}

pub struct NoWhisper;

impl WhisperRegistry for NoWhisper {
    fn register(&self, _call: &CallInfo, _summary: &HandoffSummary) -> Option<String> {
        None
    }
}
