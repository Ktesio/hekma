//! Story 24-2 (T6, spine AD-23): does the service manager keep agents in
//! their own Shim running when this host stops? Read-only and honest: the
//! check runs once at start, reads what the OS admits, and says `unverified`
//! — with how to check — wherever it cannot tell. It never refuses to start
//! (`unsafe` warns: drain-and-stop is a working configuration).
//!
//! systemd: a unit's `KillMode` decides — `process` or `none` leave the
//! Shims alone; `control-group` (the default) or `mixed` stop every process
//! in the unit's cgroup with the host. Found from `INVOCATION_ID` (set for
//! every unit) and the unit name in `/proc/self/cgroup`. Other managers
//! (launchd, Docker, supervisord) and Windows job breakaway are not
//! checked yet.

/// The verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffCheck {
    /// The manager leaves the Shims running (the setting it reported).
    Verified(String),
    /// The manager stops them with the host (the setting it reported).
    Unsafe(String),
    /// It cannot be told here (why, and how to check).
    Unverified(String),
}

impl HandoffCheck {
    /// The wire form (`verified`, `unsafe`, `unverified`).
    pub fn as_str(&self) -> &'static str {
        match self {
            HandoffCheck::Verified(_) => "verified",
            HandoffCheck::Unsafe(_) => "unsafe",
            HandoffCheck::Unverified(_) => "unverified",
        }
    }

    /// The detail: the setting, or why it is unverified.
    pub fn detail(&self) -> &str {
        match self {
            HandoffCheck::Verified(detail)
            | HandoffCheck::Unsafe(detail)
            | HandoffCheck::Unverified(detail) => detail,
        }
    }

    /// The started notice's line.
    pub fn note(&self) -> String {
        match self {
            HandoffCheck::Verified(setting) => format!(
                "the service manager keeps agents in their own Shim running when this host stops \
                 ({setting})"
            ),
            HandoffCheck::Unsafe(setting) => format!(
                "the service manager stops every process of this host's unit with it ({setting}), \
                 agents in their own Shim included: set KillMode=process (or none) on the unit so \
                 Handoff Mode and in-place upgrades keep them running"
            ),
            HandoffCheck::Unverified(why) => format!(
                "whether the service manager keeps agents running when this host stops is \
                 unverified: {why}"
            ),
        }
    }
}

const HOW_TO_CHECK: &str = "check that your service manager stops only the host's own process \
     (systemd: KillMode=process; launchd: AbandonProcessGroup=true; a container stops every \
     process in it)";

/// The verdict for this process.
pub fn check() -> HandoffCheck {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok();
    check_with(
        std::env::var("INVOCATION_ID").ok().as_deref(),
        cgroup.as_deref(),
        &query_kill_mode,
    )
}

/// The verdict from what the OS reported: the `INVOCATION_ID`, the
/// `/proc/self/cgroup` text, and a `KillMode` query for a unit (the seam).
pub(crate) fn check_with(
    invocation_id: Option<&str>,
    cgroup: Option<&str>,
    query: &dyn Fn(&str, bool) -> Option<String>,
) -> HandoffCheck {
    if invocation_id.is_none_or(str::is_empty) {
        return HandoffCheck::Unverified(format!("not started by systemd; {HOW_TO_CHECK}"));
    }
    let Some((unit, user)) = cgroup.and_then(systemd_unit) else {
        return HandoffCheck::Unverified(format!(
            "under systemd, but its unit could not be named; {HOW_TO_CHECK}"
        ));
    };
    match query(&unit, user).as_deref().map(str::trim) {
        Some(mode @ ("process" | "none")) => {
            HandoffCheck::Verified(format!("{unit}: KillMode={mode}"))
        }
        Some(mode @ ("control-group" | "mixed")) => {
            HandoffCheck::Unsafe(format!("{unit}: KillMode={mode}"))
        }
        Some(other) => HandoffCheck::Unverified(format!(
            "{unit} reports KillMode={other}, which Hekma does not know; {HOW_TO_CHECK}"
        )),
        None => HandoffCheck::Unverified(format!(
            "{unit}'s KillMode could not be read; {HOW_TO_CHECK}"
        )),
    }
}

/// The systemd unit a cgroup v2 (or v1 `name=systemd`) path names, and
/// whether it is a user unit.
fn systemd_unit(cgroup: &str) -> Option<(String, bool)> {
    let line = cgroup
        .lines()
        .find(|line| line.starts_with("0::"))
        .or_else(|| cgroup.lines().find(|line| line.contains("name=systemd")))?;
    let path = line.rsplit_once(':')?.1;
    let user = path.contains("/user@");
    let unit = path
        .split('/')
        .rev()
        .find(|part| part.ends_with(".service") || part.ends_with(".scope"))?;
    Some((unit.to_string(), user))
}

/// `systemctl [--user] show --value -p KillMode <unit>`.
fn query_kill_mode(unit: &str, user: bool) -> Option<String> {
    let mut command = std::process::Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    let output = command
        .args(["show", "--value", "-p", "KillMode", unit])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|mode| !mode.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM: &str = "0::/system.slice/hekma.service\n";
    const USER: &str = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/hekma.service\n";

    #[test]
    fn systemd_kill_modes_classify_and_everything_else_is_unverified() {
        let reports = |mode: &'static str| move |_: &str, _: bool| Some(mode.to_string());
        assert_eq!(
            check_with(Some("abc"), Some(SYSTEM), &reports("process")),
            HandoffCheck::Verified("hekma.service: KillMode=process".into())
        );
        assert_eq!(
            check_with(Some("abc"), Some(SYSTEM), &reports("none")).as_str(),
            "verified"
        );
        let unsafe_ = check_with(Some("abc"), Some(SYSTEM), &reports("control-group"));
        assert_eq!(unsafe_.as_str(), "unsafe");
        assert!(
            unsafe_.note().contains("KillMode=process"),
            "{}",
            unsafe_.note()
        );
        assert_eq!(
            check_with(Some("abc"), Some(SYSTEM), &reports("mixed")).detail(),
            "hekma.service: KillMode=mixed"
        );
        let odd = check_with(Some("abc"), Some(SYSTEM), &reports("weird"));
        assert!(odd.detail().contains("KillMode=weird"));
        let none = |_: &str, _: bool| None;
        assert!(check_with(Some("abc"), Some(SYSTEM), &none)
            .detail()
            .contains("could not be read"));
        assert!(check_with(None, Some(SYSTEM), &none)
            .note()
            .contains("not started by systemd"));
        assert!(check_with(Some(""), None, &none)
            .detail()
            .contains("not started"));
        assert!(check_with(Some("abc"), Some("0::/\n"), &none)
            .detail()
            .contains("could not be named"));
        assert!(check_with(Some("abc"), Some(SYSTEM), &reports("process"))
            .note()
            .contains("keeps agents"));
    }

    #[test]
    fn the_unit_is_read_from_the_cgroup_path_and_a_user_unit_asks_the_user_manager() {
        assert_eq!(systemd_unit(SYSTEM), Some(("hekma.service".into(), false)));
        assert_eq!(systemd_unit(USER), Some(("hekma.service".into(), true)));
        assert_eq!(
            systemd_unit("12:name=systemd:/system.slice/x.scope\n"),
            Some(("x.scope".into(), false))
        );
        assert_eq!(
            systemd_unit("0::/init.scope/sub\n"),
            Some(("init.scope".into(), false))
        );
        assert_eq!(systemd_unit("garbage"), None);
        let asked = std::sync::Mutex::new(None);
        let query = |unit: &str, user: bool| {
            *asked.lock().unwrap() = Some((unit.to_string(), user));
            Some("process".to_string())
        };
        check_with(Some("id"), Some(USER), &query);
        assert_eq!(*asked.lock().unwrap(), Some(("hekma.service".into(), true)));
        // The real query answers or declines (no systemctl, no such unit),
        // for a system and a user unit alike — never a fabricated mode.
        for user in [false, true] {
            let mode = query_kill_mode("hekma-no-such-unit.service", user);
            assert!(mode.is_none_or(|mode| !mode.is_empty()));
        }
        // The real check on this machine answers something, never panics.
        assert!(["verified", "unsafe", "unverified"].contains(&check().as_str()));
    }
}
