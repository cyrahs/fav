//! The two site checks that need the login window itself rather than the cookies.
//!
//! Python origin: `_verify_rednote`, `_bilibili_extras`, `_BILIBILI_STORAGE_URLS` and
//! `_BILIBILI_TOKEN_WAIT_SECONDS` in `src/api/browser_login.py`. The cookie-only checks
//! (pixiv, X, NGA, Bilibili) are the sources' own `verify_session`s, wired in `fav-api`.

use std::time::Duration;

use fav_core::logging;
use fav_runtime::site_sessions::StoredCookie;
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::devtools::origin;
use crate::login::{LoginBrowser, LoginCheck, LoginError};
use crate::rednote::{LOGIN_PROBE_SCRIPT, truthy, user_id_from_probe};

fn log() -> logging::Logger {
    logging::get("browser-login")
}

/// How long the RedNote page gets to show anything after loading, and how often it is
/// looked at meanwhile (`_REDNOTE_SETTLE_SECONDS`, `_REDNOTE_SETTLE_POLL_SECONDS`).
pub const REDNOTE_SETTLE: Duration = Duration::from_secs(6);
pub const REDNOTE_SETTLE_POLL: Duration = Duration::from_millis(500);

/// `rednote_browser.read_login_probe(browser)` on a login window: an evaluate that lands
/// during a navigation, or a non-object answer, is an empty sample.
async fn read_login_probe(browser: &dyn LoginBrowser) -> Map<String, Value> {
    match browser.evaluate(LOGIN_PROBE_SCRIPT).await {
        Ok(Value::Object(probe)) => probe,
        Ok(_) => Map::new(),
        Err(error) => {
            logging::get("rednote").debug(format!("RedNote login probe landed mid-navigation: {error}"));
            Map::new()
        }
    }
}

/// `_verify_rednote(browser, cookies)`: ask the login window's own page whether it is
/// signed in, never a second client. A page that, once settled, shows neither a session
/// nor a login modal is what a finished scan leaves behind (the page's store still holds
/// the signed-out snapshot it was loaded with), so it is reloaded and the next poll reads
/// the answer from the fresh page.
pub async fn verify_rednote(
    browser: &dyn LoginBrowser,
    _cookies: &[StoredCookie],
    settle: Duration,
    poll: Duration,
) -> Result<LoginCheck, LoginError> {
    let deadline = Instant::now() + settle;
    loop {
        let probe = read_login_probe(browser).await;
        if truthy(probe.get("logged_in")) {
            return Ok(LoginCheck {
                ok: true,
                account: user_id_from_probe(&probe),
                message: String::new(),
            });
        }
        if truthy(probe.get("has_login_modal")) || truthy(probe.get("has_verify_modal")) || truthy(probe.get("qr_src")) {
            // Still signing in: the account QR, the phone form, or the security scan.
            return Ok(LoginCheck::default());
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(poll).await;
    }
    if let Err(error) = browser.reload().await {
        // The site navigating itself is the usual cause, and harmless.
        log().info(format_args!("Reloading the RedNote login page was interrupted: {}", error.class));
    }
    Ok(LoginCheck {
        ok: false,
        account: String::new(),
        message: "Reloaded the page to confirm the sign-in.".to_owned(),
    })
}

/// Where the Bilibili login page may have left the refresh token: the passport page
/// signs in on its own origin, the main site's login popup on www. A static file of each
/// origin is enough to read its `localStorage`.
pub const BILIBILI_STORAGE_URLS: [&str; 2] = ["https://passport.bilibili.com/robots.txt", "https://www.bilibili.com/robots.txt"];
/// `bilibili_session.REFRESH_TOKEN_STORAGE_KEY`: the page's `localStorage` key.
pub const BILIBILI_REFRESH_TOKEN_STORAGE_KEY: &str = "ac_time_value";
/// `bilibili_session.REFRESH_TOKEN_KEY`: the `extra` key it is stored under.
pub const BILIBILI_REFRESH_TOKEN_KEY: &str = "refresh_token";
/// `_BILIBILI_TOKEN_WAIT_SECONDS`.
pub const BILIBILI_TOKEN_WAIT: Duration = Duration::from_secs(10);

/// `_bilibili_extras(browser)`: the refresh token the login page keeps in `localStorage`,
/// which fav needs to refresh `SESSDATA`. The page writes it about when `SESSDATA` arrives,
/// so it is asked for every second until `wait` has passed.
pub async fn bilibili_extras(browser: &dyn LoginBrowser, wait: Duration) -> Result<Vec<(String, String)>, LoginError> {
    let deadline = Instant::now() + wait;
    loop {
        for url in BILIBILI_STORAGE_URLS {
            let token = browser.local_storage(url, BILIBILI_REFRESH_TOKEN_STORAGE_KEY).await?;
            if !token.is_empty() {
                log().info(format_args!("Captured the Bilibili refresh token from {}", origin(url)));
                return Ok(vec![(BILIBILI_REFRESH_TOKEN_KEY.to_owned(), token)]);
            }
        }
        if Instant::now() >= deadline {
            log().warning("The Bilibili login left no refresh token; fav will not be able to refresh SESSDATA itself");
            return Ok(Vec::new());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests;
