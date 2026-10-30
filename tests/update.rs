//! The new-version check against the mock server: GitHub's answer, the daily
//! cache, and the switches that turn it off.

mod support;

use agent86::update::{cache_path, enabled, latest, Checked, Version, DISABLE_ENV};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::{MockServer, Reply};

fn ok(body: &str) -> Reply {
    Reply::Status {
        status: 200,
        headers: vec![("Content-Type", "application/json".to_owned())],
        body: body.to_owned(),
    }
}

fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent86-update-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

const TIMEOUT: Duration = Duration::from_secs(5);

// The check blocks its thread while the mock server answers on another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_latest_release_is_asked_for_once_a_day_and_cached() {
    let server = MockServer::start(vec![
        ok(r#"{"tag_name": "v1.2.3", "name": "agent86 v1.2.3"}"#),
        ok(r#"{"tag_name": "v1.2.4"}"#),
    ])
    .await;
    let url = format!("{}/repos/x/releases/latest", server.base_url);
    let home = home("cache");
    let cache = cache_path(&home);

    assert_eq!(latest(&url, &cache, false, TIMEOUT), Ok(Version(1, 2, 3)));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .header("user-agent")
        .unwrap()
        .starts_with("agent86/"));
    let checked: Checked = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert_eq!(checked.latest, "1.2.3");

    // Within the day the cache answers, with no request.
    assert_eq!(latest(&url, &cache, false, TIMEOUT), Ok(Version(1, 2, 3)));
    assert_eq!(server.requests().len(), 1);
    // `/update` asks anyway.
    assert_eq!(latest(&url, &cache, true, TIMEOUT), Ok(Version(1, 2, 4)));
    assert_eq!(server.requests().len(), 2);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_cache_is_checked_again_and_failures_say_why() {
    let server = MockServer::start(vec![
        ok(r#"{"tag_name": "v2.0.0"}"#),
        ok(r#"{"tag_name": "nightly"}"#),
        Reply::Status {
            status: 403,
            headers: Vec::new(),
            body: "rate limited".to_owned(),
        },
    ])
    .await;
    let url = format!("{}/latest", server.base_url);
    let home = home("stale");
    let cache = cache_path(&home);
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let day_old = Checked {
        checked_at: now() - 25 * 60 * 60,
        latest: "1.0.0".to_owned(),
    };
    std::fs::write(&cache, serde_json::to_vec(&day_old).unwrap()).unwrap();

    assert_eq!(latest(&url, &cache, false, TIMEOUT), Ok(Version(2, 0, 0)));
    let error = latest(&url, &cache, true, TIMEOUT).unwrap_err();
    assert!(error.contains("\"nightly\""), "{error}");
    let error = latest(&url, &cache, true, TIMEOUT).unwrap_err();
    assert!(error.contains("403"), "{error}");
    // A failed check leaves the last good answer cached.
    let checked: Checked = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    assert_eq!(checked.latest, "2.0.0");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_environment_turns_the_check_off() {
    // This test binary has no other test reading the variable.
    assert!(enabled(true));
    std::env::set_var(DISABLE_ENV, "1");
    assert!(!enabled(true));
    std::env::set_var(DISABLE_ENV, "");
    assert!(enabled(true));
    std::env::remove_var(DISABLE_ENV);
    assert!(!enabled(false));
}
