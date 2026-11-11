//! The sites the 登录态 page offers, how each one's login is checked, and how its stored
//! session is shown.
//!
//! Python origin: `_verify_pixiv`, `_verify_bilibili`, `_verify_nga`, `_verify_twitter`,
//! `_rednote_proxy`, `LOGIN_SITES`, `bilibili_login_site`, `login_sites` and
//! `rednote_running` in `src/api/browser_login.py`, and `_session_view` in
//! `src/api/routes.py`. The login window and its manager are `fav_browser::login`; the
//! cookie checks are the sources' own `verify_session`s. Every setting a site reads is
//! loaded when it is needed, as Python's lambdas do, so a settings edit applies to the
//! next login without a restart. `rust/goldens/api/login_sites.json` and
//! `rust/goldens/browser_login/login_sites.json` hold Python's lists.

use std::path::PathBuf;
use std::sync::Arc;

use fav_browser::login::{BusyCheck, LoginCheck, LoginError, LoginSite, LoginSites};
use fav_browser::{profile, rednote, site_checks};
use fav_core::pyjson::Value;
use fav_core::settings::Settings;
use fav_db::{Db, SettingsStore};
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::{SiteSession, StoredCookie, cookie_value};
use fav_sources::{bilibili, nga_post, pixiv, twitter};

use crate::json;

/// Loads the settings snapshot a site reads when it needs it.
pub type LoadSettings = Arc<dyn Fn() -> BoxFuture<'static, Result<Arc<Settings>, LoginError>> + Send + Sync>;

/// `settings.load()` from the API's store.
pub fn settings_loader(store: Arc<SettingsStore>) -> LoadSettings {
    Arc::new(move || {
        let store = Arc::clone(&store);
        Box::pin(async move {
            store
                .load()
                .await
                .map_err(|error| LoginError::new(error_class(&error), error.to_string()))
        })
    })
}

/// The Python exception a failed `settings.load()` would have raised.
fn error_class(error: &fav_db::settings_store::SettingsStoreError) -> &'static str {
    match error {
        fav_db::settings_store::SettingsStoreError::Db(_) => "OperationalError",
        _ => "ValidationError",
    }
}

fn check(ok: bool, account: String, message: String) -> LoginCheck {
    LoginCheck { ok, account, message }
}

const TWITTER_HINT: &str = "用 X 账号（要和 X 设置里的用户名是同一个）登录；X 多半会再要邮箱或手机验证码，按页面提示输入即可。";
const NGA_HINT: &str = "用 NGA 账号登录（账号密码或手机验证码）；微信源里转发的 NGA 帖子靠这个登录读取。";
const REDNOTE_HINT: &str = "用手机小红书扫画面里的二维码，或用手机号和验证码登录；之后多半还会弹一个安全验证码，同样用 App 扫。";

fn built(site: Result<LoginSite, LoginError>) -> LoginSite {
    // Every site below names a verifier, which is all `build` checks.
    site.unwrap_or_else(|error| unreachable!("{error}"))
}

fn pixiv_site(load: &LoadSettings) -> LoginSite {
    let verify_load = Arc::clone(load);
    let proxy_load = Arc::clone(load);
    built(
        LoginSite::builder(
            pixiv::SESSION_SITE,
            "Pixiv",
            pixiv::LOGIN_URL,
            &pixiv::SESSION_COOKIE_URLS,
            "PHPSESSID",
        )
        .verify(Arc::new(move |cookies: Vec<StoredCookie>| {
            let load = Arc::clone(&verify_load);
            Box::pin(async move {
                let settings = load().await?;
                let cfg = &settings.web.pixiv;
                let found = pixiv::verify_session(cookie_value(&cookies, "PHPSESSID"), &cfg.user_id, &cfg.proxy).await;
                Ok(check(found.ok, found.account, found.message))
            })
        }))
        .proxy(Arc::new(move || {
            let load = Arc::clone(&proxy_load);
            Box::pin(async move { Ok(load().await?.web.pixiv.proxy.clone()) })
        }))
        .build(),
    )
}

fn twitter_site(load: &LoadSettings) -> LoginSite {
    let verify_load = Arc::clone(load);
    let proxy_load = Arc::clone(load);
    built(
        LoginSite::builder(
            twitter::SESSION_SITE,
            "X",
            twitter::LOGIN_URL,
            &twitter::SESSION_COOKIE_URLS,
            twitter::SESSION_COOKIE,
        )
        .verify(Arc::new(move |cookies: Vec<StoredCookie>| {
            let load = Arc::clone(&verify_load);
            Box::pin(async move {
                let settings = load().await?;
                let cfg = &settings.web.twitter;
                let found = twitter::verify_session(&cookies, &cfg.username, &cfg.proxy).await;
                Ok(check(found.ok, found.account, found.message))
            })
        }))
        .proxy(Arc::new(move || {
            let load = Arc::clone(&proxy_load);
            Box::pin(async move { Ok(load().await?.web.twitter.proxy.clone()) })
        }))
        .hint(TWITTER_HINT)
        .build(),
    )
}

fn nga_site() -> LoginSite {
    built(
        LoginSite::builder(
            nga_post::SESSION_SITE,
            "NGA",
            nga_post::LOGIN_URL,
            &nga_post::SESSION_COOKIE_URLS,
            nga_post::SESSION_COOKIE,
        )
        .verify(Arc::new(|cookies: Vec<StoredCookie>| {
            Box::pin(async move {
                let found = nga_post::verify_session(&cookies).await;
                Ok(check(found.ok, found.account, found.message))
            })
        }))
        .hint(NGA_HINT)
        .build(),
    )
}

/// `_rednote_proxy()`: the crawl refuses to leave from the pod's own address, and so does
/// its login.
async fn rednote_proxy(load: &LoadSettings) -> Result<String, LoginError> {
    let settings = load().await?;
    let cfg = &settings.web.rednote;
    if !cfg.validate_runnable().is_empty() {
        return Err(LoginError::login(
            "RedNote has no proxy configured (web.rednote.proxy); set one before signing in.",
        ));
    }
    Ok(cfg.proxy.clone())
}

async fn rednote_profile(load: &LoadSettings) -> Result<PathBuf, LoginError> {
    Ok(load().await?.web.rednote.profile_path.as_path().to_path_buf())
}

fn rednote_site(load: &LoadSettings) -> LoginSite {
    let proxy_load = Arc::clone(load);
    let profile_load = Arc::clone(load);
    let lock_load = Arc::clone(load);
    built(
        LoginSite::builder(
            rednote::SESSION_SITE,
            "小红书",
            rednote::LOGIN_URL,
            &[rednote::WEB_ORIGIN],
            rednote::SESSION_COOKIE,
        )
        .verify_in_browser(Arc::new(|browser, cookies| {
            Box::pin(site_checks::verify_rednote(
                browser,
                cookies,
                site_checks::REDNOTE_SETTLE,
                site_checks::REDNOTE_SETTLE_POLL,
            ))
        }))
        .proxy(Arc::new(move || {
            let load = Arc::clone(&proxy_load);
            Box::pin(async move { rednote_proxy(&load).await })
        }))
        .profile_dir(Arc::new(move || {
            let load = Arc::clone(&profile_load);
            Box::pin(async move { rednote_profile(&load).await })
        }))
        // The very lock the crawl takes, or the two could open the profile at once.
        .lock_name(Arc::new(move || {
            let load = Arc::clone(&lock_load);
            Box::pin(async move { Ok(profile::rednote_profile_lock_name(&rednote_profile(&load).await?)) })
        }))
        .prepare_page(Arc::new(|context, page| Box::pin(rednote::prepare_page(context, page))))
        .store_cookie_values(false)
        .hint(REDNOTE_HINT)
        .build(),
    )
}

/// `bilibili_login_site`: one per account, keyed by the account's name. The name only
/// appears in the label when there is more than one to tell apart.
fn bilibili_login_site(account_name: &str, only_account: bool) -> LoginSite {
    let label = if only_account {
        "Bilibili".to_owned()
    } else {
        format!("Bilibili（{account_name}）")
    };
    built(
        LoginSite::builder(
            &bilibili::session_site(account_name),
            &label,
            bilibili::LOGIN_URL,
            &bilibili::SESSION_COOKIE_URLS,
            bilibili::SESSION_COOKIE,
        )
        .verify(Arc::new(|cookies: Vec<StoredCookie>| {
            Box::pin(async move {
                let found = bilibili::verify_session(&cookies).await;
                Ok(check(found.ok, found.account, found.message))
            })
        }))
        .after_login(Arc::new(|browser| {
            Box::pin(site_checks::bilibili_extras(browser, site_checks::BILIBILI_TOKEN_WAIT))
        }))
        .hint(&format!(
            "用要给账号 {account_name} 抓取的 B 站账号登录：B 站 App 扫码，或手机号加验证码。"
        ))
        .build(),
    )
}

/// `login_sites()`: the fixed sites, then one per Bilibili account of `settings`. What a
/// site reads later (its proxy, its profile, the verifier's settings) comes from `load`.
pub fn login_sites(settings: &Settings, load: &LoadSettings) -> Vec<LoginSite> {
    let mut sites = vec![pixiv_site(load), twitter_site(load), nga_site(), rednote_site(load)];
    let accounts = &settings.web.bilibili.accounts;
    for account in accounts {
        let site = bilibili_login_site(&account.name, accounts.len() == 1);
        // A later site with the same key replaces the earlier one in place, as in a dict.
        if let Some(existing) = sites.iter_mut().find(|existing| existing.key == site.key) {
            *existing = site;
        } else {
            sites.push(site);
        }
    }
    sites
}

/// The site list the login manager asks for each time: [`login_sites`] of the settings
/// as they are now.
pub struct FavLoginSites {
    load: LoadSettings,
}

impl std::fmt::Debug for FavLoginSites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FavLoginSites").finish_non_exhaustive()
    }
}

impl FavLoginSites {
    pub fn new(load: LoadSettings) -> Self {
        Self { load }
    }
}

impl LoginSites for FavLoginSites {
    fn sites(&self) -> BoxFuture<'_, Result<Vec<LoginSite>, LoginError>> {
        Box::pin(async move {
            let settings = (self.load)().await?;
            Ok(login_sites(&settings, &self.load))
        })
    }
}

/// `rednote_running()`: refuse a login while the RedNote crawl has its own Chromium up.
/// Both browsers share the pod's memory limit, and a login can always wait the few
/// minutes a RedNote run takes. The check is a courtesy: a database error lets it pass.
pub fn rednote_running(db: Db) -> BusyCheck {
    Arc::new(move || {
        let db = db.clone();
        Box::pin(async move {
            let running: Result<Option<i32>, sqlx::Error> =
                sqlx::query_scalar("SELECT 1 FROM control_requests WHERE status = 'running' AND target = 'rednote' LIMIT 1;")
                    .fetch_optional(db.pool())
                    .await;
            match running {
                Ok(Some(_)) => "RedNote is crawling with its own browser right now; try again in a few minutes.".to_owned(),
                Ok(None) | Err(_) => String::new(),
            }
        })
    })
}

/// `_session_view`: a site and its stored session, never with cookie values.
pub fn session_view(site: &LoginSite, stored: Option<&SiteSession>) -> Value {
    let (signed_in, account, status, message, cookie_count, expires_at, signed_in_at, checked_at) = match stored {
        None => (false, "", "", "", 0, None, None, None),
        Some(stored) => (
            !stored.cookie(&site.session_cookie).is_empty(),
            stored.account.as_str(),
            stored.status.as_str(),
            stored.message.as_str(),
            stored.cookies.len(),
            stored.expires_at.as_ref(),
            stored.signed_in_at.as_ref(),
            stored.checked_at.as_ref(),
        ),
    };
    json::dict([
        ("site", Value::Str(site.key.clone())),
        ("label", Value::Str(site.label.clone())),
        ("signed_in", Value::Bool(signed_in)),
        ("hint", Value::Str(site.hint.clone())),
        ("account", Value::from(account)),
        ("status", Value::from(status)),
        ("message", Value::from(message)),
        ("cookie_count", Value::Int(i128::try_from(cookie_count).unwrap_or(i128::MAX))),
        ("expires_at", json::utc_z_opt(expires_at)),
        ("signed_in_at", json::utc_z_opt(signed_in_at)),
        ("checked_at", json::utc_z_opt(checked_at)),
    ])
}

/// `_login_view(attempt)`: the login in progress, or the last one. The ticket is only
/// shown while the login is open.
pub fn login_view(attempt: Option<&fav_browser::login::LoginAttempt>) -> Value {
    let Some(attempt) = attempt else {
        return Value::Null;
    };
    let progress = attempt.progress();
    let active = progress.finished_at.is_none();
    json::dict([
        ("site", Value::Str(attempt.site.clone())),
        ("state", Value::from(progress.state)),
        ("active", Value::Bool(active)),
        ("message", Value::Str(progress.message)),
        ("account", Value::Str(progress.account)),
        ("started_at", json::utc_z_opt(Some(&attempt.started_at))),
        ("deadline", json::utc_z_opt(Some(&attempt.deadline))),
        ("finished_at", json::utc_z_opt(progress.finished_at.as_ref())),
        ("ticket", Value::Str(if active { attempt.ticket.clone() } else { String::new() })),
    ])
}

#[cfg(test)]
mod tests;
