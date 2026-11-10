//! Registry completeness (WP18): the worker has a runner for exactly Python's `JOB_SPECS`, with
//! Python's names, default schedules and settings sections, and the post-link dispatcher knows
//! exactly Python's four kinds of post. Routes and MCP tools are counted in `fav-api`'s tests
//! (`the_contract_has_python_routes_and_tools`).

use std::collections::BTreeSet;
use std::sync::Arc;

use fav_core::golden;
use fav_db::Db;
use fav_runtime::jobs::{JOB_SPECS, build_jobs};

use crate::SourceContext;

fn context() -> SourceContext {
    let db = Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool");
    SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    }
}

#[tokio::test]
async fn the_registry_has_exactly_python_job_specs() {
    let specs = golden::load("jobs/job_specs.json");
    let python: Vec<String> = golden::cases(&specs).iter().map(|case| golden::string(golden::get(case, "key"))).collect();
    assert_eq!(python.len(), 13);
    let mut sorted = python.clone();
    sorted.sort();

    let registry = crate::registry(&context());

    assert_eq!(registry.keys(), sorted.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(JOB_SPECS.iter().map(|spec| spec.key).collect::<Vec<_>>(), python);
}

#[tokio::test]
async fn every_registered_job_keeps_python_name_schedule_and_settings_section() {
    let document = golden::load("jobs/default_jobs.json");
    let python = golden::cases(&document);
    let jobs = build_jobs(&fav_db::settings_store::default_settings());
    let registry = crate::registry(&context());
    assert_eq!(jobs.len(), python.len());
    for (job, case) in jobs.iter().zip(python) {
        assert!(registry.contains(&job.key), "{} has no runner", job.key);
        assert_eq!(job.key, golden::string(golden::get(case, "key")));
        assert_eq!(job.name, golden::string(golden::get(case, "name")), "{}", job.key);
        assert_eq!(job.cron, golden::string(golden::get(case, "cron")), "{}", job.key);
        let spec = JOB_SPECS.iter().find(|spec| spec.key == job.key).expect("a spec");
        assert_eq!(format!("web.{}", spec.attr), golden::string(golden::get(case, "section")), "{}", job.key);
    }
}

#[test]
fn the_post_link_dispatcher_knows_python_four_kinds() {
    let document = golden::load("post_links/parsing.json");
    let mut python = BTreeSet::new();
    for case in golden::cases(&document) {
        if golden::string(golden::get(case, "fn")) == "post_links.site_name"
            && let Some(result) = case.get("result").and_then(|result| result.as_str())
        {
            python.insert(result.to_owned());
        }
    }
    let expected: BTreeSet<String> = ["NGA", "Tieba", "WeChat article", "Zhihu"].map(str::to_owned).into();
    assert_eq!(python, expected);

    let mut rust = BTreeSet::new();
    for url in [
        "https://tieba.baidu.com/p/9876543210",
        "https://bbs.nga.cn/read.php?tid=12345678",
        "https://www.zhihu.com/question/1/answer/2",
        "https://mp.weixin.qq.com/s/abcdefghijklmnop",
    ] {
        let link = crate::post_links::link_for_url(url, "", "").expect("parses").expect("a post link");
        rust.insert(crate::post_links::site_name(&link).expect("a site").to_owned());
    }
    assert_eq!(rust, expected);
}
