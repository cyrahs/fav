//! The sites the 登录态 page lists, and how each one's stored session is shown.
//!
//! Python origin: `LOGIN_SITES`, `bilibili_login_site` and `login_sites` in
//! `src/api/browser_login.py`, and `_session_view` in `src/api/routes.py`. Only what the
//! session list needs is here (key, label, hint, the cookie that marks a login); the login
//! window itself arrives with WP16. `rust/goldens/api/login_sites.json` holds Python's lists.

use fav_core::pyjson::Value;
use fav_core::settings::Settings;
use fav_runtime::site_sessions::SiteSession;

use crate::json;

/// One site the 登录态 page offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSite {
    pub key: String,
    pub label: String,
    /// One line for the page: how this site's login goes.
    pub hint: String,
    /// The cookie whose presence means the stored session is a login.
    pub session_cookie: String,
}

impl LoginSite {
    fn fixed(key: &str, label: &str, session_cookie: &str, hint: &str) -> Self {
        Self {
            key: key.to_owned(),
            label: label.to_owned(),
            hint: hint.to_owned(),
            session_cookie: session_cookie.to_owned(),
        }
    }
}

/// `bilibili_login_site`: one per account, keyed by the account's name.
fn bilibili_login_site(account_name: &str, only_account: bool) -> LoginSite {
    LoginSite {
        key: format!("bilibili-{account_name}"),
        label: if only_account {
            "Bilibili".to_owned()
        } else {
            format!("Bilibili（{account_name}）")
        },
        hint: format!("用要给账号 {account_name} 抓取的 B 站账号登录：B 站 App 扫码，或手机号加验证码。"),
        session_cookie: "SESSDATA".to_owned(),
    }
}

/// `login_sites()`: the fixed sites, then one per Bilibili account.
pub fn login_sites(settings: &Settings) -> Vec<LoginSite> {
    let mut sites = vec![
        LoginSite::fixed("pixiv", "Pixiv", "PHPSESSID", ""),
        LoginSite::fixed(
            "twitter",
            "X",
            "auth_token",
            "用 X 账号（要和 X 设置里的用户名是同一个）登录；X 多半会再要邮箱或手机验证码，按页面提示输入即可。",
        ),
        LoginSite::fixed(
            "nga",
            "NGA",
            "ngaPassportCid",
            "用 NGA 账号登录（账号密码或手机验证码）；微信源里转发的 NGA 帖子靠这个登录读取。",
        ),
        LoginSite::fixed(
            "rednote",
            "小红书",
            "web_session",
            "用手机小红书扫画面里的二维码，或用手机号和验证码登录；之后多半还会弹一个安全验证码，同样用 App 扫。",
        ),
    ];
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

#[cfg(test)]
mod tests {
    use fav_core::golden;
    use fav_core::pyjson::{Dict, Value};
    use fav_core::settings::build_settings;

    use super::login_sites;

    #[test]
    fn login_sites_match_python() {
        let document = golden::load("api/login_sites.json");
        for case in golden::cases(&document) {
            let accounts: Vec<Value> = golden::list(golden::get(case, "bilibili_accounts"))
                .iter()
                .map(|name| crate::json::dict([("name", name.clone())]))
                .collect();
            let mut section = Dict::new();
            section.insert("accounts", Value::List(accounts));
            let section = Value::Dict(section);
            let settings = build_settings([("web.bilibili", &section)]).expect("valid settings");
            let sites: Vec<Value> = login_sites(&settings)
                .into_iter()
                .map(|site| {
                    crate::json::dict([
                        ("key", Value::Str(site.key)),
                        ("label", Value::Str(site.label)),
                        ("hint", Value::Str(site.hint)),
                        ("session_cookie", Value::Str(site.session_cookie)),
                    ])
                })
                .collect();
            assert_eq!(Value::List(sites), *golden::get(case, "sites"), "{case:?}");
        }
    }
}
