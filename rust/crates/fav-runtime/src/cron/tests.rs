use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use fav_core::golden;

use super::{CronTrigger, ZonedTime};

fn zone(name: &str) -> Tz {
    name.parse().unwrap_or_else(|error| panic!("unknown zone {name}: {error}"))
}

fn chain(trigger: &CronTrigger, now: ZonedTime, strict: bool) -> Vec<String> {
    let mut fires = Vec::new();
    let mut previous: Option<ZonedTime> = None;
    let mut now = now;
    for _ in 0..5 {
        let fire = match (&previous, strict) {
            (Some(previous), true) => trigger.next_fire_time_after(previous, &now),
            (previous, _) => trigger.next_fire_time(previous.as_ref(), &now),
        };
        let Some(fire) = fire else { break };
        fires.push(fire.isoformat());
        previous = Some(fire);
        now = fire;
    }
    fires
}

#[test]
fn next_fire_times_match_apscheduler() {
    let document = golden::load("scheduler/cron_next_fire.json");
    let mut checked = 0;
    for case in golden::cases(&document) {
        let cron = golden::string(golden::get(case, "cron"));
        let tz = zone(&golden::string(golden::get(case, "timezone")));
        let parsed = CronTrigger::from_crontab(&cron, tz);
        if let Some(message) = case.get("message") {
            let error = parsed.expect_err(&cron);
            assert_eq!(error.0, golden::string(message), "{cron:?}");
            continue;
        }
        let trigger = parsed.unwrap_or_else(|error| panic!("{cron:?}: {error}"));
        assert_eq!(trigger.to_string(), golden::string(golden::get(case, "trigger")), "{cron:?}");
        let now_text = golden::string(golden::get(case, "now"));
        let instant: DateTime<Utc> = DateTime::parse_from_rfc3339(&now_text).expect("now parses").to_utc();
        let now_zone = match golden::string(golden::get(case, "now_zone")).as_str() {
            "UTC" => Tz::UTC,
            _ => tz,
        };
        let now = ZonedTime::from_utc(instant, now_zone);
        assert_eq!(now.isoformat(), now_text);
        let expected: Vec<String> = golden::list(golden::get(case, "fires")).iter().map(golden::string).collect();
        assert_eq!(chain(&trigger, now, false), expected, "{cron:?} in {tz} from {now_text}");
        let strict: Vec<String> = case
            .get("strict")
            .map_or_else(|| expected.clone(), |strict| golden::list(strict).iter().map(golden::string).collect());
        assert_eq!(chain(&trigger, now, true), strict, "{cron:?} in {tz} from {now_text}, strict");
        checked += 1;
    }
    assert!(checked > 600, "only {checked} chains checked");
}

#[test]
fn weekday_numbers_start_on_monday() {
    let trigger = CronTrigger::from_crontab("0 9 * * 1", Tz::UTC).expect("parses");
    let now = ZonedTime::from_utc(DateTime::parse_from_rfc3339("2026-11-09T00:00:00Z").expect("time").to_utc(), Tz::UTC);
    // 2026-11-09 is a Monday; APScheduler's 1 is Tuesday.
    let fire = trigger.next_fire_time(None, &now).expect("fires");
    assert_eq!(fire.isoformat(), "2026-11-10T09:00:00+00:00");
}

#[test]
fn a_skipped_wall_time_keeps_the_offset_from_before_the_gap() {
    let new_york = zone("America/New_York");
    let gap = chrono::NaiveDate::from_ymd_opt(2026, 3, 8).and_then(|day| day.and_hms_opt(2, 30, 0)).expect("date");
    assert_eq!(ZonedTime::new(gap, false, new_york).isoformat(), "2026-03-08T02:30:00-05:00");
    assert_eq!(ZonedTime::new(gap, true, new_york).isoformat(), "2026-03-08T02:30:00-04:00");
}
