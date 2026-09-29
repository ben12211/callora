//! Settings the owner changes from the settings page, per business, without a deployment:
//! the dispatch desk the agent hands callers to. Kept in memory for the calls and in
//! PostgreSQL (`callora_v2.business_settings`) across restarts. A business with no saved
//! settings uses its `handoff.phone_number_env` number, as before.

use std::collections::HashMap;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use sqlx::Row;

use callora_core::business::{is_e164, Business};

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

#[derive(Default)]
pub struct SettingsStore {
    desks: RwLock<HashMap<String, DeskSettings>>,
}

impl SettingsStore {
    /// Every business's saved settings.
    pub async fn load(pool: &PgPool) -> sqlx::Result<Self> {
        let rows =
            sqlx::query("SELECT business_id, settings FROM callora_v2.business_settings").fetch_all(pool).await?;
        let mut desks = HashMap::new();
        for r in rows {
            let settings: serde_json::Value = r.get("settings");
            match serde_json::from_value::<DeskSettings>(settings["desk"].clone()) {
                Ok(d) => {
                    desks.insert(r.get::<String, _>("business_id"), d);
                }
                Err(e) => tracing::warn!(error = %e, "unreadable saved settings; ignored"),
            }
        }
        Ok(Self { desks: RwLock::new(desks) })
    }

    /// The desk a call of this business hands off to: the saved settings, else the
    /// business file's number.
    pub fn desk(&self, business: &Business) -> DeskSettings {
        if let Some(d) = self.desks.read().get(&business.config.id) {
            return d.clone();
        }
        DeskSettings { numbers: business.handoff_number.iter().cloned().collect(), ..DeskSettings::default() }
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
