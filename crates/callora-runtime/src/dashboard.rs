//! The owner's dashboard: a password login that sets a signed session cookie, and the rules
//! that keep guessing slow. The site itself (`web/`, built into `WEB_DIR`) is served as
//! static files; its data comes from the admin API, which accepts the cookie as well as
//! `X-Api-Key`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub const COOKIE: &str = "callora_session";
/// A session lasts a working day.
pub const SESSION_SECONDS: i64 = 12 * 3600;
/// Wrong passwords from one address before it waits.
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

/// Signs and checks session cookies. The key is derived from a server secret and the
/// password, so sessions survive a restart and end when the password changes.
pub struct Sessions {
    password: String,
    key: Vec<u8>,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Sessions {
    pub fn new(password: &str, secret: &str) -> Self {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
        mac.update(b"dashboard-session:");
        mac.update(password.as_bytes());
        Self { password: password.to_string(), key: mac.finalize().into_bytes().to_vec(), failures: Mutex::default() }
    }

    /// Never true while no password is configured.
    pub fn password_matches(&self, given: &str) -> bool {
        !self.password.is_empty() && bool::from(given.as_bytes().ct_eq(self.password.as_bytes()))
    }

    fn sign(&self, expires: i64) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("hmac accepts any key");
        mac.update(format!("session.{expires}").as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    /// A session token valid until `now + SESSION_SECONDS`.
    pub fn issue(&self, now: i64) -> String {
        let expires = now + SESSION_SECONDS;
        format!("v1.{expires}.{}", self.sign(expires))
    }

    pub fn valid(&self, token: &str, now: i64) -> bool {
        let mut parts = token.splitn(3, '.');
        let (Some("v1"), Some(expires), Some(sig)) = (parts.next(), parts.next(), parts.next()) else { return false };
        let Ok(expires) = expires.parse::<i64>() else { return false };
        expires > now && bool::from(sig.as_bytes().ct_eq(self.sign(expires).as_bytes()))
    }

    /// Whether `client` may try a password now.
    pub fn allowed(&self, client: &str) -> bool {
        let mut failures = self.failures.lock();
        match failures.get(client) {
            Some(&(n, since)) if n >= MAX_FAILURES && since.elapsed() < LOCKOUT => false,
            Some(&(n, since)) if n >= MAX_FAILURES && since.elapsed() >= LOCKOUT => {
                failures.remove(client);
                true
            }
            _ => true,
        }
    }

    pub fn failed(&self, client: &str) {
        let mut failures = self.failures.lock();
        // A bounded map: an address that stopped trying long ago is forgotten.
        if failures.len() > 10_000 {
            failures.retain(|_, (_, since)| since.elapsed() < LOCKOUT);
        }
        let entry = failures.entry(client.to_string()).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    pub fn succeeded(&self, client: &str) {
        self.failures.lock().remove(client);
    }
}

/// The session token in a `Cookie` header, if any.
pub fn cookie_token(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').map(str::trim).find_map(|pair| pair.strip_prefix(COOKIE)?.strip_prefix('='))
}

/// `Set-Cookie` for a session: not readable by scripts, sent only to this site over HTTPS
/// (browsers allow it on http://localhost too).
pub fn set_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={SESSION_SECONDS}")
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_valid_until_it_expires_and_only_for_its_password() {
        let s = Sessions::new("12345678", "server-secret");
        let token = s.issue(1_000);
        assert!(s.valid(&token, 1_000 + SESSION_SECONDS - 1));
        assert!(!s.valid(&token, 1_000 + SESSION_SECONDS), "expired");
        assert!(!Sessions::new("another", "server-secret").valid(&token, 1_001), "the password changed");
        assert!(!Sessions::new("12345678", "other-secret").valid(&token, 1_001), "another server");
        let forged = token.replace("v1.", "v1.9").to_string();
        assert!(!s.valid(&forged, 1_001));
        assert!(!s.valid("garbage", 1_001));
    }

    #[test]
    fn guessing_is_stopped_after_five_wrong_passwords() {
        let s = Sessions::new("12345678", "k");
        assert!(s.password_matches("12345678") && !s.password_matches("1234567"));
        let unset = Sessions::new("", "k");
        assert!(!unset.password_matches("") && !unset.password_matches("12345678"), "no password, no sign-in");
        for _ in 0..5 {
            assert!(s.allowed("1.2.3.4"));
            s.failed("1.2.3.4");
        }
        assert!(!s.allowed("1.2.3.4"));
        assert!(s.allowed("5.6.7.8"), "per address");
        s.succeeded("1.2.3.4");
        assert!(s.allowed("1.2.3.4"));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        assert_eq!(cookie_token("a=1; callora_session=v1.5.x; b=2"), Some("v1.5.x"));
        assert_eq!(cookie_token("callora_session_other=1"), None);
        assert!(set_cookie("t").contains("HttpOnly") && set_cookie("t").contains("SameSite=Strict"));
    }
}
