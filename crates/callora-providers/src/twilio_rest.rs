//! Twilio REST: hang a call up, or redirect it to a human. The CallSid always comes from
//! the server-side stream authorization, never from the caller or a model.

use async_trait::async_trait;

use callora_runtime::ports::Telephony;
use callora_runtime::twilio::twiml_dial;

pub const DEFAULT_BASE_URL: &str = "https://api.twilio.com";

pub struct TwilioRest {
    http: reqwest::Client,
    account_sid: String,
    auth_token: String,
    base_url: String,
}

impl TwilioRest {
    pub fn new(http: reqwest::Client, account_sid: String, auth_token: String, base_url: Option<String>) -> Self {
        Self { http, account_sid, auth_token, base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into()) }
    }

    async fn update(&self, call_sid: &str, form: &[(&str, &str)]) -> anyhow::Result<()> {
        let url = format!("{}/2010-04-01/Accounts/{}/Calls/{}.json", self.base_url, self.account_sid, call_sid);
        let resp = self.http.post(url).basic_auth(&self.account_sid, Some(&self.auth_token)).form(form).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let code = body.get("code").and_then(serde_json::Value::as_i64);
        // The call is already over (caller hung up first): that is the outcome we wanted.
        if status.as_u16() == 404 || matches!(code, Some(20404 | 20009 | 21220)) {
            return Ok(());
        }
        anyhow::bail!("Twilio call update failed with HTTP {status} (code {code:?})")
    }
}

#[async_trait]
impl Telephony for TwilioRest {
    async fn hangup(&self, call_sid: &str) -> anyhow::Result<()> {
        self.update(call_sid, &[("Status", "completed")]).await
    }

    async fn transfer(&self, call_sid: &str, to: &str, whisper_url: Option<&str>) -> anyhow::Result<()> {
        let twiml = twiml_dial(to, whisper_url);
        self.update(call_sid, &[("Twiml", &twiml)]).await
    }

    async fn redirect(&self, call_sid: &str, twiml: &str) -> anyhow::Result<()> {
        self.update(call_sid, &[("Twiml", twiml)]).await
    }

    async fn dial(&self, to: &str, from: &str, url: &str, ring_seconds: u32) -> anyhow::Result<String> {
        let endpoint = format!("{}/2010-04-01/Accounts/{}/Calls.json", self.base_url, self.account_sid);
        let timeout = ring_seconds.to_string();
        let form = [("To", to), ("From", from), ("Url", url), ("Method", "POST"), ("Timeout", timeout.as_str())];
        let resp =
            self.http.post(endpoint).basic_auth(&self.account_sid, Some(&self.auth_token)).form(&form).send().await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Twilio call create failed with HTTP {status}: {}", body["message"].as_str().unwrap_or(""));
        }
        body["sid"].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("Twilio returned no call sid"))
    }

    async fn cancel(&self, call_sid: &str) -> anyhow::Result<()> {
        // "canceled" stops a call still ringing; "completed" ends one already answered.
        if self.update(call_sid, &[("Status", "canceled")]).await.is_err() {
            self.update(call_sid, &[("Status", "completed")]).await?;
        }
        Ok(())
    }
}
