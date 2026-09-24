//! Twilio: webhook signatures, TwiML, the Media Streams wire protocol, and the short-lived
//! token that binds a media stream to the call that the signed webhook authorized.
//!
//! Webhook paths and the token scheme follow the legacy deployment, so the numbers already
//! configured in the Twilio console keep working unchanged.

use std::collections::BTreeMap;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub const VOICE_PATH: &str = "/webhooks/twilio/voice";
pub const STATUS_PATH: &str = "/webhooks/twilio/call-status";
pub const MEDIA_PATH: &str = "/webhooks/twilio/media";
pub const WHISPER_PATH: &str = "/webhooks/twilio/handoff-whisper";

/// `X-Twilio-Signature`: base64(HMAC-SHA1(auth_token, url + sorted(key + value)...)).
pub fn signature(auth_token: &str, url: &str, params: &BTreeMap<String, String>) -> String {
    let mut data = url.to_string();
    for (k, v) in params {
        data.push_str(k);
        data.push_str(v);
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()).expect("hmac accepts any key");
    mac.update(data.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

pub fn signature_valid(auth_token: &str, url: &str, params: &BTreeMap<String, String>, provided: &str) -> bool {
    let expected = signature(auth_token, url, params);
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

// ---------------------------------------------------------------------------------------
// Stream token

const TOKEN_LABEL: &str = "callora:media-stream:v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamToken {
    pub call_sid: String,
    pub business_id: String,
    pub expires_at: i64,
}

fn sign_token(secret: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
    mac.update(format!("{TOKEN_LABEL}.{payload}").as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn create_stream_token(secret: &str, call_sid: &str, business_id: &str, ttl_seconds: i64, now: i64) -> String {
    let token =
        StreamToken { call_sid: call_sid.into(), business_id: business_id.into(), expires_at: now + ttl_seconds };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&token).unwrap_or_default());
    format!("{payload}.{}", sign_token(secret, &payload))
}

/// Checks against every secret (so a secret can be rotated without dropping calls).
pub fn verify_stream_token(secrets: &[String], token: &str, now: i64) -> Option<StreamToken> {
    let (payload, sig) = token.split_once('.')?;
    let mut ok = false;
    for s in secrets {
        ok |= bool::from(sign_token(s, payload).as_bytes().ct_eq(sig.as_bytes()));
    }
    if !ok {
        return None;
    }
    let token: StreamToken = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (token.expires_at > now).then_some(token)
}

// ---------------------------------------------------------------------------------------
// TwiML

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

pub fn twiml_stream(media_url: &str, token: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Response><Connect><Stream url="{}"><Parameter name="token" value="{}"/></Stream></Connect></Response>"#,
        xml_escape(media_url),
        xml_escape(token)
    )
}

pub fn twiml_say_hangup(text: &str, language: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Response><Say language="{}">{}</Say><Hangup/></Response>"#,
        xml_escape(language),
        xml_escape(text)
    )
}

pub fn twiml_say(text: &str, language: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Response><Say language="{}">{}</Say></Response>"#,
        xml_escape(language),
        xml_escape(text)
    )
}

pub fn twiml_dial(to: &str, whisper_url: Option<&str>) -> String {
    let number = match whisper_url {
        Some(url) => format!(r#"<Number url="{}">{}</Number>"#, xml_escape(url), xml_escape(to)),
        None => format!("<Number>{}</Number>", xml_escape(to)),
    };
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><Response><Dial>{number}</Dial></Response>"#)
}

// ---------------------------------------------------------------------------------------
// Media Streams protocol

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum StreamMessage {
    Connected,
    Start { start: StartInfo },
    Media { media: MediaPayload },
    Mark { mark: MarkInfo },
    Dtmf,
    Stop,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartInfo {
    pub stream_sid: String,
    pub call_sid: String,
    #[serde(default)]
    pub custom_parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaPayload {
    #[serde(default)]
    pub track: Option<String>,
    pub payload: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkInfo {
    pub name: String,
}

impl MediaPayload {
    pub fn audio(&self) -> Option<Bytes> {
        STANDARD.decode(&self.payload).ok().map(Bytes::from)
    }
}

pub fn media_message(stream_sid: &str, audio: &[u8]) -> String {
    serde_json::json!({ "event": "media", "streamSid": stream_sid, "media": { "payload": STANDARD.encode(audio) } })
        .to_string()
}

pub fn clear_message(stream_sid: &str) -> String {
    serde_json::json!({ "event": "clear", "streamSid": stream_sid }).to_string()
}

pub fn mark_message(stream_sid: &str, name: &str) -> String {
    serde_json::json!({ "event": "mark", "streamSid": stream_sid, "mark": { "name": name } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_twilio_reference_example() {
        // The worked example from Twilio's security documentation.
        let mut params = BTreeMap::new();
        for (k, v) in [
            ("CallSid", "CA1234567890ABCDE"),
            ("Caller", "+12349013030"),
            ("Digits", "1234"),
            ("From", "+12349013030"),
            ("To", "+18005551212"),
        ] {
            params.insert(k.to_string(), v.to_string());
        }
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let sig = signature("12345", url, &params);
        assert_eq!(sig, "0/KCTR6DLpKmkAf8muzZqo1nDgQ=");
        assert!(signature_valid("12345", url, &params, &sig));
        assert!(!signature_valid("12345", url, &params, "nope"));
    }

    #[test]
    fn stream_token_round_trip_and_expiry() {
        let secrets = vec!["new-secret".to_string(), "old-secret".to_string()];
        let t = create_stream_token("old-secret", "CA1", "taxi", 300, 1000);
        assert_eq!(verify_stream_token(&secrets, &t, 1100).unwrap().call_sid, "CA1");
        assert!(verify_stream_token(&secrets, &t, 1400).is_none(), "expired");
        assert!(verify_stream_token(&["other".into()], &t, 1100).is_none(), "wrong secret");
        let tampered = t.replacen('e', "f", 1);
        assert!(verify_stream_token(&secrets, &tampered, 1100).is_none() || tampered == t);
    }

    #[test]
    fn parses_media_stream_messages() {
        let start: StreamMessage = serde_json::from_str(r#"{"event":"start","sequenceNumber":"1","start":{"streamSid":"MZ1","accountSid":"AC1","callSid":"CA1","tracks":["inbound"],"customParameters":{"token":"abc"},"mediaFormat":{"encoding":"audio/x-mulaw","sampleRate":8000,"channels":1}},"streamSid":"MZ1"}"#).unwrap();
        let StreamMessage::Start { start } = start else { panic!() };
        assert_eq!(start.custom_parameters["token"], "abc");
        let media: StreamMessage = serde_json::from_str(r#"{"event":"media","media":{"track":"inbound","chunk":"2","timestamp":"5","payload":"/w=="},"streamSid":"MZ1"}"#).unwrap();
        let StreamMessage::Media { media } = media else { panic!() };
        assert_eq!(media.audio().unwrap().as_ref(), &[0xFF]);
    }
}
