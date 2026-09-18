use std::env;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallChannel {
    Cargo,
    Homebrew,
    Manual,
}

pub(crate) trait CommandProbe {
    fn command_exists(&self, command: &str) -> bool;
    fn command_succeeds(&self, command: &str, args: &[&str]) -> bool;
}

pub(crate) fn detect_install_channel<P>(exe_path: &Path, command_probe: &P) -> InstallChannel
where
    P: CommandProbe,
{
    let exe_path = exe_path
        .canonicalize()
        .unwrap_or_else(|_| exe_path.to_path_buf());
    detect_install_channel_with_env(
        &exe_path,
        env::var_os("CARGO_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
        command_probe,
    )
}

pub(crate) fn detect_install_channel_with_env<P>(
    exe_path: &Path,
    cargo_home: Option<PathBuf>,
    home: Option<PathBuf>,
    command_probe: &P,
) -> InstallChannel
where
    P: CommandProbe,
{
    let components = exe_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>();

    if components
        .windows(2)
        .any(|window| window[0] == "Cellar" && window[1] == "hemaka")
    {
        return InstallChannel::Homebrew;
    }

    if command_probe.command_exists("brew")
        && (command_probe.command_succeeds("brew", &["list", "--formula", "hemaka"])
            || command_probe.command_succeeds("brew", &["list", "--formula", "ktesio/tap/hemaka"]))
    {
        return InstallChannel::Homebrew;
    }

    if cargo_home
        .as_deref()
        .map(|cargo_home| exe_path.starts_with(cargo_home.join("bin")))
        .unwrap_or(false)
    {
        return InstallChannel::Cargo;
    }

    if home
        .as_deref()
        .map(|home| exe_path.starts_with(home.join(".cargo").join("bin")))
        .unwrap_or(false)
    {
        return InstallChannel::Cargo;
    }

    InstallChannel::Manual
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct FakeProbe {
        commands: HashSet<String>,
        successes: RefCell<HashSet<String>>,
    }

    impl FakeProbe {
        fn with_command(mut self, command: &str) -> Self {
            self.commands.insert(command.to_string());
            self
        }

        fn with_success(self, command: &str, args: &[&str]) -> Self {
            self.successes
                .borrow_mut()
                .insert(command_key(command, args));
            self
        }
    }

    impl CommandProbe for FakeProbe {
        fn command_exists(&self, command: &str) -> bool {
            self.commands.contains(command)
        }

        fn command_succeeds(&self, command: &str, args: &[&str]) -> bool {
            self.successes
                .borrow()
                .contains(&command_key(command, args))
        }
    }

    fn command_key(command: &str, args: &[&str]) -> String {
        format!("{command} {}", args.join(" "))
    }

    fn restore_env(name: &str, value: Option<OsString>) {
        if let Some(value) = value {
            env::set_var(name, value);
        } else {
            env::remove_var(name);
        }
    }

    #[test]
    fn test_detect_install_channel_prefers_homebrew_cellar_path() {
        let channel = detect_install_channel_with_env(
            Path::new("/opt/homebrew/Cellar/hemaka/0.3.1/bin/hemaka"),
            None,
            None,
            &FakeProbe::default(),
        );

        assert_eq!(channel, InstallChannel::Homebrew);
    }

    #[test]
    fn test_detect_install_channel_uses_homebrew_formula_probe() {
        let probe = FakeProbe::default()
            .with_command("brew")
            .with_success("brew", &["list", "--formula", "ktesio/tap/hemaka"]);
        let channel =
            detect_install_channel_with_env(Path::new("/usr/local/bin/hemaka"), None, None, &probe);

        assert_eq!(channel, InstallChannel::Homebrew);
    }

    #[test]
    fn test_detect_install_channel_wrapper_falls_back_when_path_missing() {
        let channel = detect_install_channel(
            Path::new("/definitely/not/a/real/hemaka/bin/hemaka"),
            &FakeProbe::default(),
        );

        assert_eq!(channel, InstallChannel::Manual);
    }

    #[test]
    fn test_detect_install_channel_wrapper_uses_cargo_home_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let original_cargo_home = env::var_os("CARGO_HOME");
        let dir = tempfile::TempDir::new().unwrap();
        let cargo_home = dir.path().join("cargo-home");
        let exe = cargo_home.join("bin").join("hemaka");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"hemaka").unwrap();

        env::set_var("CARGO_HOME", cargo_home.canonicalize().unwrap());
        let channel = detect_install_channel(&exe, &FakeProbe::default());

        restore_env("CARGO_HOME", original_cargo_home);
        assert_eq!(channel, InstallChannel::Cargo);
    }

    #[test]
    fn test_detect_install_channel_uses_cargo_home_and_home_default() {
        let custom = detect_install_channel_with_env(
            Path::new("/custom/cargo/bin/hemaka"),
            Some(PathBuf::from("/custom/cargo")),
            None,
            &FakeProbe::default(),
        );
        let default_home = detect_install_channel_with_env(
            Path::new("/Users/alice/.cargo/bin/hemaka"),
            None,
            Some(PathBuf::from("/Users/alice")),
            &FakeProbe::default(),
        );

        assert_eq!(custom, InstallChannel::Cargo);
        assert_eq!(default_home, InstallChannel::Cargo);
    }

    #[test]
    fn test_detect_install_channel_falls_back_to_manual() {
        let channel = detect_install_channel_with_env(
            Path::new("/usr/local/bin/hemaka"),
            None,
            None,
            &FakeProbe::default(),
        );

        assert_eq!(channel, InstallChannel::Manual);
    }
}
