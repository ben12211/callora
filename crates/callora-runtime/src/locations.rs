//! A link a caller opens on their phone to send the call their location.
//!
//! A pickup heard wrong twice ("עריף", "בני ברק ו...") is the call's hardest problem: the
//! phone knows where the caller stands. The call texts them a link (`/l/<token>`, valid while
//! the call lasts); the page asks the browser for its position and posts it back, and the call
//! takes it as the place, with a map link for the driver. The token is random and maps to one
//! live call only.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;

/// A link stops working after this long, or when its call ends.
const LINK_LIFE: Duration = Duration::from_secs(15 * 60);

/// Where a link's positions go, and when it was made.
type Link = (mpsc::UnboundedSender<(f64, f64)>, Instant);

pub struct LocationLinks {
    base_url: String,
    links: Mutex<HashMap<String, Link>>,
}

impl LocationLinks {
    pub fn new(base_url: &str) -> Self {
        Self { base_url: base_url.trim_end_matches('/').to_string(), links: Mutex::new(HashMap::new()) }
    }

    /// A new link for a call; the positions posted to it arrive on `to`.
    pub fn create(&self, to: mpsc::UnboundedSender<(f64, f64)>) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut links = self.links.lock();
        links.retain(|_, (tx, at)| at.elapsed() < LINK_LIFE && !tx.is_closed());
        links.insert(token.clone(), (to, Instant::now()));
        format!("{}/l/{token}", self.base_url)
    }

    /// A position posted to a link: delivered to its call, when the link is still live.
    pub fn deliver(&self, token: &str, lat: f64, lon: f64) -> bool {
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return false;
        }
        let links = self.links.lock();
        match links.get(token) {
            Some((tx, at)) if at.elapsed() < LINK_LIFE => tx.send((lat, lon)).is_ok(),
            _ => false,
        }
    }

    pub fn is_live(&self, token: &str) -> bool {
        self.links.lock().get(token).is_some_and(|(tx, at)| at.elapsed() < LINK_LIFE && !tx.is_closed())
    }
}

/// The page behind the link, in Hebrew: one button, the browser's position, posted back.
pub const PAGE: &str = r#"<!doctype html>
<html lang="he" dir="rtl"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>שליחת מיקום למונית</title>
<style>
body{font-family:system-ui,-apple-system,sans-serif;margin:0;padding:24px 16px;background:#f6f7f9;color:#111;text-align:center}
@media (prefers-color-scheme:dark){body{background:#111;color:#eee}}
button{font-size:1.3rem;padding:18px 28px;border:0;border-radius:14px;background:#1f7a3f;color:#fff;width:100%;max-width:420px}
p{font-size:1.1rem;line-height:1.5}
</style></head><body>
<h1>שליחת מיקום למונית</h1>
<p>לחיצה על הכפתור שולחת לנו את המיקום שלך, כדי שהנהג יגיע בדיוק אליך.</p>
<button id="send">שלח את המיקום שלי</button>
<p id="status"></p>
<script>
const status = document.getElementById('status');
document.getElementById('send').onclick = () => {
  if (!navigator.geolocation) { status.textContent = 'הטלפון לא מאפשר לשלוח מיקום. אפשר להגיד את הכתובת בשיחה.'; return; }
  status.textContent = 'מאתר מיקום...';
  navigator.geolocation.getCurrentPosition(async (p) => {
    try {
      const r = await fetch(location.pathname, { method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ lat: p.coords.latitude, lon: p.coords.longitude }) });
      status.textContent = r.ok ? 'המיקום נשלח. אפשר לחזור לשיחה.' : 'הקישור כבר לא בתוקף. אפשר להגיד את הכתובת בשיחה.';
    } catch { status.textContent = 'השליחה נכשלה. אפשר להגיד את הכתובת בשיחה.'; }
  }, () => { status.textContent = 'לא התקבלה הרשאה למיקום. אפשר להגיד את הכתובת בשיחה.'; },
  { enableHighAccuracy: true, timeout: 15000 });
};
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_reaches_only_its_live_call() {
        let links = LocationLinks::new("https://calls.example.test/");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let url = links.create(tx);
        assert!(url.starts_with("https://calls.example.test/l/"));
        let token = url.rsplit('/').next().unwrap();
        assert!(links.deliver(token, 32.08, 34.95));
        assert_eq!(rx.try_recv().unwrap(), (32.08, 34.95));
        assert!(!links.deliver("not-a-token", 32.0, 34.0));
        assert!(!links.deliver(token, 132.0, 34.0), "no such place");
        drop(rx);
        assert!(!links.is_live(token), "its call ended");
    }
}
