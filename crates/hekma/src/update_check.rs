use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Ktesio/hekma/releases/latest";
const CHECK_INTERVAL_SECS: u64 = 60 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateNotice {
    pub(crate) current_version: String,
    pub(crate) latest_version: String,
}

#[cfg(not(tarpaulin_include))]
pub(crate) fn maybe_notice() -> Option<UpdateNotice> {
    if !automatic_checks_enabled() {
        return None;
    }

    check_for_update(
        &UreqReleaseTransport::new(),
        &SystemClock,
        &default_cache_file(),
        env!("CARGO_PKG_VERSION"),
    )
}

fn check_for_update<T, C>(
    transport: &T,
    clock: &C,
    cache_file: &Path,
    current_version: &str,
) -> Option<UpdateNotice>
where
    T: ReleaseTransport,
    C: Clock,
{
    let now = clock.now_unix_secs();
    let cache = load_cache(cache_file);

    if cache
        .as_ref()
        .is_some_and(|cache| cache_is_fresh(cache, now))
    {
        return cache
            .and_then(|cache| cache.latest_tag)
            .and_then(|tag| notice_for(current_version, &tag));
    }

    let previous_tag = cache.and_then(|cache| cache.latest_tag);
    let latest_tag = transport.latest_release_tag().ok().or(previous_tag);

    let _ = save_cache(
        cache_file,
        &UpdateCheckCache {
            checked_at_unix_secs: now,
            latest_tag: latest_tag.clone(),
        },
    );

    latest_tag.and_then(|tag| notice_for(current_version, &tag))
}

fn notice_for(current_version: &str, latest_tag: &str) -> Option<UpdateNotice> {
    is_newer_version(current_version, latest_tag).then(|| UpdateNotice {
        current_version: current_version.to_string(),
        latest_version: display_version(latest_tag),
    })
}

fn is_newer_version(current_version: &str, latest_tag: &str) -> bool {
    let Some(current) = parse_version(current_version) else {
        return false;
    };
    let Some(latest) = parse_version(latest_tag) else {
        return false;
    };

    latest > current
}

fn parse_version(version: &str) -> Option<Version> {
    Version::parse(version.trim().trim_start_matches('v')).ok()
}

fn display_version(tag: &str) -> String {
    tag.trim().trim_start_matches('v').trim().to_string()
}

/// Opt-out for the passive update check — the LEGACY name, preserved
/// unchanged through the Hekma rename (v0.8.0) so existing environments
/// keep suppressing checks.
const UPDATE_OPT_OUT_ENV_LEGACY: &str = "KTESIO_NO_UPDATE_CHECK";
/// Opt-out, forward-looking alias (v0.8.0+). EITHER truthy name disables
/// the check; neither ever re-enables it.
const UPDATE_OPT_OUT_ENV: &str = "HEKMA_NO_UPDATE_CHECK";

fn automatic_checks_enabled() -> bool {
    !truthy_env(UPDATE_OPT_OUT_ENV_LEGACY) && !truthy_env(UPDATE_OPT_OUT_ENV) && !truthy_env("CI")
}

fn truthy_env(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            let value = value.trim();
            !value.is_empty()
                && !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
        })
        .unwrap_or(false)
}

fn default_cache_file() -> PathBuf {
    user_cache_dir()
        .unwrap_or_else(env::temp_dir)
        .join("ktesio")
        .join("update-check.json")
}

fn user_cache_dir() -> Option<PathBuf> {
    if let Some(cache_home) = non_empty_env_path("XDG_CACHE_HOME") {
        return Some(cache_home);
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = non_empty_env_path("HOME") {
            return Some(home.join("Library").join("Caches"));
        }
    }

    #[cfg(windows)]
    {
        if let Some(local_app_data) = non_empty_env_path("LOCALAPPDATA") {
            return Some(local_app_data);
        }
        if let Some(app_data) = non_empty_env_path("APPDATA") {
            return Some(app_data);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(home) = non_empty_env_path("HOME") {
            return Some(home.join(".cache"));
        }
    }

    None
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn cache_is_fresh(cache: &UpdateCheckCache, now: u64) -> bool {
    cache.checked_at_unix_secs >= now
        || now.saturating_sub(cache.checked_at_unix_secs) < CHECK_INTERVAL_SECS
}

fn load_cache(path: &Path) -> Option<UpdateCheckCache> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_cache(path: &Path, cache: &UpdateCheckCache) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(cache)?)?;
    Ok(())
}

trait ReleaseTransport {
    fn latest_release_tag(&self) -> Result<String, String>;
}

trait Clock {
    fn now_unix_secs(&self) -> u64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }
}

#[cfg(not(tarpaulin_include))]
struct UreqReleaseTransport {
    agent: ureq::Agent,
}

#[cfg(not(tarpaulin_include))]
impl UreqReleaseTransport {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl ReleaseTransport for UreqReleaseTransport {
    fn latest_release_tag(&self) -> Result<String, String> {
        let mut response = self
            .agent
            .get(LATEST_RELEASE_URL)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", concat!("hekma/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|error| error.to_string())?;

        if response.status() != 200 {
            return Err(format!("GitHub returned HTTP {}", response.status()));
        }

        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| error.to_string())?;
        let release: GitHubLatestRelease =
            serde_json::from_str(&body).map_err(|error| error.to_string())?;
        Ok(release.tag_name)
    }
}

#[derive(Debug, Deserialize)]
struct GitHubLatestRelease {
    tag_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCheckCache {
    checked_at_unix_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_tag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Clone)]
    struct FakeClock {
        now: u64,
    }

    impl Clock for FakeClock {
        fn now_unix_secs(&self) -> u64 {
            self.now
        }
    }

    struct FakeTransport {
        responses: RefCell<Vec<Result<String, String>>>,
        calls: Cell<usize>,
    }

    impl FakeTransport {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: RefCell::new(responses),
                calls: Cell::new(0),
            }
        }
    }

    impl ReleaseTransport for FakeTransport {
        fn latest_release_tag(&self) -> Result<String, String> {
            self.calls.set(self.calls.get() + 1);
            self.responses
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Err("no fake response".to_string()))
        }
    }

    fn cache_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("cache").join("update-check.json")
    }

    fn restore_env(name: &str, value: Option<OsString>) {
        if let Some(value) = value {
            env::set_var(name, value);
        } else {
            env::remove_var(name);
        }
    }

    #[test]
    fn test_version_comparison_handles_tags_and_prereleases() {
        assert!(is_newer_version("0.3.0", "v0.3.1"));
        assert!(is_newer_version("0.3.0", "0.4.0-rc.1"));
        assert!(!is_newer_version("0.3.0", "v0.3.0"));
        assert!(!is_newer_version("0.3.0", "v0.2.9"));
        assert!(!is_newer_version("0.4.0", "v0.4.0-rc.1"));
        assert!(!is_newer_version("not-a-version", "v0.4.0"));
        assert!(!is_newer_version("0.3.0", "not-a-version"));
    }

    #[test]
    fn test_fresh_cache_skips_transport_and_reports_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_file = cache_file(&dir);
        save_cache(
            &cache_file,
            &UpdateCheckCache {
                checked_at_unix_secs: 100,
                latest_tag: Some("v0.4.0".to_string()),
            },
        )
        .unwrap();
        let transport = FakeTransport::new(vec![Ok("v9.9.9".to_string())]);

        let notice = check_for_update(
            &transport,
            &FakeClock { now: 100 + 120 },
            &cache_file,
            "0.3.0",
        )
        .unwrap();

        assert_eq!(transport.calls.get(), 0);
        assert_eq!(notice.latest_version, "0.4.0");
    }

    #[test]
    fn test_stale_cache_fetches_and_saves_latest_tag() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_file = cache_file(&dir);
        save_cache(
            &cache_file,
            &UpdateCheckCache {
                checked_at_unix_secs: 1,
                latest_tag: Some("v0.3.1".to_string()),
            },
        )
        .unwrap();
        let transport = FakeTransport::new(vec![Ok("v0.5.0".to_string())]);

        let notice = check_for_update(
            &transport,
            &FakeClock {
                now: CHECK_INTERVAL_SECS + 10,
            },
            &cache_file,
            "0.3.0",
        )
        .unwrap();
        let saved = load_cache(&cache_file).unwrap();

        assert_eq!(transport.calls.get(), 1);
        assert_eq!(notice.latest_version, "0.5.0");
        assert_eq!(saved.checked_at_unix_secs, CHECK_INTERVAL_SECS + 10);
        assert_eq!(saved.latest_tag.as_deref(), Some("v0.5.0"));
    }

    #[test]
    fn test_failed_fetch_updates_checked_at_and_reuses_previous_tag() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_file = cache_file(&dir);
        save_cache(
            &cache_file,
            &UpdateCheckCache {
                checked_at_unix_secs: 1,
                latest_tag: Some("v0.4.0".to_string()),
            },
        )
        .unwrap();
        let transport = FakeTransport::new(vec![Err("offline".to_string())]);

        let notice = check_for_update(
            &transport,
            &FakeClock {
                now: CHECK_INTERVAL_SECS + 20,
            },
            &cache_file,
            "0.3.0",
        )
        .unwrap();
        let saved = load_cache(&cache_file).unwrap();

        assert_eq!(transport.calls.get(), 1);
        assert_eq!(notice.latest_version, "0.4.0");
        assert_eq!(saved.checked_at_unix_secs, CHECK_INTERVAL_SECS + 20);
        assert_eq!(saved.latest_tag.as_deref(), Some("v0.4.0"));
    }

    #[test]
    fn test_failed_fetch_without_previous_tag_still_writes_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_file = cache_file(&dir);
        let transport = FakeTransport::new(vec![Err("offline".to_string())]);

        let notice = check_for_update(&transport, &FakeClock { now: 42 }, &cache_file, "0.3.0");
        let saved = load_cache(&cache_file).unwrap();

        assert!(notice.is_none());
        assert_eq!(transport.calls.get(), 1);
        assert_eq!(saved.checked_at_unix_secs, 42);
        assert!(saved.latest_tag.is_none());
    }

    #[test]
    fn test_equal_cached_version_does_not_report_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache_file = cache_file(&dir);
        let transport = FakeTransport::new(vec![Ok("v0.3.0".to_string())]);

        let notice = check_for_update(&transport, &FakeClock { now: 42 }, &cache_file, "0.3.0");

        assert!(notice.is_none());
    }

    #[test]
    fn test_truthy_env_handles_common_false_values() {
        let name = "KTESIO_TEST_TRUTHY_ENV";
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var(name);
        assert!(!truthy_env(name));

        env::set_var(name, "1");
        assert!(truthy_env(name));

        for value in ["", "0", "false", "no", "off"] {
            env::set_var(name, value);
            assert!(!truthy_env(name), "{value:?} should be false");
        }

        env::remove_var(name);
    }

    #[test]
    fn test_default_cache_file_uses_legacy_ktesio_leaf() {
        let path = default_cache_file();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("update-check.json")
        );
        assert!(path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "ktesio"));
    }

    #[test]
    fn test_non_empty_env_path_filters_empty_values() {
        let name = "KTESIO_TEST_EMPTY_PATH";
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var(name, "");
        assert!(non_empty_env_path(name).is_none());

        env::set_var(name, "/tmp/hekma-cache");
        assert_eq!(
            non_empty_env_path(name),
            Some(PathBuf::from("/tmp/hekma-cache"))
        );

        env::remove_var(name);
    }

    #[test]
    fn test_automatic_checks_enabled_obeys_ci_and_opt_out() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original_ci = env::var_os("CI");
        let original_opt_out = env::var_os("KTESIO_NO_UPDATE_CHECK");
        let original_alias = env::var_os("HEKMA_NO_UPDATE_CHECK");

        env::remove_var("CI");
        env::remove_var("KTESIO_NO_UPDATE_CHECK");
        env::remove_var("HEKMA_NO_UPDATE_CHECK");
        assert!(automatic_checks_enabled());

        env::set_var("CI", "true");
        assert!(!automatic_checks_enabled());

        env::remove_var("CI");
        env::set_var("KTESIO_NO_UPDATE_CHECK", "1");
        assert!(!automatic_checks_enabled());

        env::set_var("KTESIO_NO_UPDATE_CHECK", "false");
        assert!(automatic_checks_enabled());

        // The forward-looking alias disables checks on its own, and EITHER
        // truthy name wins even when the other is explicitly "false".
        env::remove_var("KTESIO_NO_UPDATE_CHECK");
        env::set_var("HEKMA_NO_UPDATE_CHECK", "1");
        assert!(!automatic_checks_enabled());

        env::set_var("KTESIO_NO_UPDATE_CHECK", "false");
        assert!(!automatic_checks_enabled());

        restore_env("CI", original_ci);
        restore_env("KTESIO_NO_UPDATE_CHECK", original_opt_out);
        restore_env("HEKMA_NO_UPDATE_CHECK", original_alias);
    }

    #[test]
    fn test_user_cache_dir_prefers_xdg_then_home_cache() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original_xdg = env::var_os("XDG_CACHE_HOME");
        let original_home = env::var_os("HOME");

        env::set_var("XDG_CACHE_HOME", "/tmp/hekma-xdg-cache");
        env::set_var("HOME", "/tmp/hekma-home");
        assert_eq!(
            user_cache_dir(),
            Some(PathBuf::from("/tmp/hekma-xdg-cache"))
        );

        env::remove_var("XDG_CACHE_HOME");
        #[cfg(target_os = "macos")]
        let expected_home_cache = PathBuf::from("/tmp/hekma-home")
            .join("Library")
            .join("Caches");
        #[cfg(all(unix, not(target_os = "macos")))]
        let expected_home_cache = PathBuf::from("/tmp/hekma-home").join(".cache");
        #[cfg(windows)]
        let expected_home_cache = user_cache_dir().unwrap();
        assert_eq!(user_cache_dir(), Some(expected_home_cache));

        env::remove_var("HOME");
        #[cfg(unix)]
        assert_eq!(user_cache_dir(), None);

        restore_env("XDG_CACHE_HOME", original_xdg);
        restore_env("HOME", original_home);
    }

    #[test]
    fn test_system_clock_returns_epoch_or_later() {
        assert!(SystemClock.now_unix_secs() > 0);
    }

    #[test]
    fn test_cache_freshness_treats_future_cache_as_fresh() {
        assert!(cache_is_fresh(
            &UpdateCheckCache {
                checked_at_unix_secs: 200,
                latest_tag: None,
            },
            100
        ));
        assert!(cache_is_fresh(
            &UpdateCheckCache {
                checked_at_unix_secs: 100,
                latest_tag: None,
            },
            100 + CHECK_INTERVAL_SECS - 1
        ));
        assert!(!cache_is_fresh(
            &UpdateCheckCache {
                checked_at_unix_secs: 100,
                latest_tag: None,
            },
            100 + CHECK_INTERVAL_SECS
        ));
    }
}
