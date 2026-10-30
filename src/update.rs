//! The new-version notice and `/update`, beyond Python.
//!
//! The interactive surfaces ask GitHub for the latest release at most once a day,
//! on a thread of their own, and say so when it is newer than this build. The
//! answer is cached in `~/.agent86/update-check.json`, so most starts make no
//! request at all. `run` and `run --json` never check, since scripts read their
//! output. `[ui] update_check = false` or `AGENT86_NO_UPDATE_CHECK` turns it off.
//!
//! `/update` says how to update for the way this binary was installed, and
//! `/update now` does it: the npm or cargo command, or the release page for an
//! archive install, which has no installer to run.

use crate::provider::catalog::get_json;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where agent86 is published.
pub const REPOSITORY: &str = "Maetek-Technologies/agent86";
/// Set to anything to turn the check off.
pub const DISABLE_ENV: &str = "AGENT86_NO_UPDATE_CHECK";
/// Set to `npm` by the npm package's launcher, which is how an npm install is told
/// apart from the others.
pub const INSTALLER_ENV: &str = "AGENT86_INSTALLER";

/// How long a checked answer is trusted.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The background check gives up quickly: it is never worth waiting for.
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(5);
/// `/update` waits a little longer, since the user asked.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// GitHub's latest-release endpoint for agent86.
pub fn latest_release_url() -> String {
    format!("https://api.github.com/repos/{REPOSITORY}/releases/latest")
}

/// The page of one release.
pub fn release_page(version: &Version) -> String {
    format!("https://github.com/{REPOSITORY}/releases/tag/v{version}")
}

/// A release version, `major.minor.patch`, ordered numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `1.0.6` or a tag such as `v1.0.6`. Anything else (a pre-release suffix
    /// included) is not a release this check offers.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.').map(|part| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        });
        let version = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }

    /// This build's version.
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Self(0, 0, 0))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// How this binary was installed, which decides how it is updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// `npm install -g agent86`; its launcher sets [`INSTALLER_ENV`].
    Npm,
    /// `cargo install`, into cargo's `bin` directory.
    Cargo,
    /// An archive from the releases page, put on `PATH` by hand.
    Archive,
}

impl Install {
    /// Work out how `exe` was installed. `installer` is [`INSTALLER_ENV`]'s value
    /// and `cargo_bin` cargo's `bin` directory.
    pub fn detect(exe: &Path, installer: Option<&str>, cargo_bin: Option<&Path>) -> Self {
        if installer == Some("npm") {
            return Self::Npm;
        }
        let in_cargo_bin = cargo_bin.is_some_and(|bin| {
            let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => a == b,
            };
            exe.parent().is_some_and(|dir| same(dir, bin))
        });
        if in_cargo_bin {
            Self::Cargo
        } else {
            Self::Archive
        }
    }

    /// This process's install.
    pub fn of_this_process() -> Self {
        let exe = std::env::current_exe().unwrap_or_default();
        let installer = std::env::var(INSTALLER_ENV).ok();
        Self::detect(&exe, installer.as_deref(), cargo_bin().as_deref())
    }

    /// The command that installs `version`, or `None` for an archive install.
    pub fn command(self, version: &Version) -> Option<Vec<String>> {
        let words: Vec<String> = match self {
            Self::Npm => vec![
                "npm".into(),
                "install".into(),
                "-g".into(),
                format!("agent86@{version}"),
            ],
            Self::Cargo => vec![
                "cargo".into(),
                "install".into(),
                "--git".into(),
                format!("https://github.com/{REPOSITORY}"),
                "--tag".into(),
                format!("v{version}"),
                "--locked".into(),
                "--force".into(),
            ],
            Self::Archive => return None,
        };
        Some(words)
    }
}

/// `$CARGO_HOME/bin`, or `~/.cargo/bin`.
fn cargo_bin() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::config::home_dir().map(|home| home.join(".cargo")))
        .map(|home| home.join("bin"))
}

/// The last answer, as cached on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checked {
    /// Seconds since the Unix epoch.
    pub checked_at: u64,
    /// The latest release's version.
    pub latest: String,
}

/// The cache file, `~/.agent86/update-check.json`.
pub fn cache_path(home: &Path) -> PathBuf {
    home.join(".agent86").join("update-check.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn read_cache(path: &Path) -> Option<Checked> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cache(path: &Path, checked: &Checked) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec(checked) {
        let _ = std::fs::write(path, text);
    }
}

/// The latest release: from the cache when it was checked within a day (unless
/// `force`), else from `url`, caching the answer.
pub fn latest(url: &str, cache: &Path, force: bool, timeout: Duration) -> Result<Version, String> {
    let now = now_secs();
    if !force {
        if let Some(checked) = read_cache(cache) {
            let fresh = now.saturating_sub(checked.checked_at) < CHECK_EVERY.as_secs();
            if let (true, Some(version)) = (fresh, Version::parse(&checked.latest)) {
                return Ok(version);
            }
        }
    }
    let headers = [
        ("Accept", "application/vnd.github+json".to_owned()),
        // GitHub refuses requests without one.
        (
            "User-Agent",
            format!("agent86/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    let body = get_json(url, &headers, timeout).map_err(|error| error.0)?;
    let tag = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let version =
        Version::parse(tag).ok_or_else(|| format!("the latest release has no version: {tag:?}"))?;
    write_cache(
        cache,
        &Checked {
            checked_at: now,
            latest: version.to_string(),
        },
    );
    Ok(version)
}

/// Whether the background check may run: the setting is on and the environment
/// does not turn it off.
pub fn enabled(setting: bool) -> bool {
    setting && std::env::var_os(DISABLE_ENV).is_none_or(|value| value.is_empty())
}

/// The startup notice for `latest`, when it is newer than `current`.
pub fn notice(current: &Version, latest: &Version) -> Option<String> {
    (latest > current).then(|| {
        format!("agent86 {latest} is available (you have {current}) · /update to install it")
    })
}

/// Check in the background, and hand `on_notice` the notice when there is a newer
/// release. Failures are silent: a missing network must not bother anyone. Also
/// clears away the binary a previous `/update` moved aside on Windows.
pub fn start(setting: bool, on_notice: impl FnOnce(String) + Send + 'static) {
    remove_moved_aside();
    if !enabled(setting) {
        return;
    }
    let Some(home) = crate::config::home_dir() else {
        return;
    };
    std::thread::spawn(move || {
        let cache = cache_path(&home);
        if let Ok(latest) = latest(&latest_release_url(), &cache, false, BACKGROUND_TIMEOUT) {
            if let Some(text) = notice(&Version::current(), &latest) {
                on_notice(text);
            }
        }
    });
}

/// What `/update` found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// This build is the latest.
    Current(Version),
    /// A newer release, and how to install it.
    Newer {
        current: Version,
        latest: Version,
        install: Install,
    },
}

impl Plan {
    /// What `/update` says.
    pub fn describe(&self) -> String {
        match self {
            Self::Current(version) => format!("agent86 {version} is the latest release"),
            Self::Newer {
                current,
                latest,
                install,
            } => match install.command(latest) {
                Some(command) => format!(
                    "agent86 {latest} is available (you have {current}). /update now runs: {}",
                    command.join(" ")
                ),
                None => format!(
                    "agent86 {latest} is available (you have {current}). Download it from {}, \
                     or /update now to open that page",
                    release_page(latest)
                ),
            },
        }
    }
}

/// Ask GitHub now, ignoring the cache, and plan the update for this process.
pub fn plan() -> Result<Plan, String> {
    let home = crate::config::home_dir().ok_or("no home directory to cache the answer in")?;
    let latest = latest(
        &latest_release_url(),
        &cache_path(&home),
        true,
        COMMAND_TIMEOUT,
    )?;
    let current = Version::current();
    Ok(if latest > current {
        Plan::Newer {
            current,
            latest,
            install: Install::of_this_process(),
        }
    } else {
        Plan::Current(current)
    })
}

/// Carry out `plan`. `inherit` lets the installer write to the terminal (the plain
/// loop); otherwise its output is captured, and the last lines come back on failure.
pub fn apply(plan: &Plan, inherit: bool) -> Result<String, String> {
    let Plan::Newer {
        latest, install, ..
    } = plan
    else {
        return Ok(plan.describe());
    };
    let Some(words) = install.command(latest) else {
        let page = release_page(latest);
        open_in_browser(&page).map_err(|error| format!("could not open {page}: {error}"))?;
        return Ok(format!("opened {page}"));
    };
    let moved = if cfg!(windows) {
        let exe = std::env::current_exe()
            .map_err(|error| format!("could not find the running binary: {error}"))?;
        Some(
            move_aside(&exe, &std::env::temp_dir(), std::process::id(), *install).map_err(
                |error| {
                    format!(
                        "could not move the running binary out of the installer's way \
                         ({error}); quit agent86 and run: {}",
                        words.join(" ")
                    )
                },
            )?,
        )
    } else {
        None
    };
    let result = run(&words, inherit);
    if let (Err(error), Some((from, to))) = (&result, &moved) {
        // Nothing was installed, so the old binary goes back where it was. If the
        // installer took its directory away, say so rather than leave it broken.
        if std::fs::rename(to, from).is_err() && !from.exists() {
            return Err(format!(
                "{error}\nThe old binary could not be put back; quit agent86 and run: {}",
                words.join(" ")
            ));
        }
    }
    result.map(|()| format!("agent86 {latest} installed; restart agent86 to use it"))
}

fn run(words: &[String], inherit: bool) -> Result<(), String> {
    // npm is a batch file on Windows, which only cmd.exe runs.
    let mut command = if cfg!(windows) && words[0] == "npm" {
        let mut command = Command::new("cmd");
        command.arg("/C").args(words);
        command
    } else {
        let mut command = Command::new(&words[0]);
        command.args(&words[1..]);
        command
    };
    command.stdin(Stdio::null());
    if inherit {
        let status = command
            .status()
            .map_err(|error| format!("could not run {}: {error}", words[0]))?;
        return if status.success() {
            Ok(())
        } else {
            Err(format!("{} failed ({status})", words.join(" ")))
        };
    }
    let output = command
        .output()
        .map_err(|error| format!("could not run {}: {error}", words[0]))?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = text.lines().rev().take(5).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(format!(
        "{} failed ({}):\n{}",
        words.join(" "),
        output.status,
        tail.join("\n")
    ))
}

/// What a binary moved aside by `/update` is named: `agent86-old-<pid>.exe` in the
/// temporary directory, or `agent86.old` beside itself.
const MOVED_PREFIX: &str = "agent86-old-";
const MOVED_SIBLING: &str = "old";

/// Windows will not overwrite a running executable, nor remove the directory that
/// holds one, but it will move one within a volume. So before the installer runs,
/// the running binary is moved out of its directory into `temp`: npm (and Volta,
/// which replaces the whole package directory) can then install freely. If `temp`
/// is on another volume, a cargo install, which replaces only the file, can make do
/// with a rename beside it; an npm install cannot, so it is refused. Elsewhere the
/// installer replaces the file while the process keeps the old one open, so
/// nothing moves. Returns where the binary was and where it went.
pub fn move_aside(
    exe: &Path,
    temp: &Path,
    pid: u32,
    install: Install,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let aside = temp.join(format!("{MOVED_PREFIX}{pid}.exe"));
    let _ = std::fs::remove_file(&aside);
    match std::fs::rename(exe, &aside) {
        Ok(()) => Ok((exe.to_path_buf(), aside)),
        Err(error) if install != Install::Cargo => Err(error),
        Err(_) => {
            let aside = exe.with_extension(MOVED_SIBLING);
            let _ = std::fs::remove_file(&aside);
            std::fs::rename(exe, &aside)?;
            Ok((exe.to_path_buf(), aside))
        }
    }
}

/// Remove the binaries earlier `/update`s moved aside, once they no longer run (a
/// running one cannot be deleted, so it is left for next time).
fn remove_moved_aside() {
    if !cfg!(windows) {
        return;
    }
    remove_stale(&std::env::temp_dir());
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension(MOVED_SIBLING));
    }
}

/// Delete `agent86-old-*.exe` in `dir`, skipping any that are still running.
pub fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(MOVED_PREFIX) && name.ends_with(".exe") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        // `start` takes its first quoted argument as a window title.
        command.args(["/C", "start", ""]).arg(url);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("{status}")))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_from_tags_and_order_numerically() {
        assert_eq!(Version::parse("v1.0.6"), Some(Version(1, 0, 6)));
        assert_eq!(Version::parse(" 1.10.0 "), Some(Version(1, 10, 0)));
        for bad in [
            "",
            "v1.0",
            "1.0.6-rc.1",
            "1.0.6.1",
            "v1..6",
            "x1.0.6",
            "1.0.+6",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(Version(1, 10, 0) > Version(1, 9, 9));
        assert!(Version(2, 0, 0) > Version(1, 99, 99));
        assert_eq!(Version(1, 0, 6).to_string(), "1.0.6");
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_notice_only_names_a_newer_release() {
        let current = Version(1, 0, 5);
        assert_eq!(notice(&current, &Version(1, 0, 5)), None);
        assert_eq!(notice(&current, &Version(1, 0, 4)), None);
        assert_eq!(
            notice(&current, &Version(1, 0, 6)).unwrap(),
            "agent86 1.0.6 is available (you have 1.0.5) · /update to install it"
        );
    }

    #[test]
    fn the_install_is_told_by_the_launcher_or_cargo_s_bin() {
        let dir = std::env::temp_dir().join(format!("agent86-update-{}", std::process::id()));
        let bin = dir.join("cargo").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("agent86");
        assert_eq!(Install::detect(&exe, None, Some(&bin)), Install::Cargo);
        assert_eq!(Install::detect(&exe, Some("npm"), Some(&bin)), Install::Npm);
        let elsewhere = dir.join("tools").join("agent86");
        assert_eq!(
            Install::detect(&elsewhere, None, Some(&bin)),
            Install::Archive
        );
        assert_eq!(
            Install::detect(&elsewhere, Some(""), None),
            Install::Archive
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_install_has_its_command_or_the_release_page() {
        let latest = Version(1, 0, 6);
        assert_eq!(
            Install::Npm.command(&latest).unwrap().join(" "),
            "npm install -g agent86@1.0.6"
        );
        assert_eq!(
            Install::Cargo.command(&latest).unwrap().join(" "),
            "cargo install --git https://github.com/Maetek-Technologies/agent86 --tag v1.0.6 --locked --force"
        );
        assert_eq!(Install::Archive.command(&latest), None);
        let plan = Plan::Newer {
            current: Version(1, 0, 5),
            latest,
            install: Install::Archive,
        };
        assert_eq!(
            plan.describe(),
            "agent86 1.0.6 is available (you have 1.0.5). Download it from \
             https://github.com/Maetek-Technologies/agent86/releases/tag/v1.0.6, or /update now to open that page"
        );
        assert_eq!(
            Plan::Current(Version(1, 0, 5)).describe(),
            "agent86 1.0.5 is the latest release"
        );
    }

    #[test]
    fn the_setting_and_the_environment_can_turn_it_off() {
        // The environment is shared by every test in this process, so only the
        // setting is varied here; `tests/update.rs` covers the variable.
        assert!(!enabled(false));
    }
}
