//! The source registry and the jobs built from the settings.
//!
//! Python origin: `src/service/jobs.py`. `JOB_SPECS` is the authoritative list of
//! sources; [`build_jobs`] turns a settings snapshot into what the scheduler and the
//! trigger paths work from. The factories live in [`crate::source::Registry`].

use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;
use fav_core::settings::Settings;

use crate::pystr::repr_str;

/// One entry of `JOB_SPECS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobSpec {
    pub key: &'static str,
    pub name: &'static str,
    /// The field on `Web` holding the source's section.
    pub attr: &'static str,
    /// External programs the job shells out to.
    pub required_commands: &'static [&'static str],
}

/// `JOB_SPECS`, in Python's order.
pub const JOB_SPECS: [JobSpec; 13] = [
    spec("bilibili", "Bilibili", &["yt-dlp"]),
    spec("hanime1", "Hanime1", &["yt-dlp"]),
    spec("jandan", "Jandan", &[]),
    spec("kemono", "Kemono", &[]),
    spec("nikke", "Nikke", &[]),
    spec("bd2", "BD2", &[]),
    spec("azurlane", "Azur Lane", &[]),
    spec("telegram", "Telegram", &[]),
    spec("stellasora", "Stella Sora", &[]),
    spec("twitter", "X", &["gallery-dl"]),
    spec("pixiv", "Pixiv", &[]),
    spec("rednote", "RedNote", &[]),
    spec("wechat", "WeChat", &[]),
];

const fn spec(key: &'static str, name: &'static str, required_commands: &'static [&'static str]) -> JobSpec {
    JobSpec {
        key,
        name,
        attr: key,
        required_commands,
    }
}

/// The spec for a job key.
pub fn job_spec(key: &str) -> Option<&'static JobSpec> {
    JOB_SPECS.iter().find(|spec| spec.key == key)
}

/// `ScheduledJob`: a source as the current settings configure it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledJob {
    pub key: String,
    pub name: String,
    pub cron: String,
    /// The UI toggle, and only when the section is complete enough to run.
    pub enabled: bool,
    pub required_commands: Vec<String>,
    pub section: String,
    pub notify: bool,
    pub missing_fields: Vec<String>,
}

impl ScheduledJob {
    /// `as_public_dict()`: what the API shows for a job.
    pub fn as_public_dict(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("key", Value::from(self.key.as_str()));
        dict.insert("name", Value::from(self.name.as_str()));
        dict.insert("cron", Value::from(self.cron.as_str()));
        dict.insert("enabled", Value::Bool(self.enabled));
        dict.insert("notify", Value::Bool(self.notify));
        dict.insert("section", Value::from(self.section.as_str()));
        dict.insert(
            "missing_fields",
            Value::List(self.missing_fields.iter().map(|field| Value::from(field.as_str())).collect()),
        );
        Value::Dict(dict)
    }
}

/// `build_jobs(current)`.
pub fn build_jobs(settings: &Settings) -> Vec<ScheduledJob> {
    JOB_SPECS
        .iter()
        .map(|spec| {
            let section = format!("web.{}", spec.attr);
            let schedule = settings
                .job_schedule(spec.attr)
                .unwrap_or_else(|| panic!("JOB_SPECS names {} but Web has no such section", spec.attr));
            let missing = settings
                .section(&section)
                .map(|value| value.validate_runnable())
                .unwrap_or_default();
            ScheduledJob {
                key: spec.key.to_owned(),
                name: spec.name.to_owned(),
                cron: schedule.cron.clone(),
                // A source with an incomplete configuration stays out of the scheduler
                // even when the UI toggle says enabled.
                enabled: schedule.enabled && missing.is_empty(),
                required_commands: spec.required_commands.iter().map(|command| (*command).to_owned()).collect(),
                section,
                notify: schedule.notify,
                missing_fields: missing,
            }
        })
        .collect()
}

/// `resolve_trigger_jobs(target, jobs)`: the jobs `fav trigger <target>` runs. The error is
/// the `SystemExit` message.
pub fn resolve_trigger_jobs(target: &str, all_jobs: &[ScheduledJob]) -> Result<Vec<ScheduledJob>, String> {
    let normalized = py_strip(target).to_lowercase();
    if normalized.is_empty() {
        return Err("Trigger target cannot be empty".to_owned());
    }
    if normalized == "all" {
        let selected: Vec<ScheduledJob> = all_jobs.iter().filter(|job| job.enabled).cloned().collect();
        if selected.is_empty() {
            return Err("No enabled jobs available for --trigger all".to_owned());
        }
        return Ok(selected);
    }
    if let Some(job) = all_jobs.iter().find(|job| job.key == normalized) {
        return Ok(vec![job.clone()]);
    }
    let mut keys: Vec<&str> = all_jobs.iter().map(|job| job.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    let valid = std::iter::once("all").chain(keys).collect::<Vec<_>>().join(", ");
    Err(format!("Unknown --trigger target: {}. Valid targets: {valid}", repr_str(target)))
}

#[cfg(test)]
mod tests;
