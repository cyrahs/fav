//! The settings models: one section per source plus notifications and shared services.
//!
//! Python origin: `src/core/settings.py` (the models, `SECTION_MODELS`, `SENSITIVE_FIELDS`,
//! `build_settings`, `notify_enabled`). The `app_settings` store itself is in `fav-db`.
//!
//! Every model validates like its pydantic counterpart (lax coercion, defaults that are
//! not validated, field validators in declaration order, model validators only when every
//! field passed) and dumps like `json.loads(model.model_dump_json())`, field order
//! included. Every section is constructible from its defaults so a fresh database boots;
//! what a source needs before it can run is reported by `validate_runnable`.

use crate::pyjson::{Dict, Value};
use crate::pytext::{py_isdigit, py_lstrip_chars, py_rstrip_chars, py_split, py_strip};
use crate::validate::{
    self, Errors, Fields, LiteralStr, LocItem, Model, PyPath, ValidationError, after, boolean, dump_float, float, int, list_of, literal,
    nullable, path, string,
};

const CRON_FIELDS: usize = 5;

fn is_account_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn is_twitter_username(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Order-preserving dedupe, like the validators' `if x in deduped: continue` loops.
fn dedupe<T: PartialEq>(values: Vec<T>) -> Vec<T> {
    let mut deduped: Vec<T> = Vec::with_capacity(values.len());
    for value in values {
        if !deduped.contains(&value) {
            deduped.push(value);
        }
    }
    deduped
}

/// `str` with surrounding whitespace removed (an `after` validator that never fails).
fn stripped_string(value: &Value, loc: &[LocItem]) -> Result<String, Errors> {
    string(value, loc).map(|text| py_strip(&text).to_owned())
}

fn non_negative_seconds(field: &'static str) -> impl FnOnce(f64) -> Result<f64, String> {
    move |value| {
        if value < 0.0 {
            Err(format!("{field} cannot be negative"))
        } else {
            Ok(value)
        }
    }
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(key, value);
    }
    Value::Dict(dict)
}

fn paths(value: &PyPath) -> Value {
    Value::from(value)
}

fn models<M: Model>(items: &[M]) -> Value {
    Value::List(items.iter().map(Model::dump).collect())
}

// ---------- ScheduleJob ----------

/// The fields every source section starts with (`ScheduleJob`).
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    /// Standard five-field crontab: minute hour day month `day_of_week`.
    pub cron: String,
    /// Off by default, so an unconfigured deployment starts up idle.
    pub enabled: bool,
    /// On by default: a source that runs reports what it found.
    pub notify: bool,
}

fn cron_check(value: String) -> Result<String, String> {
    if py_split(&value).count() == CRON_FIELDS {
        Ok(value)
    } else {
        Err("cron must contain exactly 5 fields".to_owned())
    }
}

impl Schedule {
    /// Validate the three leading fields; `cron` is required only on the bare `ScheduleJob`.
    fn read(fields: &mut Fields<'_>, default_cron: Option<&str>) -> Option<Self> {
        let cron = match default_cron {
            Some(default) => fields.optional("cron", || default.to_owned(), after(string, cron_check)),
            None => fields.required("cron", after(string, cron_check)),
        };
        let enabled = fields.optional("enabled", || false, boolean);
        let notify = fields.optional("notify", || true, boolean);
        Some(Self {
            cron: cron?,
            enabled: enabled?,
            notify: notify?,
        })
    }

    fn dump_into(&self, entries: &mut Vec<(&'static str, Value)>) {
        entries.push(("cron", Value::Str(self.cron.clone())));
        entries.push(("enabled", Value::Bool(self.enabled)));
        entries.push(("notify", Value::Bool(self.notify)));
    }
}

/// `ScheduleJob` on its own, with `cron` required.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleJob {
    pub schedule: Schedule,
}

impl Model for ScheduleJob {
    const CLASS_NAME: &'static str = "ScheduleJob";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, None);
        fields.finish(|| Some(Self { schedule: schedule? }), |_| Ok(()))
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        dict(entries)
    }
}

impl ScheduleJob {
    pub fn validate_runnable(&self) -> Vec<String> {
        Vec::new()
    }
}

// ---------- Bilibili ----------

/// One favourite list and the directory its videos land in.
#[derive(Debug, Clone, PartialEq)]
pub struct BilibiliFavorite {
    pub fav_id: i64,
    pub path: PyPath,
}

impl Model for BilibiliFavorite {
    const CLASS_NAME: &'static str = "BilibiliFavorite";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let fav_id = fields.required(
            "fav_id",
            after(int, |value| {
                if value <= 0 {
                    Err("bilibili fav_id must be a positive favourite list id".to_owned())
                } else {
                    Ok(value)
                }
            }),
        );
        let path = fields.required("path", path);
        fields.finish(
            || {
                Some(Self {
                    fav_id: fav_id?,
                    path: path?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![("fav_id", Value::from(self.fav_id)), ("path", paths(&self.path))])
    }
}

/// One logged-in Bilibili account, its favourite lists, and its watch-later list.
#[derive(Debug, Clone, PartialEq)]
pub struct BilibiliAccount {
    pub name: String,
    pub favorites: Vec<BilibiliFavorite>,
    pub toview_enabled: bool,
    pub toview_path: PyPath,
}

fn account_name(kind: &'static str) -> impl FnOnce(String) -> Result<String, String> {
    move |value| {
        let normalized = py_strip(&value);
        if normalized.is_empty() {
            return Err(format!("{kind} account name cannot be empty"));
        }
        if !is_account_name(normalized) {
            return Err(format!(
                "{kind} account name must contain only ASCII letters, digits, underscores, or hyphens"
            ));
        }
        Ok(normalized.to_owned())
    }
}

impl Model for BilibiliAccount {
    const CLASS_NAME: &'static str = "BilibiliAccount";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let name = fields.required("name", after(string, account_name("bilibili")));
        let favorites = fields.optional(
            "favorites",
            Vec::new,
            after(list_of(validate::model::<BilibiliFavorite>), |favorites: Vec<BilibiliFavorite>| {
                let mut seen = Vec::new();
                for favorite in &favorites {
                    if seen.contains(&favorite.fav_id) {
                        return Err(format!("duplicate bilibili favourite list id: {}", favorite.fav_id));
                    }
                    seen.push(favorite.fav_id);
                }
                Ok(favorites)
            }),
        );
        let toview_enabled = fields.optional("toview_enabled", || false, boolean);
        let toview_path = fields.optional("toview_path", || PyPath::new("./collection/bilibili/toview"), path);
        fields.finish(
            || {
                Some(Self {
                    name: name?,
                    favorites: favorites?,
                    toview_enabled: toview_enabled?,
                    toview_path: toview_path?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("name", Value::Str(self.name.clone())),
            ("favorites", models(&self.favorites)),
            ("toview_enabled", Value::Bool(self.toview_enabled)),
            ("toview_path", paths(&self.toview_path)),
        ])
    }
}

impl BilibiliAccount {
    pub fn validate_runnable(&self) -> Vec<String> {
        // An account that collects neither favourites nor watch-later has nothing to do.
        if self.favorites.is_empty() && !self.toview_enabled {
            vec!["favorites".to_owned()]
        } else {
            Vec::new()
        }
    }
}

/// Duplicate account names, compared like Python's `casefold()` (names are ASCII by then).
fn duplicate_account(kind: &str, names: impl Iterator<Item = String>) -> Result<(), String> {
    let mut seen: Vec<String> = Vec::new();
    for name in names {
        let folded = name.to_lowercase();
        if seen.contains(&folded) {
            return Err(format!("duplicate {kind} account name: {name}"));
        }
        seen.push(folded);
    }
    Ok(())
}

/// `web.bilibili`.
#[derive(Debug, Clone, PartialEq)]
pub struct Bilibili {
    pub schedule: Schedule,
    /// Egress for the yt-dlp downloads only; steers Bilibili's CDN mirror assignment.
    pub proxy: String,
    pub accounts: Vec<BilibiliAccount>,
}

impl Model for Bilibili {
    const CLASS_NAME: &'static str = "Bilibili";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("*/30 * * * *"));
        let proxy = fields.optional("proxy", String::new, stripped_string);
        let accounts = fields.optional("accounts", Vec::new, list_of(validate::model::<BilibiliAccount>));
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    proxy: proxy?,
                    accounts: accounts?,
                })
            },
            |model| duplicate_account("bilibili", model.accounts.iter().map(|account| account.name.clone())),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("proxy", Value::Str(self.proxy.clone())));
        entries.push(("accounts", models(&self.accounts)));
        dict(entries)
    }
}

impl Bilibili {
    pub fn validate_runnable(&self) -> Vec<String> {
        if self.accounts.is_empty() {
            return vec!["accounts".to_owned()];
        }
        self.accounts
            .iter()
            .enumerate()
            .flat_map(|(index, account)| {
                account
                    .validate_runnable()
                    .into_iter()
                    .map(move |field| format!("accounts[{index}].{field}"))
            })
            .collect()
    }
}

// ---------- Telegram ----------

/// What a Telegram channel route saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TelegramMediaType {
    Video,
    Image,
}

impl LiteralStr for TelegramMediaType {
    const VARIANTS: &'static [(Self, &'static str)] = &[(Self::Video, "video"), (Self::Image, "image")];

    fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Image => "image",
        }
    }
}

/// `_normalize_telegram_channel_id`: a Bot API id `-100<id>` becomes the Telethon id.
///
/// Python does this on the decimal text, so `-100` alone fails in `int('')`, whose message
/// pydantic reports as is.
fn normalize_telegram_channel_id(channel_id: i64) -> Result<i64, String> {
    let mut channel_id = channel_id;
    let text = channel_id.to_string();
    if channel_id < 0
        && let Some(rest) = text.strip_prefix("-100")
    {
        if rest.is_empty() {
            return Err("invalid literal for int() with base 10: ''".to_owned());
        }
        channel_id = rest
            .parse()
            .map_err(|_| format!("invalid literal for int() with base 10: '{rest}'"))?;
    }
    if channel_id <= 0 {
        return Err("telegram channel id must be a positive Telethon channel id or a -100 Bot API channel id".to_owned());
    }
    Ok(channel_id)
}

/// One channel route: a channel, the media it saves, and where they land.
#[derive(Debug, Clone, PartialEq)]
pub struct TelegramChannel {
    pub id: i64,
    pub path: PyPath,
    pub media_types: Vec<TelegramMediaType>,
    /// Report messages that hold nothing to save (for channels fed on purpose).
    pub report_unsupported: bool,
}

impl Model for TelegramChannel {
    const CLASS_NAME: &'static str = "TelegramChannel";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let id = fields.required("id", after(int, normalize_telegram_channel_id));
        let path = fields.required("path", path);
        let media_types = fields.optional(
            "media_types",
            || vec![TelegramMediaType::Video],
            after(list_of(literal::<TelegramMediaType>), |value| {
                let deduped = dedupe(value);
                if deduped.is_empty() {
                    Err("telegram channel media_types cannot be empty".to_owned())
                } else {
                    Ok(deduped)
                }
            }),
        );
        let report_unsupported = fields.optional("report_unsupported", || false, boolean);
        fields.finish(
            || {
                Some(Self {
                    id: id?,
                    path: path?,
                    media_types: media_types?,
                    report_unsupported: report_unsupported?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("id", Value::from(self.id)),
            ("path", paths(&self.path)),
            (
                "media_types",
                Value::List(self.media_types.iter().map(|media| Value::from(media.as_str())).collect()),
            ),
            ("report_unsupported", Value::Bool(self.report_unsupported)),
        ])
    }
}

/// One Telegram account and its channel routes.
#[derive(Clone, PartialEq)]
pub struct TelegramAccount {
    pub name: String,
    pub channels: Vec<TelegramChannel>,
    pub api_id: i64,
    pub api_hash: String,
    pub session_path: PyPath,
}

/// The routes of one channel id: media type to channel, in first-seen order.
pub type MediaRoutes<'a> = Vec<(TelegramMediaType, &'a TelegramChannel)>;

impl Model for TelegramAccount {
    const CLASS_NAME: &'static str = "TelegramAccount";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let name = fields.required("name", after(string, account_name("telegram")));
        let channels = fields.optional(
            "channels",
            Vec::new,
            after(list_of(validate::model::<TelegramChannel>), |channels: Vec<TelegramChannel>| {
                let mut seen: Vec<(i64, TelegramMediaType)> = Vec::new();
                for channel in &channels {
                    for media_type in &channel.media_types {
                        let route = (channel.id, *media_type);
                        if seen.contains(&route) {
                            return Err(format!(
                                "duplicate telegram media route for channel {}: {}",
                                channel.id,
                                media_type.as_str()
                            ));
                        }
                        seen.push(route);
                    }
                }
                Ok(channels)
            }),
        );
        let api_id = fields.optional("api_id", || 0, int);
        let api_hash = fields.optional("api_hash", String::new, string);
        let session_path = fields.optional("session_path", || PyPath::new("./data/telethon-session"), path);
        fields.finish(
            || {
                Some(Self {
                    name: name?,
                    channels: channels?,
                    api_id: api_id?,
                    api_hash: api_hash?,
                    session_path: session_path?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("name", Value::Str(self.name.clone())),
            ("channels", models(&self.channels)),
            ("api_id", Value::from(self.api_id)),
            ("api_hash", Value::Str(self.api_hash.clone())),
            ("session_path", paths(&self.session_path)),
        ])
    }
}

impl TelegramAccount {
    /// Channel id to its media routes, both in the order the channels list them.
    pub fn channel_routes(&self) -> Vec<(i64, MediaRoutes<'_>)> {
        let mut routes: Vec<(i64, MediaRoutes<'_>)> = Vec::new();
        for channel in &self.channels {
            let position = routes.iter().position(|(id, _)| *id == channel.id).unwrap_or_else(|| {
                routes.push((channel.id, Vec::new()));
                routes.len() - 1
            });
            let media_routes = &mut routes[position].1;
            for media_type in &channel.media_types {
                match media_routes.iter_mut().find(|(existing, _)| existing == media_type) {
                    Some(slot) => slot.1 = channel,
                    None => media_routes.push((*media_type, channel)),
                }
            }
        }
        routes
    }

    pub fn validate_runnable(&self) -> Vec<String> {
        let mut missing = Vec::new();
        if self.api_id <= 0 {
            missing.push("api_id".to_owned());
        }
        if py_strip(&self.api_hash).is_empty() {
            missing.push("api_hash".to_owned());
        }
        if self.channels.is_empty() {
            missing.push("channels".to_owned());
        }
        missing
    }
}

/// `web.telegram`.
#[derive(Debug, Clone, PartialEq)]
pub struct Telegram {
    pub schedule: Schedule,
    pub accounts: Vec<TelegramAccount>,
    pub scan_limit: i64,
    pub download_limit_per_channel: i64,
    pub download_delay_seconds: f64,
    pub channel_cooldown_seconds: f64,
    pub history_wait_seconds: f64,
    pub flood_sleep_threshold_seconds: i64,
}

fn telegram_limit(value: i64) -> Result<i64, String> {
    if value < 1 {
        Err("telegram limits must be greater than or equal to 1".to_owned())
    } else {
        Ok(value)
    }
}

fn telegram_delay(value: f64) -> Result<f64, String> {
    if !value.is_finite() || value < 0.0 {
        Err("telegram delays must be finite and greater than or equal to 0".to_owned())
    } else {
        Ok(value)
    }
}

impl Model for Telegram {
    const CLASS_NAME: &'static str = "Telegram";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("*/30 * * * *"));
        let accounts = fields.optional("accounts", Vec::new, list_of(validate::model::<TelegramAccount>));
        let scan_limit = fields.optional("scan_limit", || 50, after(int, telegram_limit));
        let download_limit_per_channel = fields.optional("download_limit_per_channel", || 2, after(int, telegram_limit));
        let download_delay_seconds = fields.optional("download_delay_seconds", || 60.0, after(float, telegram_delay));
        let channel_cooldown_seconds = fields.optional("channel_cooldown_seconds", || 1800.0, after(float, telegram_delay));
        let history_wait_seconds = fields.optional("history_wait_seconds", || 1.0, after(float, telegram_delay));
        let flood_sleep_threshold_seconds = fields.optional(
            "flood_sleep_threshold_seconds",
            || 300,
            after(int, |value| {
                if value < 0 {
                    Err("telegram flood_sleep_threshold_seconds must be greater than or equal to 0".to_owned())
                } else {
                    Ok(value)
                }
            }),
        );
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    accounts: accounts?,
                    scan_limit: scan_limit?,
                    download_limit_per_channel: download_limit_per_channel?,
                    download_delay_seconds: download_delay_seconds?,
                    channel_cooldown_seconds: channel_cooldown_seconds?,
                    history_wait_seconds: history_wait_seconds?,
                    flood_sleep_threshold_seconds: flood_sleep_threshold_seconds?,
                })
            },
            |model| duplicate_account("telegram", model.accounts.iter().map(|account| account.name.clone())),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("accounts", models(&self.accounts)));
        entries.push(("scan_limit", Value::from(self.scan_limit)));
        entries.push(("download_limit_per_channel", Value::from(self.download_limit_per_channel)));
        entries.push(("download_delay_seconds", dump_float(self.download_delay_seconds)));
        entries.push(("channel_cooldown_seconds", dump_float(self.channel_cooldown_seconds)));
        entries.push(("history_wait_seconds", dump_float(self.history_wait_seconds)));
        entries.push(("flood_sleep_threshold_seconds", Value::from(self.flood_sleep_threshold_seconds)));
        dict(entries)
    }
}

impl Telegram {
    pub fn resolved_accounts(&self) -> &[TelegramAccount] {
        &self.accounts
    }

    pub fn validate_runnable(&self) -> Vec<String> {
        if self.accounts.is_empty() {
            return vec!["accounts".to_owned()];
        }
        self.accounts
            .iter()
            .enumerate()
            .flat_map(|(index, account)| {
                account
                    .validate_runnable()
                    .into_iter()
                    .map(move |field| format!("accounts[{index}].{field}"))
            })
            .collect()
    }
}

// ---------- the game sources ----------

/// A section that is only a schedule and a collection path (`Stellasora`, `BD2`).
macro_rules! path_section {
    ($(#[$doc:meta])* $name:ident, $class:literal, $path:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            pub schedule: Schedule,
            pub path: PyPath,
        }

        impl Model for $name {
            const CLASS_NAME: &'static str = $class;

            fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
                let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
                let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
                let path = fields.optional("path", || PyPath::new($path), path);
                fields.finish(|| Some(Self { schedule: schedule?, path: path? }), |_| Ok(()))
            }

            fn dump(&self) -> Value {
                let mut entries = Vec::new();
                self.schedule.dump_into(&mut entries);
                entries.push(("path", paths(&self.path)));
                dict(entries)
            }
        }

        impl $name {
            pub fn validate_runnable(&self) -> Vec<String> {
                Vec::new()
            }
        }
    };
}

path_section!(
    /// `web.stellasora`.
    Stellasora,
    "Stellasora",
    "./collection/stellasora"
);
path_section!(
    /// `web.bd2`.
    Bd2,
    "BD2",
    "./collection/bd2"
);

/// `web.nikke`.
#[derive(Debug, Clone, PartialEq)]
pub struct Nikke {
    pub schedule: Schedule,
    pub path: PyPath,
    pub runtime_capture_enabled: bool,
    pub runtime_capture_timeout_seconds: f64,
    pub runtime_capture_force_refresh: bool,
}

impl Model for Nikke {
    const CLASS_NAME: &'static str = "Nikke";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/nikke"), path);
        let runtime_capture_enabled = fields.optional("runtime_capture_enabled", || false, boolean);
        let runtime_capture_timeout_seconds = fields.optional("runtime_capture_timeout_seconds", || 60.0, float);
        let runtime_capture_force_refresh = fields.optional("runtime_capture_force_refresh", || false, boolean);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    runtime_capture_enabled: runtime_capture_enabled?,
                    runtime_capture_timeout_seconds: runtime_capture_timeout_seconds?,
                    runtime_capture_force_refresh: runtime_capture_force_refresh?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("runtime_capture_enabled", Value::Bool(self.runtime_capture_enabled)));
        entries.push(("runtime_capture_timeout_seconds", dump_float(self.runtime_capture_timeout_seconds)));
        entries.push(("runtime_capture_force_refresh", Value::Bool(self.runtime_capture_force_refresh)));
        dict(entries)
    }
}

impl Nikke {
    pub fn validate_runnable(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `web.azurlane`.
#[derive(Debug, Clone, PartialEq)]
pub struct AzurLane {
    pub schedule: Schedule,
    pub path: PyPath,
    /// Optional route for the ship index and per-ship detail requests; empty is direct.
    pub origin_proxy: String,
    /// Host to connect to while l2d.su does not resolve; TLS name and Host stay l2d.su.
    pub origin_connect_host: String,
    /// Spacing between l2d.su origin requests.
    pub origin_request_interval_seconds: f64,
}

impl Model for AzurLane {
    const CLASS_NAME: &'static str = "AzurLane";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/azurlane"), path);
        let origin_proxy = fields.optional("origin_proxy", String::new, stripped_string);
        let origin_connect_host = fields.optional("origin_connect_host", || "static.l2d.su".to_owned(), stripped_string);
        let origin_request_interval_seconds = fields.optional(
            "origin_request_interval_seconds",
            || 1.0,
            after(float, non_negative_seconds("origin_request_interval_seconds")),
        );
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    origin_proxy: origin_proxy?,
                    origin_connect_host: origin_connect_host?,
                    origin_request_interval_seconds: origin_request_interval_seconds?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("origin_proxy", Value::Str(self.origin_proxy.clone())));
        entries.push(("origin_connect_host", Value::Str(self.origin_connect_host.clone())));
        entries.push(("origin_request_interval_seconds", dump_float(self.origin_request_interval_seconds)));
        dict(entries)
    }
}

impl AzurLane {
    pub fn validate_runnable(&self) -> Vec<String> {
        Vec::new()
    }
}

// ---------- Hanime1 ----------

/// `Hanime1RankingDeepScan`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hanime1RankingDeepScan {
    pub enabled: bool,
    pub quota: f64,
    pub max_extra_pages: i64,
}

impl Model for Hanime1RankingDeepScan {
    const CLASS_NAME: &'static str = "Hanime1RankingDeepScan";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let enabled = fields.optional("enabled", || false, boolean);
        let quota = fields.optional(
            "quota",
            || 0.25,
            after(float, |value| {
                if !value.is_finite() || value <= 0.0 {
                    Err("quota must be finite and greater than 0".to_owned())
                } else {
                    Ok(value)
                }
            }),
        );
        let max_extra_pages = fields.optional(
            "max_extra_pages",
            || 5,
            after(int, |value| {
                if value < 1 {
                    Err("max_extra_pages must be greater than or equal to 1".to_owned())
                } else {
                    Ok(value)
                }
            }),
        );
        fields.finish(
            || {
                Some(Self {
                    enabled: enabled?,
                    quota: quota?,
                    max_extra_pages: max_extra_pages?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("enabled", Value::Bool(self.enabled)),
            ("quota", dump_float(self.quota)),
            ("max_extra_pages", Value::from(self.max_extra_pages)),
        ])
    }
}

impl Default for Hanime1RankingDeepScan {
    fn default() -> Self {
        Self {
            enabled: false,
            quota: 0.25,
            max_extra_pages: 5,
        }
    }
}

/// A Hanime1 ranking period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankingPeriod {
    Weekly,
    Monthly,
}

impl LiteralStr for RankingPeriod {
    const VARIANTS: &'static [(Self, &'static str)] = &[(Self::Weekly, "weekly"), (Self::Monthly, "monthly")];

    fn as_str(self) -> &'static str {
        match self {
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

/// `Hanime1Ranking`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hanime1Ranking {
    pub enabled: bool,
    pub periods: Vec<RankingPeriod>,
    pub pages: i64,
    pub deep_scan: Hanime1RankingDeepScan,
}

impl Default for Hanime1Ranking {
    fn default() -> Self {
        Self {
            enabled: false,
            periods: vec![RankingPeriod::Weekly, RankingPeriod::Monthly],
            pages: 1,
            deep_scan: Hanime1RankingDeepScan::default(),
        }
    }
}

impl Model for Hanime1Ranking {
    const CLASS_NAME: &'static str = "Hanime1Ranking";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let enabled = fields.optional("enabled", || false, boolean);
        let periods = fields.optional(
            "periods",
            || vec![RankingPeriod::Weekly, RankingPeriod::Monthly],
            after(list_of(literal::<RankingPeriod>), |value: Vec<RankingPeriod>| {
                if value.is_empty() {
                    Err("periods cannot be empty".to_owned())
                } else {
                    Ok(dedupe(value))
                }
            }),
        );
        let pages = fields.optional(
            "pages",
            || 1,
            after(int, |value| {
                if value < 1 {
                    Err("pages must be greater than or equal to 1".to_owned())
                } else {
                    Ok(value)
                }
            }),
        );
        let deep_scan = fields.optional("deep_scan", Hanime1RankingDeepScan::default, validate::model);
        fields.finish(
            || {
                Some(Self {
                    enabled: enabled?,
                    periods: periods?,
                    pages: pages?,
                    deep_scan: deep_scan?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("enabled", Value::Bool(self.enabled)),
            (
                "periods",
                Value::List(self.periods.iter().map(|period| Value::from(period.as_str())).collect()),
            ),
            ("pages", Value::from(self.pages)),
            ("deep_scan", self.deep_scan.dump()),
        ])
    }
}

/// Which script Hanime1 pages are requested in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hanime1Lang {
    Zhs,
    Zht,
}

impl LiteralStr for Hanime1Lang {
    const VARIANTS: &'static [(Self, &'static str)] = &[(Self::Zhs, "zhs"), (Self::Zht, "zht")];

    fn as_str(self) -> &'static str {
        match self {
            Self::Zhs => "zhs",
            Self::Zht => "zht",
        }
    }
}

/// `web.hanime1`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hanime1 {
    pub schedule: Schedule,
    pub path: PyPath,
    pub host: String,
    pub user_lang: Hanime1Lang,
    pub ranking: Hanime1Ranking,
}

impl Model for Hanime1 {
    const CLASS_NAME: &'static str = "Hanime1";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/hanime1"), path);
        let host = fields.optional("host", || "https://hanime1.me".to_owned(), string);
        let user_lang = fields.optional("user_lang", || Hanime1Lang::Zhs, literal);
        let ranking = fields.optional("ranking", Hanime1Ranking::default, validate::model);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    host: host?,
                    user_lang: user_lang?,
                    ranking: ranking?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("host", Value::Str(self.host.clone())));
        entries.push(("user_lang", Value::from(self.user_lang.as_str())));
        entries.push(("ranking", self.ranking.dump()));
        dict(entries)
    }
}

impl Hanime1 {
    pub fn validate_runnable(&self) -> Vec<String> {
        Vec::new()
    }
}

// ---------- Jandan, Kemono ----------

/// `web.jandan`.
#[derive(Debug, Clone, PartialEq)]
pub struct Jandan {
    pub schedule: Schedule,
    pub path: PyPath,
    pub api_url: String,
    pub user_id: i64,
    pub fav_types: Vec<i64>,
    pub fav_num_limit: i64,
}

const JANDAN_FAV_TYPES: [i64; 3] = [1, 2, 6];

impl Model for Jandan {
    const CLASS_NAME: &'static str = "Jandan";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/jandan"), path);
        let api_url = fields.optional("api_url", || "https://joiningss.com/jd/api".to_owned(), string);
        let user_id = fields.optional("user_id", || 0, int);
        let fav_types = fields.optional(
            "fav_types",
            || JANDAN_FAV_TYPES.to_vec(),
            after(list_of(int), |value: Vec<i64>| {
                if value.is_empty() {
                    return Err("fav_types cannot be empty".to_owned());
                }
                if value.iter().any(|fav_type| !JANDAN_FAV_TYPES.contains(fav_type)) {
                    return Err("fav_types must be in [1, 2, 6]".to_owned());
                }
                Ok(dedupe(value))
            }),
        );
        let fav_num_limit = fields.optional("fav_num_limit", || 45, int);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    api_url: api_url?,
                    user_id: user_id?,
                    fav_types: fav_types?,
                    fav_num_limit: fav_num_limit?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("api_url", Value::Str(self.api_url.clone())));
        entries.push(("user_id", Value::from(self.user_id)));
        entries.push((
            "fav_types",
            Value::List(self.fav_types.iter().map(|fav_type| Value::from(*fav_type)).collect()),
        ));
        entries.push(("fav_num_limit", Value::from(self.fav_num_limit)));
        dict(entries)
    }
}

impl Jandan {
    pub fn validate_runnable(&self) -> Vec<String> {
        if self.user_id > 0 { Vec::new() } else { vec!["user_id".to_owned()] }
    }
}

/// One Kemono creator to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KemonoCreator {
    pub service: String,
    pub id: String,
    pub name: String,
}

impl Model for KemonoCreator {
    const CLASS_NAME: &'static str = "KemonoCreator";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let service = fields.required("service", string);
        let id = fields.required("id", string);
        let name = fields.required("name", string);
        fields.finish(
            || {
                Some(Self {
                    service: service?,
                    id: id?,
                    name: name?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("service", Value::Str(self.service.clone())),
            ("id", Value::Str(self.id.clone())),
            ("name", Value::Str(self.name.clone())),
        ])
    }
}

/// `web.kemono`.
#[derive(Debug, Clone, PartialEq)]
pub struct Kemono {
    pub schedule: Schedule,
    pub path: PyPath,
    /// The site moves domains; the next move is a settings edit, not a deploy.
    pub base_url: String,
    /// Attachments live on a separate host.
    pub file_base_url: String,
    /// Compressed previews, used for `preview_only` attachments.
    pub thumbnail_base_url: String,
    /// Applied before every API and file request.
    pub sleep_request_seconds: f64,
    pub creators: Vec<KemonoCreator>,
}

fn base_url(value: &str) -> Result<String, String> {
    let normalized = py_rstrip_chars(py_strip(value), &['/']);
    if normalized.starts_with("http://") || normalized.starts_with("https://") {
        Ok(normalized.to_owned())
    } else {
        Err("base URLs must start with http:// or https://".to_owned())
    }
}

impl Model for Kemono {
    const CLASS_NAME: &'static str = "Kemono";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/kemono"), path);
        let base_url_field = fields.optional(
            "base_url",
            || "https://pawchive.pw".to_owned(),
            after(string, |value: String| base_url(&value)),
        );
        let file_base_url = fields.optional(
            "file_base_url",
            || "https://file.pawchive.pw".to_owned(),
            after(string, |value: String| base_url(&value)),
        );
        let thumbnail_base_url = fields.optional(
            "thumbnail_base_url",
            || "https://img.pawchive.pw".to_owned(),
            after(string, |value: String| base_url(&value)),
        );
        let sleep_request_seconds = fields.optional(
            "sleep_request_seconds",
            || 1.0,
            after(float, non_negative_seconds("sleep_request_seconds")),
        );
        let creators = fields.optional("creators", Vec::new, list_of(validate::model::<KemonoCreator>));
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    base_url: base_url_field?,
                    file_base_url: file_base_url?,
                    thumbnail_base_url: thumbnail_base_url?,
                    sleep_request_seconds: sleep_request_seconds?,
                    creators: creators?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("base_url", Value::Str(self.base_url.clone())));
        entries.push(("file_base_url", Value::Str(self.file_base_url.clone())));
        entries.push(("thumbnail_base_url", Value::Str(self.thumbnail_base_url.clone())));
        entries.push(("sleep_request_seconds", dump_float(self.sleep_request_seconds)));
        entries.push(("creators", models(&self.creators)));
        dict(entries)
    }
}

impl Kemono {
    pub fn validate_runnable(&self) -> Vec<String> {
        if self.creators.is_empty() {
            vec!["creators".to_owned()]
        } else {
            Vec::new()
        }
    }
}

// ---------- Twitter, Pixiv, RedNote, WeChat ----------

/// `video_path`: the form sends `''` for "keep them with the images", which must not become
/// `Path('')`, i.e. the working directory (a `mode='before'` validator in Python).
fn video_path(value: &Value, loc: &[LocItem]) -> Result<Option<PyPath>, Errors> {
    if let Value::Str(text) = value
        && py_strip(text).is_empty()
    {
        return Ok(None);
    }
    nullable(path)(value, loc)
}

fn positive_count(field: &'static str) -> impl FnOnce(i64) -> Result<i64, String> {
    move |value| {
        if value < 1 {
            Err(format!("{field} must be greater than or equal to 1"))
        } else {
            Ok(value)
        }
    }
}

/// `web.twitter`: liked tweets on X, crawled through gallery-dl.
#[derive(Debug, Clone, PartialEq)]
pub struct Twitter {
    pub schedule: Schedule,
    pub path: PyPath,
    /// Where videos and GIFs land; unset keeps them with the images.
    pub video_path: Option<PyPath>,
    /// Own screen name, without the leading @.
    pub username: String,
    pub sleep_request_seconds: f64,
    /// Consecutive already-archived files that end an incremental run.
    pub abort_after: i64,
    pub proxy: String,
    pub include_retweets: bool,
    pub include_videos: bool,
}

impl Model for Twitter {
    const CLASS_NAME: &'static str = "Twitter";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/twitter"), path);
        let video = fields.optional("video_path", || None, video_path);
        let username = fields.optional(
            "username",
            String::new,
            after(string, |value| {
                let normalized = py_lstrip_chars(py_strip(&value), &['@']);
                if !normalized.is_empty() && !is_twitter_username(normalized) {
                    return Err("username must contain only ASCII letters, digits, or underscores".to_owned());
                }
                Ok(normalized.to_owned())
            }),
        );
        let sleep_request_seconds = fields.optional(
            "sleep_request_seconds",
            || 2.0,
            after(float, non_negative_seconds("sleep_request_seconds")),
        );
        let abort_after = fields.optional("abort_after", || 20, after(int, positive_count("abort_after")));
        let proxy = fields.optional("proxy", String::new, stripped_string);
        let include_retweets = fields.optional("include_retweets", || true, boolean);
        let include_videos = fields.optional("include_videos", || true, boolean);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    video_path: video?,
                    username: username?,
                    sleep_request_seconds: sleep_request_seconds?,
                    abort_after: abort_after?,
                    proxy: proxy?,
                    include_retweets: include_retweets?,
                    include_videos: include_videos?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("video_path", self.video_path.as_ref().map_or(Value::Null, paths)));
        entries.push(("username", Value::Str(self.username.clone())));
        entries.push(("sleep_request_seconds", dump_float(self.sleep_request_seconds)));
        entries.push(("abort_after", Value::from(self.abort_after)));
        entries.push(("proxy", Value::Str(self.proxy.clone())));
        entries.push(("include_retweets", Value::Bool(self.include_retweets)));
        entries.push(("include_videos", Value::Bool(self.include_videos)));
        dict(entries)
    }
}

impl Twitter {
    /// The captured login lives in the database, so only the username is checked here.
    pub fn validate_runnable(&self) -> Vec<String> {
        if self.username.is_empty() {
            vec!["username".to_owned()]
        } else {
            Vec::new()
        }
    }
}

/// `web.pixiv`: public bookmarks through the www.pixiv.net ajax API.
#[derive(Debug, Clone, PartialEq)]
pub struct Pixiv {
    pub schedule: Schedule,
    pub path: PyPath,
    /// Optional override; empty means "derive from the PHPSESSID cookie".
    pub user_id: String,
    pub sleep_request_seconds: f64,
    pub proxy: String,
}

impl Model for Pixiv {
    const CLASS_NAME: &'static str = "Pixiv";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/pixiv"), path);
        let user_id = fields.optional(
            "user_id",
            String::new,
            after(string, |value| {
                let normalized = py_strip(&value);
                // Python's `isdigit()`: any Unicode digit, not only ASCII.
                if !normalized.is_empty() && !py_isdigit(normalized) {
                    return Err("user_id must contain only digits".to_owned());
                }
                Ok(normalized.to_owned())
            }),
        );
        let sleep_request_seconds = fields.optional(
            "sleep_request_seconds",
            || 1.0,
            after(float, non_negative_seconds("sleep_request_seconds")),
        );
        let proxy = fields.optional("proxy", String::new, stripped_string);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    user_id: user_id?,
                    sleep_request_seconds: sleep_request_seconds?,
                    proxy: proxy?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("user_id", Value::Str(self.user_id.clone())));
        entries.push(("sleep_request_seconds", dump_float(self.sleep_request_seconds)));
        entries.push(("proxy", Value::Str(self.proxy.clone())));
        dict(entries)
    }
}

impl Pixiv {
    pub fn validate_runnable(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `web.rednote`: liked notes, read through a signed-in Chromium profile.
#[derive(Debug, Clone, PartialEq)]
pub struct RedNote {
    pub schedule: Schedule,
    pub path: PyPath,
    pub video_path: Option<PyPath>,
    /// The signed-in Chromium profile; has to be on a volume.
    pub profile_path: PyPath,
    /// Egress for xiaohongshu.com (most datacenter ranges answer HTTP 461).
    pub proxy: String,
    /// Deliberate opt-out for a deployment whose own egress is residential.
    pub allow_direct_connection: bool,
    pub proxy_media: bool,
    pub user_id: String,
    pub sleep_request_seconds: f64,
    pub abort_after: i64,
    pub max_pages_per_run: i64,
    pub headless: bool,
}

impl Model for RedNote {
    const CLASS_NAME: &'static str = "RedNote";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("0 */6 * * *"));
        let root = fields.optional("path", || PyPath::new("./collection/rednote"), path);
        let video = fields.optional("video_path", || None, video_path);
        let profile_path = fields.optional("profile_path", || PyPath::new("./data/rednote-profile"), path);
        let proxy = fields.optional("proxy", String::new, stripped_string);
        let allow_direct_connection = fields.optional("allow_direct_connection", || false, boolean);
        let proxy_media = fields.optional("proxy_media", || false, boolean);
        let user_id = fields.optional("user_id", String::new, stripped_string);
        let sleep_request_seconds = fields.optional(
            "sleep_request_seconds",
            || 3.0,
            after(float, non_negative_seconds("sleep_request_seconds")),
        );
        let abort_after = fields.optional("abort_after", || 2, after(int, positive_count("abort_after")));
        let max_pages_per_run = fields.optional("max_pages_per_run", || 40, after(int, positive_count("max_pages_per_run")));
        let headless = fields.optional("headless", || true, boolean);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: root?,
                    video_path: video?,
                    profile_path: profile_path?,
                    proxy: proxy?,
                    allow_direct_connection: allow_direct_connection?,
                    proxy_media: proxy_media?,
                    user_id: user_id?,
                    sleep_request_seconds: sleep_request_seconds?,
                    abort_after: abort_after?,
                    max_pages_per_run: max_pages_per_run?,
                    headless: headless?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("video_path", self.video_path.as_ref().map_or(Value::Null, paths)));
        entries.push(("profile_path", paths(&self.profile_path)));
        entries.push(("proxy", Value::Str(self.proxy.clone())));
        entries.push(("allow_direct_connection", Value::Bool(self.allow_direct_connection)));
        entries.push(("proxy_media", Value::Bool(self.proxy_media)));
        entries.push(("user_id", Value::Str(self.user_id.clone())));
        entries.push(("sleep_request_seconds", dump_float(self.sleep_request_seconds)));
        entries.push(("abort_after", Value::from(self.abort_after)));
        entries.push(("max_pages_per_run", Value::from(self.max_pages_per_run)));
        entries.push(("headless", Value::Bool(self.headless)));
        dict(entries)
    }
}

impl RedNote {
    /// Going out from a datacenter address has to be asked for: it once cost the account its sessions.
    pub fn validate_runnable(&self) -> Vec<String> {
        if !self.proxy.is_empty() || self.allow_direct_connection {
            Vec::new()
        } else {
            vec!["proxy".to_owned()]
        }
    }
}

/// `str` with surrounding whitespace and trailing slashes removed.
fn url_without_trailing_slash(value: &Value, loc: &[LocItem]) -> Result<String, Errors> {
    string(value, loc).map(|text| py_rstrip_chars(py_strip(&text), &['/']).to_owned())
}

/// `web.wechat`: what is forwarded to the agent's WeChat account (`FavInbox` plugin).
#[derive(Clone, PartialEq)]
pub struct Wechat {
    pub schedule: Schedule,
    pub path: PyPath,
    /// The `FavInbox` service, as the cluster reaches it.
    pub api_url: String,
    /// `FAVINBOX_TOKEN` of the host running the service.
    pub token: String,
}

impl Model for Wechat {
    const CLASS_NAME: &'static str = "Wechat";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let schedule = Schedule::read(&mut fields, Some("*/15 * * * *"));
        let path = fields.optional("path", || PyPath::new("./collection/wechat"), path);
        let api_url = fields.optional(
            "api_url",
            || "http://favinbox-oracle.proxy.svc".to_owned(),
            url_without_trailing_slash,
        );
        let token = fields.optional("token", String::new, stripped_string);
        fields.finish(
            || {
                Some(Self {
                    schedule: schedule?,
                    path: path?,
                    api_url: api_url?,
                    token: token?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        let mut entries = Vec::new();
        self.schedule.dump_into(&mut entries);
        entries.push(("path", paths(&self.path)));
        entries.push(("api_url", Value::Str(self.api_url.clone())));
        entries.push(("token", Value::Str(self.token.clone())));
        dict(entries)
    }
}

impl Wechat {
    pub fn validate_runnable(&self) -> Vec<String> {
        [("api_url", &self.api_url), ("token", &self.token)]
            .into_iter()
            .filter(|(_, value)| value.is_empty())
            .map(|(name, _)| name.to_owned())
            .collect()
    }
}

// ---------- notifications and services ----------

/// `notifications.telegram`: the bot that delivers notifications.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct TelegramNotification {
    pub enabled: bool,
    pub bot_token: String,
    pub chat_id: String,
    pub message_thread_id: Option<i64>,
}

impl Model for TelegramNotification {
    const CLASS_NAME: &'static str = "TelegramNotification";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let enabled = fields.optional("enabled", || false, boolean);
        let bot_token = fields.optional("bot_token", String::new, stripped_string);
        let chat_id = fields.optional("chat_id", String::new, stripped_string);
        let message_thread_id = fields.optional(
            "message_thread_id",
            || None,
            after(nullable(int), |value| match value {
                Some(id) if id <= 0 => Err("message_thread_id must be a positive integer".to_owned()),
                other => Ok(other),
            }),
        );
        fields.finish(
            || {
                Some(Self {
                    enabled: enabled?,
                    bot_token: bot_token?,
                    chat_id: chat_id?,
                    message_thread_id: message_thread_id?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("enabled", Value::Bool(self.enabled)),
            ("bot_token", Value::Str(self.bot_token.clone())),
            ("chat_id", Value::Str(self.chat_id.clone())),
            ("message_thread_id", self.message_thread_id.map_or(Value::Null, Value::from)),
        ])
    }
}

impl TelegramNotification {
    pub fn configured(&self) -> bool {
        !self.bot_token.is_empty() && !self.chat_id.is_empty()
    }

    pub fn validate_runnable(&self) -> Vec<String> {
        [("bot_token", &self.bot_token), ("chat_id", &self.chat_id)]
            .into_iter()
            .filter(|(_, value)| value.is_empty())
            .map(|(name, _)| name.to_owned())
            .collect()
    }
}

/// `services.tikhub`: a paid API that reads sites which refuse the cluster.
#[derive(Clone, PartialEq, Eq)]
pub struct TikHub {
    /// `api.tikhub.dev` serves mainland China.
    pub api_url: String,
    pub token: String,
}

impl Default for TikHub {
    fn default() -> Self {
        Self {
            api_url: "https://api.tikhub.io".to_owned(),
            token: String::new(),
        }
    }
}

impl Model for TikHub {
    const CLASS_NAME: &'static str = "TikHub";

    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
        let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
        let api_url = fields.optional("api_url", || "https://api.tikhub.io".to_owned(), url_without_trailing_slash);
        let token = fields.optional("token", String::new, stripped_string);
        fields.finish(
            || {
                Some(Self {
                    api_url: api_url?,
                    token: token?,
                })
            },
            |_| Ok(()),
        )
    }

    fn dump(&self) -> Value {
        dict(vec![
            ("api_url", Value::Str(self.api_url.clone())),
            ("token", Value::Str(self.token.clone())),
        ])
    }
}

impl TikHub {
    pub fn configured(&self) -> bool {
        !self.token.is_empty()
    }
}

// ---------- the tree ----------

/// A container model whose fields are all models with defaults.
macro_rules! container {
    ($(#[$doc:meta])* $name:ident, $class:literal, { $($field:ident : $ty:ty),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            $(pub $field: $ty,)+
        }

        impl Default for $name {
            fn default() -> Self {
                Self { $($field: <$ty>::defaults(),)+ }
            }
        }

        impl Model for $name {
            const CLASS_NAME: &'static str = $class;

            fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors> {
                let mut fields = Fields::begin(value, loc, Self::CLASS_NAME)?;
                $(let $field = fields.optional(stringify!($field), <$ty>::defaults, validate::model::<$ty>);)+
                fields.finish(|| Some(Self { $($field: $field?,)+ }), |_| Ok(()))
            }

            fn dump(&self) -> Value {
                dict(vec![$((stringify!($field), self.$field.dump()),)+])
            }
        }
    };
}

/// A model that can be built from its defaults, like `Model()` in Python.
pub trait Defaults {
    fn defaults() -> Self;
}

macro_rules! defaults_from_empty {
    ($($ty:ty),+ $(,)?) => {
        $(impl Defaults for $ty {
            fn defaults() -> Self {
                // Every field has a default, so an empty input always validates.
                match <$ty as Model>::validate(&Value::Dict(Dict::new())) {
                    Ok(model) => model,
                    Err(error) => unreachable!("{} has a field without a default: {error}", <$ty as Model>::CLASS_NAME),
                }
            }
        })+
    };
}

defaults_from_empty!(
    Bilibili,
    Telegram,
    Stellasora,
    Nikke,
    Bd2,
    AzurLane,
    Hanime1,
    Jandan,
    Kemono,
    Twitter,
    Pixiv,
    RedNote,
    Wechat,
    TelegramNotification,
    TikHub,
    Web,
    Notifications,
    Services,
    Settings,
);

container!(
    /// `Web`: one section per source.
    Web,
    "Web",
    {
        bilibili: Bilibili,
        telegram: Telegram,
        stellasora: Stellasora,
        nikke: Nikke,
        bd2: Bd2,
        azurlane: AzurLane,
        hanime1: Hanime1,
        jandan: Jandan,
        kemono: Kemono,
        twitter: Twitter,
        pixiv: Pixiv,
        rednote: RedNote,
        wechat: Wechat,
    }
);

container!(
    /// `Notifications`.
    Notifications,
    "Notifications",
    { telegram: TelegramNotification }
);

container!(
    /// `Services`: third-party services that sources share rather than own.
    Services,
    "Services",
    { tikhub: TikHub }
);

container!(
    /// The whole settings tree.
    Settings,
    "Settings",
    { web: Web, notifications: Notifications, services: Services }
);

// ---------- sections ----------

/// The section names, in the order the UI renders them (`SECTION_MODELS`).
pub const SECTIONS: [&str; 15] = [
    "web.bilibili",
    "web.telegram",
    "web.stellasora",
    "web.nikke",
    "web.bd2",
    "web.azurlane",
    "web.hanime1",
    "web.jandan",
    "web.kemono",
    "web.twitter",
    "web.pixiv",
    "web.rednote",
    "web.wechat",
    "notifications.telegram",
    "services.tikhub",
];

/// `Debug` for a model holding a secret: the `SENSITIVE_FIELDS` print as `<redacted>` (or
/// `""` when empty, so an unset secret is still visible), so a stray `{:?}` cannot leak one.
macro_rules! redacted_debug {
    ($ty:ident { $($field:ident),* } secret { $($secret:ident),* }) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), &self.$field))*
                    $(.field(stringify!($secret), &redacted(&self.$secret)))*
                    .finish()
            }
        }
    };
}

fn redacted(secret: &str) -> &'static str {
    if secret.is_empty() { "" } else { "<redacted>" }
}

redacted_debug!(TelegramAccount { name, channels, api_id, session_path } secret { api_hash });
redacted_debug!(Wechat { schedule, path, api_url } secret { token });
redacted_debug!(TelegramNotification { enabled, chat_id, message_thread_id } secret { bot_token });
redacted_debug!(TikHub { api_url } secret { token });

/// Written by the UI, never echoed back in full (`SENSITIVE_FIELDS`).
pub fn sensitive_fields(section: &str) -> &'static [&'static str] {
    match section {
        "web.telegram" => &["accounts[].api_hash"],
        "notifications.telegram" => &["bot_token"],
        "web.wechat" | "services.tikhub" => &["token"],
        _ => &[],
    }
}

/// `UnknownSectionError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Unknown settings section: {section}")]
pub struct UnknownSectionError {
    pub section: String,
}

/// Why a section payload was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SectionError {
    #[error(transparent)]
    Unknown(#[from] UnknownSectionError),
    #[error(transparent)]
    Invalid(#[from] ValidationError),
}

/// One validated section, whichever it is.
#[derive(Debug, Clone, PartialEq)]
pub enum SectionValue {
    Bilibili(Bilibili),
    Telegram(Telegram),
    Stellasora(Stellasora),
    Nikke(Nikke),
    Bd2(Bd2),
    AzurLane(AzurLane),
    Hanime1(Hanime1),
    Jandan(Jandan),
    Kemono(Kemono),
    Twitter(Twitter),
    Pixiv(Pixiv),
    RedNote(RedNote),
    Wechat(Wechat),
    TelegramNotification(TelegramNotification),
    TikHub(TikHub),
}

macro_rules! each_section {
    ($value:expr, $model:ident => $body:expr) => {
        match $value {
            SectionValue::Bilibili($model) => $body,
            SectionValue::Telegram($model) => $body,
            SectionValue::Stellasora($model) => $body,
            SectionValue::Nikke($model) => $body,
            SectionValue::Bd2($model) => $body,
            SectionValue::AzurLane($model) => $body,
            SectionValue::Hanime1($model) => $body,
            SectionValue::Jandan($model) => $body,
            SectionValue::Kemono($model) => $body,
            SectionValue::Twitter($model) => $body,
            SectionValue::Pixiv($model) => $body,
            SectionValue::RedNote($model) => $body,
            SectionValue::Wechat($model) => $body,
            SectionValue::TelegramNotification($model) => $body,
            SectionValue::TikHub($model) => $body,
        }
    };
}

impl SectionValue {
    /// `SECTION_MODELS[section].model_validate(value)`.
    pub fn validate(section: &str, value: &Value) -> Result<Self, SectionError> {
        Ok(match section {
            "web.bilibili" => Self::Bilibili(Bilibili::validate(value)?),
            "web.telegram" => Self::Telegram(Telegram::validate(value)?),
            "web.stellasora" => Self::Stellasora(Stellasora::validate(value)?),
            "web.nikke" => Self::Nikke(Nikke::validate(value)?),
            "web.bd2" => Self::Bd2(Bd2::validate(value)?),
            "web.azurlane" => Self::AzurLane(AzurLane::validate(value)?),
            "web.hanime1" => Self::Hanime1(Hanime1::validate(value)?),
            "web.jandan" => Self::Jandan(Jandan::validate(value)?),
            "web.kemono" => Self::Kemono(Kemono::validate(value)?),
            "web.twitter" => Self::Twitter(Twitter::validate(value)?),
            "web.pixiv" => Self::Pixiv(Pixiv::validate(value)?),
            "web.rednote" => Self::RedNote(RedNote::validate(value)?),
            "web.wechat" => Self::Wechat(Wechat::validate(value)?),
            "notifications.telegram" => Self::TelegramNotification(TelegramNotification::validate(value)?),
            "services.tikhub" => Self::TikHub(TikHub::validate(value)?),
            other => return Err(UnknownSectionError { section: other.to_owned() }.into()),
        })
    }

    /// The Python class name (`SECTION_MODELS[section].__name__`).
    pub fn class_name(&self) -> &'static str {
        fn name<M: Model>(_: &M) -> &'static str {
            M::CLASS_NAME
        }
        each_section!(self, model => name(model))
    }

    pub fn dump(&self) -> Value {
        each_section!(self, model => model.dump())
    }

    /// The field names that must be filled in before this section's job can run.
    pub fn validate_runnable(&self) -> Vec<String> {
        match self {
            Self::TikHub(_) => Vec::new(),
            other => each_section!(other, model => runnable(model)),
        }
    }

    /// `getattr(model, 'enabled', False)`.
    pub fn enabled(&self) -> bool {
        match self {
            Self::TikHub(_) => false,
            Self::TelegramNotification(model) => model.enabled,
            other => other.schedule().is_some_and(|schedule| schedule.enabled),
        }
    }

    /// The schedule of a source section; `None` for notifications and services.
    pub fn schedule(&self) -> Option<&Schedule> {
        Some(match self {
            Self::Bilibili(model) => &model.schedule,
            Self::Telegram(model) => &model.schedule,
            Self::Stellasora(model) => &model.schedule,
            Self::Nikke(model) => &model.schedule,
            Self::Bd2(model) => &model.schedule,
            Self::AzurLane(model) => &model.schedule,
            Self::Hanime1(model) => &model.schedule,
            Self::Jandan(model) => &model.schedule,
            Self::Kemono(model) => &model.schedule,
            Self::Twitter(model) => &model.schedule,
            Self::Pixiv(model) => &model.schedule,
            Self::RedNote(model) => &model.schedule,
            Self::Wechat(model) => &model.schedule,
            Self::TelegramNotification(_) | Self::TikHub(_) => return None,
        })
    }
}

/// Sections with a `validate_runnable`.
trait Runnable {
    fn runnable(&self) -> Vec<String>;
}

macro_rules! runnable_impl {
    ($($ty:ty),+ $(,)?) => {
        $(impl Runnable for $ty {
            fn runnable(&self) -> Vec<String> {
                self.validate_runnable()
            }
        })+
    };
}

runnable_impl!(
    Bilibili,
    Telegram,
    Stellasora,
    Nikke,
    Bd2,
    AzurLane,
    Hanime1,
    Jandan,
    Kemono,
    Twitter,
    Pixiv,
    RedNote,
    Wechat,
    TelegramNotification
);

impl Runnable for TikHub {
    fn runnable(&self) -> Vec<String> {
        Vec::new()
    }
}

fn runnable(model: &impl Runnable) -> Vec<String> {
    model.runnable()
}

impl Settings {
    /// The section at a dotted name, as stored.
    pub fn section(&self, section: &str) -> Result<SectionValue, UnknownSectionError> {
        let web = &self.web;
        Ok(match section {
            "web.bilibili" => SectionValue::Bilibili(web.bilibili.clone()),
            "web.telegram" => SectionValue::Telegram(web.telegram.clone()),
            "web.stellasora" => SectionValue::Stellasora(web.stellasora.clone()),
            "web.nikke" => SectionValue::Nikke(web.nikke.clone()),
            "web.bd2" => SectionValue::Bd2(web.bd2.clone()),
            "web.azurlane" => SectionValue::AzurLane(web.azurlane.clone()),
            "web.hanime1" => SectionValue::Hanime1(web.hanime1.clone()),
            "web.jandan" => SectionValue::Jandan(web.jandan.clone()),
            "web.kemono" => SectionValue::Kemono(web.kemono.clone()),
            "web.twitter" => SectionValue::Twitter(web.twitter.clone()),
            "web.pixiv" => SectionValue::Pixiv(web.pixiv.clone()),
            "web.rednote" => SectionValue::RedNote(web.rednote.clone()),
            "web.wechat" => SectionValue::Wechat(web.wechat.clone()),
            "notifications.telegram" => SectionValue::TelegramNotification(self.notifications.telegram.clone()),
            "services.tikhub" => SectionValue::TikHub(self.services.tikhub.clone()),
            other => return Err(UnknownSectionError { section: other.to_owned() }),
        })
    }

    /// The schedule of a source by its job key (its field name on `Web`).
    pub fn job_schedule(&self, key: &str) -> Option<&Schedule> {
        let web = &self.web;
        Some(match key {
            "bilibili" => &web.bilibili.schedule,
            "telegram" => &web.telegram.schedule,
            "stellasora" => &web.stellasora.schedule,
            "nikke" => &web.nikke.schedule,
            "bd2" => &web.bd2.schedule,
            "azurlane" => &web.azurlane.schedule,
            "hanime1" => &web.hanime1.schedule,
            "jandan" => &web.jandan.schedule,
            "kemono" => &web.kemono.schedule,
            "twitter" => &web.twitter.schedule,
            "pixiv" => &web.pixiv.schedule,
            "rednote" => &web.rednote.schedule,
            "wechat" => &web.wechat.schedule,
            _ => return None,
        })
    }

    /// Mutable access to a source's schedule, for tests and callers that pin a snapshot.
    pub fn job_schedule_mut(&mut self, key: &str) -> Option<&mut Schedule> {
        let web = &mut self.web;
        Some(match key {
            "bilibili" => &mut web.bilibili.schedule,
            "telegram" => &mut web.telegram.schedule,
            "stellasora" => &mut web.stellasora.schedule,
            "nikke" => &mut web.nikke.schedule,
            "bd2" => &mut web.bd2.schedule,
            "azurlane" => &mut web.azurlane.schedule,
            "hanime1" => &mut web.hanime1.schedule,
            "jandan" => &mut web.jandan.schedule,
            "kemono" => &mut web.kemono.schedule,
            "twitter" => &mut web.twitter.schedule,
            "pixiv" => &mut web.pixiv.schedule,
            "rednote" => &mut web.rednote.schedule,
            "wechat" => &mut web.wechat.schedule,
            _ => return None,
        })
    }
}

/// `build_settings(rows)`: place each known section at its dotted path and validate the
/// tree. Unknown sections (written by a newer version, or retired) are ignored.
pub fn build_settings<'a>(rows: impl IntoIterator<Item = (&'a str, &'a Value)>) -> Result<Settings, ValidationError> {
    let mut payload = Dict::new();
    for (section, value) in rows {
        if !SECTIONS.contains(&section) {
            continue;
        }
        let (parent, field) = section.split_once('.').unwrap_or(("", section));
        let mut container = match payload.remove(parent) {
            Some(Value::Dict(existing)) => existing,
            _ => Dict::new(),
        };
        container.insert(field, value.clone());
        payload.insert(parent, Value::Dict(container));
    }
    Settings::validate(&Value::Dict(payload))
}

/// `notify_enabled(source)` against a loaded snapshot: whether `source`'s notifications
/// should be queued. Anything that is not a source's field on `Web` (such as `worker`, the
/// job-failure source) has no toggle and is left on.
pub fn notify_enabled(settings: &Settings, source: &str) -> bool {
    let key = py_strip(source).to_lowercase();
    settings.job_schedule(&key).is_none_or(|schedule| schedule.notify)
}

#[cfg(test)]
mod tests;
