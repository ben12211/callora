//! ElevenLabs Agents as the other way to answer a call.
//!
//! By default Callora answers itself: its own agent, engine and voice. The owner can instead
//! hand the phone call to an agent built on the ElevenLabs Agents platform (its own recognition,
//! model, voice and turn-taking, billed by ElevenLabs). The handoff is ElevenLabs' register-call:
//! it takes the call's numbers and returns the TwiML that connects the call to the agent.
//!
//! Nothing here can leave a caller unanswered: when ElevenLabs does not answer in time or
//! refuses, the voice webhook goes on with Callora's own agent.

use std::time::Duration;

use anyhow::Context;

pub const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";

/// How long a ringing caller can wait for ElevenLabs to name the agent's stream.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(4);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ElevenAgents {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

/// An agent id as ElevenLabs writes them (`agent_…`): letters, digits, `_` and `-`, so it can
/// never change the URL it is put in.
pub fn agent_id_valid(id: &str) -> bool {
    (3..=128).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl ElevenAgents {
    pub fn new(http: reqwest::Client, api_key: String, base_url: Option<String>) -> Self {
        let base_url = base_url.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self { http, api_key, base_url: base_url.trim_end_matches('/').to_string() }
    }

    /// The TwiML that connects this call to the agent.
    pub async fn register_call(&self, agent_id: &str, from: &str, to: &str) -> anyhow::Result<String> {
        anyhow::ensure!(agent_id_valid(agent_id), "the agent id is not valid");
        let resp = self
            .http
            .post(format!("{}/v1/convai/twilio/register-call", self.base_url))
            .header("xi-api-key", &self.api_key)
            .timeout(REGISTER_TIMEOUT)
            .json(&serde_json::json!({
                "agent_id": agent_id,
                "from_number": from,
                "to_number": to,
                "direction": "inbound",
            }))
            .send()
            .await
            .context("ElevenLabs register-call did not answer")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::ensure!(
            status.is_success(),
            "ElevenLabs register-call failed with HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        );
        twiml_from(&body)
    }

    /// The agent's name when ElevenLabs knows it, else what to tell the owner (in Hebrew).
    pub async fn check_agent(&self, agent_id: &str) -> Result<String, String> {
        if !agent_id_valid(agent_id) {
            return Err("מזהה הסוכן לא נראה כמו מזהה של ElevenLabs (agent_…)".into());
        }
        let resp = self
            .http
            .get(format!("{}/v1/convai/agents/{agent_id}", self.base_url))
            .header("xi-api-key", &self.api_key)
            .timeout(CHECK_TIMEOUT)
            .send()
            .await
            .map_err(|_| "ElevenLabs לא ענה. נסה שוב".to_string())?;
        match resp.status().as_u16() {
            200..=299 => {
                let v: serde_json::Value = resp.json().await.unwrap_or_default();
                Ok(v["name"].as_str().unwrap_or("").to_string())
            }
            401 | 403 => Err("המפתח של ElevenLabs בשרת אינו מורשה לקרוא סוכנים".into()),
            404 | 422 => Err("ElevenLabs לא מכיר סוכן עם המזהה הזה".into()),
            other => Err(format!("ElevenLabs ענה {other}")),
        }
    }
}

/// The reply of register-call is the TwiML, as text or as a JSON string.
fn twiml_from(body: &str) -> anyhow::Result<String> {
    let trimmed = body.trim();
    let twiml = if trimmed.starts_with('"') {
        serde_json::from_str::<String>(trimmed).context("register-call returned an unreadable string")?
    } else {
        trimmed.to_string()
    };
    anyhow::ensure!(twiml.contains("<Response"), "register-call did not return TwiML");
    Ok(twiml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_cannot_change_the_url() {
        assert!(agent_id_valid("agent_0123abcXYZ"));
        assert!(!agent_id_valid(""));
        assert!(!agent_id_valid("agent/../x"));
        assert!(!agent_id_valid("a b"));
        assert!(!agent_id_valid("agent?x=1"));
    }

    #[test]
    fn twiml_comes_as_text_or_as_a_json_string() {
        let t = "<?xml version=\"1.0\"?><Response><Connect><Stream url=\"wss://x\"/></Connect></Response>";
        assert_eq!(twiml_from(t).unwrap(), t);
        let quoted = serde_json::to_string(t).unwrap();
        assert_eq!(twiml_from(&quoted).unwrap(), t);
        assert!(twiml_from("{\"detail\":\"no\"}").is_err());
    }
}
