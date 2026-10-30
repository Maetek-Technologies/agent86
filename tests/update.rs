//! The new-version check against the mock server: GitHub's answer, the daily
//! cache, and the switches that turn it off.

mod support;

use agent86::update::{
    cache_path, enabled, latest, move_aside, remove_stale, Checked, Install, Version, DISABLE_ENV,
};
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

#[test]
fn a_binary_moves_to_the_temporary_directory_or_cargo_falls_back_beside_it() {
    let root = home("move");
    let package = root.join("pkg").join("bin");
    let temp = root.join("temp");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&temp).unwrap();
    let exe = package.join("agent86.exe");

    std::fs::write(&exe, "old").unwrap();
    let (from, to) = move_aside(&exe, &temp, 42, Install::Npm).unwrap();
    assert_eq!(
        (from, to.clone()),
        (exe.clone(), temp.join("agent86-old-42.exe"))
    );
    assert!(!exe.exists() && to.exists());

    // A temporary directory it cannot reach: npm is refused, cargo renames beside.
    std::fs::write(&exe, "old").unwrap();
    let nowhere = root.join("no-such-dir");
    assert!(move_aside(&exe, &nowhere, 7, Install::Npm).is_err());
    assert!(exe.exists());
    let (_, to) = move_aside(&exe, &nowhere, 7, Install::Cargo).unwrap();
    assert_eq!(to, package.join("agent86.old"));

    // What was left behind is cleared, and nothing else.
    std::fs::write(temp.join("agent86-old-9.exe"), "x").unwrap();
    std::fs::write(temp.join("keep.exe"), "x").unwrap();
    remove_stale(&temp);
    let left: Vec<String> = std::fs::read_dir(&temp)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["keep.exe"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// What broke `/update now` under npm and Volta: a package directory holding a
/// running binary cannot be removed or replaced on Windows, even after the
/// binary is renamed within it. Moved to the temporary directory, it can.
#[cfg(windows)]
#[test]
fn a_running_binary_moved_aside_no_longer_pins_its_package_directory() {
    use std::process::{Command, Stdio};

    let root = home("running");
    let package = root.join("pkg");
    let bin = package.join("bin");
    let temp = root.join("temp");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&temp).unwrap();
    let exe = bin.join("agent86.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_agent86"), &exe).unwrap();
    // The plain loop waits on stdin, so the copy keeps running.
    let mut child = Command::new(&exe)
        .arg("--plain")
        .current_dir(&root)
        .env("HF_HUB_OFFLINE", "1")
        .env(DISABLE_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let renamed = exe.with_extension("old");
    std::fs::rename(&exe, &renamed).unwrap();
    assert!(
        std::fs::remove_dir_all(&package).is_err(),
        "a running binary pins its directory even renamed"
    );
    std::fs::rename(&renamed, &exe).unwrap();

    let (_, aside) = move_aside(&exe, &temp, child.id(), Install::Npm).unwrap();
    std::fs::remove_dir_all(&package).unwrap();
    assert!(child.try_wait().unwrap().is_none(), "it is still running");

    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();
    remove_stale(&temp);
    assert!(!aside.exists());
    let _ = std::fs::remove_dir_all(&root);
}
