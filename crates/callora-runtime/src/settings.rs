//! Settings the owner changes from the settings page, per business, without a deployment:
//! the dispatch desk the agent hands callers to. Kept in memory for the calls and in
//! PostgreSQL (`callora_v2.business_settings`) across restarts. A business with no saved
//! settings uses its `handoff.phone_number_env` number, as before.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use sqlx::Row;

use callora_core::business::{is_e164, Business};

use crate::agent_model::{AgentControl, AgentModelSettings};

/// Hold music Twilio hosts, by name; anything else must be an `https://` audio URL.
const MUSIC: [(&str, &str); 6] = [
    ("classical", "http://twimlets.com/holdmusic?Bucket=com.twilio.music.classical"),
    ("ambient", "http://twimlets.com/holdmusic?Bucket=com.twilio.music.ambient"),
    ("electronica", "http://twimlets.com/holdmusic?Bucket=com.twilio.music.electronica"),
    ("guitars", "http://twimlets.com/holdmusic?Bucket=com.twilio.music.guitars"),
    ("soft-rock", "http://twimlets.com/holdmusic?Bucket=com.twilio.music.soft-rock"),
    ("none", ""),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeskSettings {
    /// E.164 numbers rung at once; the first to answer takes the caller.
    #[serde(default)]
    pub numbers: Vec<String>,
    /// A Twilio-owned or verified E.164 caller ID; absent uses the called business number.
    #[serde(default)]
    pub caller_id: Option<String>,
    /// A name from [`MUSIC`] or an `https://` audio URL.
    #[serde(default = "default_music")]
    pub hold_music: String,
    /// How long the caller waits with the music before "no one is available".
    #[serde(default = "default_wait")]
    pub max_wait_seconds: u32,
}

fn default_music() -> String {
    "classical".into()
}
fn default_wait() -> u32 {
    60
}

impl Default for DeskSettings {
    fn default() -> Self {
        Self { numbers: Vec::new(), caller_id: None, hold_music: default_music(), max_wait_seconds: default_wait() }
    }
}

impl DeskSettings {
    /// The URL Twilio plays while the caller waits ("" for silence).
    pub fn music_url(&self) -> String {
        MUSIC
            .iter()
            .find(|(name, _)| *name == self.hold_music)
            .map_or_else(|| self.hold_music.clone(), |(_, url)| (*url).to_string())
    }

    /// Problems that keep these settings from being saved.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.caller_id.as_deref().is_some_and(|n| !is_e164(n)) {
            p.push("מספר הזיהוי היוצא חייב להיות בפורמט בינלאומי".into());
        }
        if self.numbers.len() > 10 {
            p.push("עד 10 מספרים".into());
        }
        for n in &self.numbers {
            if !is_e164(n) {
                p.push(format!("{n} אינו מספר טלפון תקין"));
            }
        }
        if !MUSIC.iter().any(|(name, _)| *name == self.hold_music) && !self.hold_music.starts_with("https://") {
            p.push("מוזיקת ההמתנה: אחת מהרשימה או קישור שמתחיל ב-https://".into());
        }
        if !(15..=600).contains(&self.max_wait_seconds) {
            p.push("זמן ההמתנה: בין 15 ל-600 שניות".into());
        }
        p
    }

    /// The names of the hosted music, for the settings page.
    pub fn music_names() -> Vec<&'static str> {
        MUSIC.iter().map(|(name, _)| *name).collect()
    }
}

/// The price-list bot a business asks prices of: the WhatsApp account that asks and the bot's
/// chat (a saved contact of that account).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceBotSettings {
    pub account: String,
    pub chat_id: String,
    /// The bot's name as the account knows it, for the settings page.
    #[serde(default)]
    pub chat_name: String,
}

/// Who answers the phone: Callora's own agent, or an agent built on the ElevenLabs Agents
/// platform (by its agent id). Chosen on the settings page, for the whole server.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallModeSettings {
    #[serde(default)]
    pub elevenlabs: bool,
    #[serde(default)]
    pub agent_id: String,
}

impl CallModeSettings {
    /// Problems that keep these settings from being saved.
    pub fn problems(&self) -> Vec<String> {
        if self.elevenlabs && !crate::eleven_agents::agent_id_valid(self.agent_id.trim()) {
            return vec!["צריך מזהה סוכן של ElevenLabs (agent_…)".into()];
        }
        Vec::new()
    }
}

#[derive(Default)]
pub struct SettingsStore {
    /// Who answers calls; Callora's own agent until the owner chooses ElevenLabs.
    call_mode: RwLock<CallModeSettings>,
    desks: RwLock<HashMap<String, DeskSettings>>,
    price_bots: RwLock<HashMap<String, PriceBotSettings>>,
    /// The voice chosen on the settings page (an ElevenLabs voice id).
    voices: RwLock<HashMap<String, String>>,
    /// The agent's model chosen on the settings page; none, the environment's.
    agent_model: RwLock<Option<AgentModelSettings>>,
    /// What changes the agent's model while calls run; set once the server has built it.
    agent_control: RwLock<Option<Arc<AgentControl>>>,
}

impl SettingsStore {
    /// Every business's saved settings.
    pub async fn load(pool: &PgPool) -> sqlx::Result<Self> {
        let rows =
            sqlx::query("SELECT business_id, settings FROM callora_v2.business_settings").fetch_all(pool).await?;
        let mut desks = HashMap::new();
        let mut price_bots = HashMap::new();
        let mut voices = HashMap::new();
        for r in rows {
            let settings: serde_json::Value = r.get("settings");
            if let Some(voice) = settings["voice"].as_str().filter(|v| !v.is_empty()) {
                voices.insert(r.get::<String, _>("business_id"), voice.to_string());
            }
            if let Ok(bot) = serde_json::from_value::<PriceBotSettings>(settings["price_bot"].clone()) {
                price_bots.insert(r.get::<String, _>("business_id"), bot);
            }
            match serde_json::from_value::<DeskSettings>(settings["desk"].clone()) {
                Ok(d) => {
                    desks.insert(r.get::<String, _>("business_id"), d);
                }
                Err(e) => tracing::warn!(error = %e, "unreadable saved settings; ignored"),
            }
        }
        // Tolerant: a database from before this setting has no such table yet.
        let agent_model = sqlx::query("SELECT value FROM callora_v2.app_settings WHERE key = 'agent_model'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|r| serde_json::from_value::<AgentModelSettings>(r.get("value")).ok())
            .filter(|m| m.problems().is_empty());
        let call_mode = sqlx::query("SELECT value FROM callora_v2.app_settings WHERE key = 'call_mode'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|r| serde_json::from_value::<CallModeSettings>(r.get("value")).ok())
            .filter(|m| m.problems().is_empty())
            .unwrap_or_default();
        Ok(Self {
            call_mode: RwLock::new(call_mode),
            desks: RwLock::new(desks),
            price_bots: RwLock::new(price_bots),
            voices: RwLock::new(voices),
            agent_model: RwLock::new(agent_model),
            agent_control: RwLock::new(None),
        })
    }

    /// The desk a call of this business hands off to: the saved settings, else the
    /// business file's number.
    pub fn desk(&self, business: &Business) -> DeskSettings {
        if let Some(d) = self.desks.read().get(&business.config.id) {
            return d.clone();
        }
        DeskSettings { numbers: business.handoff_number.iter().cloned().collect(), ..DeskSettings::default() }
    }

    /// The voice the owner chose, if any.
    pub fn voice(&self, business_id: &str) -> Option<String> {
        self.voices.read().get(business_id).cloned()
    }

    /// `None` goes back to the business's own voice.
    pub async fn save_voice(&self, pool: &PgPool, business_id: &str, voice: Option<String>) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO callora_v2.business_settings (business_id, settings) VALUES ($1, jsonb_build_object('voice', $2::jsonb))
             ON CONFLICT (business_id) DO UPDATE SET settings = callora_v2.business_settings.settings || jsonb_build_object('voice', $2::jsonb), updated_at = now()",
        )
        .bind(business_id)
        .bind(serde_json::to_value(&voice).unwrap_or_default())
        .execute(pool)
        .await?;
        match voice {
            Some(v) => self.voices.write().insert(business_id.to_string(), v),
            None => self.voices.write().remove(business_id),
        };
        Ok(())
    }

    /// Who answers calls now.
    pub fn call_mode(&self) -> CallModeSettings {
        self.call_mode.read().clone()
    }

    /// Use this mode for the calls that come next, without saving it.
    pub fn set_call_mode(&self, mode: CallModeSettings) {
        *self.call_mode.write() = mode;
    }

    /// Save the mode; the default (Callora answers) is no row at all.
    pub async fn save_call_mode(&self, pool: &PgPool, mode: CallModeSettings) -> sqlx::Result<()> {
        if mode.elevenlabs {
            sqlx::query(
                "INSERT INTO callora_v2.app_settings (key, value) VALUES ('call_mode', $1)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
            )
            .bind(serde_json::to_value(&mode).unwrap_or_default())
            .execute(pool)
            .await?;
        } else {
            sqlx::query("DELETE FROM callora_v2.app_settings WHERE key = 'call_mode'").execute(pool).await?;
        }
        self.set_call_mode(mode);
        Ok(())
    }

    /// The agent's model the owner chose, if any.
    pub fn agent_model(&self) -> Option<AgentModelSettings> {
        self.agent_model.read().clone()
    }

    /// `None` goes back to the environment's model.
    pub async fn save_agent_model(&self, pool: &PgPool, model: Option<AgentModelSettings>) -> sqlx::Result<()> {
        match &model {
            Some(m) => {
                sqlx::query(
                    "INSERT INTO callora_v2.app_settings (key, value) VALUES ('agent_model', $1)
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
                )
                .bind(serde_json::to_value(m).unwrap_or_default())
                .execute(pool)
                .await?;
            }
            None => {
                sqlx::query("DELETE FROM callora_v2.app_settings WHERE key = 'agent_model'").execute(pool).await?;
            }
        }
        *self.agent_model.write() = model;
        Ok(())
    }

    /// Called once by the server, when it has built the agent's model.
    pub fn attach_agent_control(&self, control: Arc<AgentControl>) {
        *self.agent_control.write() = Some(control);
    }

    /// The agent's model, switchable; none when the server has no model for the agent.
    pub fn agent_control(&self) -> Option<Arc<AgentControl>> {
        self.agent_control.read().clone()
    }

    pub fn price_bot(&self, business_id: &str) -> Option<PriceBotSettings> {
        self.price_bots.read().get(business_id).cloned()
    }

    /// `None` stops asking the bot.
    pub async fn save_price_bot(
        &self,
        pool: &PgPool,
        business_id: &str,
        bot: Option<PriceBotSettings>,
    ) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO callora_v2.business_settings (business_id, settings) VALUES ($1, jsonb_build_object('price_bot', $2::jsonb))
             ON CONFLICT (business_id) DO UPDATE SET settings = callora_v2.business_settings.settings || jsonb_build_object('price_bot', $2::jsonb), updated_at = now()",
        )
        .bind(business_id)
        .bind(serde_json::to_value(&bot).unwrap_or_default())
        .execute(pool)
        .await?;
        match bot {
            Some(b) => self.price_bots.write().insert(business_id.to_string(), b),
            None => self.price_bots.write().remove(business_id),
        };
        Ok(())
    }

    pub async fn save_desk(&self, pool: &PgPool, business_id: &str, desk: DeskSettings) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO callora_v2.business_settings (business_id, settings) VALUES ($1, jsonb_build_object('desk', $2::jsonb))
             ON CONFLICT (business_id) DO UPDATE SET settings = callora_v2.business_settings.settings || jsonb_build_object('desk', $2::jsonb), updated_at = now()",
        )
        .bind(business_id)
        .bind(serde_json::to_value(&desk).unwrap_or_default())
        .execute(pool)
        .await?;
        self.desks.write().insert(business_id.to_string(), desk);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_checked_before_they_are_saved() {
        let ok = DeskSettings { numbers: vec!["+972501234567".into()], ..DeskSettings::default() };
        assert!(ok.problems().is_empty());
        let old: DeskSettings =
            serde_json::from_str(r#"{"numbers":["+972501234567"],"hold_music":"none","max_wait_seconds":15}"#).unwrap();
        assert_eq!(old.caller_id, None, "saved settings remain compatible");
        assert!(!DeskSettings { caller_id: Some("054-1234567".into()), ..Default::default() }.problems().is_empty());
        assert!(ok.music_url().contains("classical"));
        let bad = DeskSettings {
            numbers: vec!["050-1234567".into()],
            hold_music: "http://x".into(),
            max_wait_seconds: 5,
            ..DeskSettings::default()
        };
        assert_eq!(bad.problems().len(), 3, "{:?}", bad.problems());
        let custom = DeskSettings { hold_music: "https://example.com/wait.mp3".into(), ..DeskSettings::default() };
        assert_eq!(custom.music_url(), "https://example.com/wait.mp3");
        assert_eq!(DeskSettings { hold_music: "none".into(), ..DeskSettings::default() }.music_url(), "");
    }
}
