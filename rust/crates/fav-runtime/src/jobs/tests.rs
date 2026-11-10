use fav_core::golden;
use fav_core::pyjson::Value;
use fav_db::settings_store::default_settings;

use super::{JOB_SPECS, build_jobs, resolve_trigger_jobs};

#[test]
fn job_specs_match_python() {
    let document = golden::load("jobs/job_specs.json");
    let cases = golden::cases(&document);
    assert_eq!(cases.len(), JOB_SPECS.len());
    for (case, spec) in cases.iter().zip(JOB_SPECS.iter()) {
        assert_eq!(golden::string(golden::get(case, "key")), spec.key);
        assert_eq!(golden::string(golden::get(case, "name")), spec.name);
        assert_eq!(golden::string(golden::get(case, "attr")), spec.attr);
        let commands: Vec<String> = golden::list(golden::get(case, "required_commands"))
            .iter()
            .map(golden::string)
            .collect();
        assert_eq!(commands, spec.required_commands);
    }
}

#[test]
fn default_jobs_match_python() {
    let document = golden::load("jobs/default_jobs.json");
    let jobs = build_jobs(&default_settings());
    let public: Vec<Value> = jobs.iter().map(super::ScheduledJob::as_public_dict).collect();
    assert_eq!(public.as_slice(), golden::cases(&document));
}

#[test]
fn trigger_targets_match_python() {
    let document = golden::load("jobs/trigger_targets.json");
    for case in golden::cases(&document) {
        let mut settings = default_settings();
        for key in golden::list(golden::get(case, "enabled")) {
            settings.job_schedule_mut(&golden::string(key)).expect("a job key").enabled = true;
        }
        let jobs = build_jobs(&settings);
        let target = golden::string(golden::get(case, "target"));
        let resolved = resolve_trigger_jobs(&target, &jobs).map(|selected| selected.into_iter().map(|job| job.key).collect::<Vec<_>>());
        match (case.get("selected"), case.get("exit")) {
            (Some(selected), None) => {
                let expected: Vec<String> = golden::list(selected).iter().map(golden::string).collect();
                assert_eq!(resolved, Ok(expected), "{target:?}");
            }
            (None, Some(exit)) => assert_eq!(resolved, Err(golden::string(exit)), "{target:?}"),
            _ => panic!("malformed case {case:?}"),
        }
    }
}
