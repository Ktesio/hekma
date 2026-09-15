use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::SelfUpdateFailed;
use crate::install_channel::{detect_install_channel, CommandProbe, InstallChannel};
use crate::ui;

const TAP: &str = "ktesio/tap/ktesio";
const CRATE: &str = "ktesio";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Ktesio/ktesio/releases/latest";
const RELEASE_BASE_URL: &str = "https://github.com/Ktesio/ktesio/releases/download";

#[cfg(not(tarpaulin_include))]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe().map_err(|error| SelfUpdateFailed {
        message: format!("Could not locate current kt executable: {error}"),
    })?;
    let runner = SystemCommandRunner;
    let release_client = UreqReleaseClient::new();
    let installer = FileBinaryInstaller;
    let platform = Platform::current();

    run_with_dependencies(
        &exe_path,
        env!("CARGO_PKG_VERSION"),
        &platform,
        &runner,
        &release_client,
        &installer,
    )?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Platform {
    os: String,
    arch: String,
}

impl Platform {
    fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReleaseTarget {
    triple: &'static str,
    extension: &'static str,
    binary_name: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
enum SelfUpdateOutcome {
    AlreadyCurrent,
    Updated(InstallChannel),
}

trait CommandRunner: CommandProbe {
    fn run_command(&self, command: &str, args: &[&str]) -> Result<(), String>;
}

trait ReleaseClient {
    fn latest_release_tag(&self) -> Result<String, String>;
    fn download(&self, url: &str) -> Result<Vec<u8>, String>;
}

trait BinaryInstaller {
    fn replace_current_exe(&self, current_exe: &Path, binary: &[u8]) -> Result<(), String>;
}

fn run_with_dependencies<R, C, B>(
    current_exe: &Path,
    current_version: &str,
    platform: &Platform,
    runner: &R,
    release_client: &C,
    installer: &B,
) -> Result<SelfUpdateOutcome, SelfUpdateFailed>
where
    R: CommandRunner,
    C: ReleaseClient,
    B: BinaryInstaller,
{
    let channel = detect_install_channel(current_exe, runner);
    run_with_channel(
        channel,
        current_exe,
        current_version,
        platform,
        runner,
        release_client,
        installer,
    )
}

fn run_with_channel<R, C, B>(
    channel: InstallChannel,
    current_exe: &Path,
    current_version: &str,
    platform: &Platform,
    runner: &R,
    release_client: &C,
    installer: &B,
) -> Result<SelfUpdateOutcome, SelfUpdateFailed>
where
    R: CommandRunner,
    C: ReleaseClient,
    B: BinaryInstaller,
{
    match channel {
        InstallChannel::Homebrew => {
            runner
                .run_command("brew", &["upgrade", TAP])
                .map_err(self_update_error)?;
            ui::success("Updated Ktesio with Homebrew.");
            Ok(SelfUpdateOutcome::Updated(channel))
        }
        InstallChannel::Cargo => {
            runner
                .run_command("cargo", &["install", CRATE, "--force"])
                .map_err(self_update_error)?;
            ui::success("Updated Ktesio with Cargo.");
            Ok(SelfUpdateOutcome::Updated(channel))
        }
        InstallChannel::Manual => update_manual_binary(
            current_exe,
            current_version,
            platform,
            release_client,
            installer,
        ),
    }
}

fn update_manual_binary<C, B>(
    current_exe: &Path,
    current_version: &str,
    platform: &Platform,
    release_client: &C,
    installer: &B,
) -> Result<SelfUpdateOutcome, SelfUpdateFailed>
where
    C: ReleaseClient,
    B: BinaryInstaller,
{
    let latest_tag = release_client
        .latest_release_tag()
        .map_err(self_update_error)?;
    if !is_newer_version(current_version, &latest_tag) {
        ui::success(format!(
            "Ktesio is already up to date ({}).",
            display_version(&latest_tag)
        ));
        return Ok(SelfUpdateOutcome::AlreadyCurrent);
    }

    let target = release_target(platform)?;
    let asset = format!("ktesio-{latest_tag}-{}.{}", target.triple, target.extension);
    let asset_url = format!("{RELEASE_BASE_URL}/{latest_tag}/{asset}");
    let checksum_url = format!("{asset_url}.sha256");

    ui::info(format!(
        "Downloading Ktesio {latest_tag} for {}.",
        target.triple
    ));
    let archive = release_client
        .download(&asset_url)
        .map_err(self_update_error)?;
    let checksum = release_client
        .download(&checksum_url)
        .map_err(self_update_error)?;
    verify_checksum(&archive, &checksum, &asset)?;

    let binary = extract_binary(&archive, &target)?;
    installer
        .replace_current_exe(current_exe, &binary)
        .map_err(self_update_error)?;
    ui::success(format!(
        "Updated Ktesio to {}.",
        display_version(&latest_tag)
    ));
    Ok(SelfUpdateOutcome::Updated(InstallChannel::Manual))
}

fn release_target(platform: &Platform) -> Result<ReleaseTarget, SelfUpdateFailed> {
    match (platform.os.as_str(), platform.arch.as_str()) {
        ("macos", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-apple-darwin",
            extension: "tar.gz",
            binary_name: "kt",
        }),
        ("macos", "aarch64") => Ok(ReleaseTarget {
            triple: "aarch64-apple-darwin",
            extension: "tar.gz",
            binary_name: "kt",
        }),
        ("linux", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-unknown-linux-gnu",
            extension: "tar.gz",
            binary_name: "kt",
        }),
        ("windows", "x86_64") => Ok(ReleaseTarget {
            triple: "x86_64-pc-windows-msvc",
            extension: "zip",
            binary_name: "kt.exe",
        }),
        _ => Err(SelfUpdateFailed {
            message: format!(
                "No prebuilt Ktesio binary is available for {}/{}. Install Rust and run: cargo install ktesio --force",
                platform.os, platform.arch
            ),
        }),
    }
}

fn verify_checksum(
    archive: &[u8],
    checksum_file: &[u8],
    asset: &str,
) -> Result<(), SelfUpdateFailed> {
    let expected = std::str::from_utf8(checksum_file)
        .ok()
        .and_then(|text| text.split_whitespace().next())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit()))
        .ok_or_else(|| SelfUpdateFailed {
            message: format!("Checksum file for {asset} did not contain a valid SHA-256 value."),
        })?;
    let actual = sha256_hex(archive);

    if expected != actual {
        return Err(SelfUpdateFailed {
            message: format!("Checksum verification failed for {asset}."),
        });
    }

    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn extract_binary(archive: &[u8], target: &ReleaseTarget) -> Result<Vec<u8>, SelfUpdateFailed> {
    match target.extension {
        "tar.gz" => extract_from_tar_gz(archive, target.binary_name),
        "zip" => extract_from_zip(archive, target.binary_name),
        extension => Err(SelfUpdateFailed {
            message: format!("Unsupported release archive extension: {extension}"),
        }),
    }
}

fn extract_from_tar_gz(archive: &[u8], binary_name: &str) -> Result<Vec<u8>, SelfUpdateFailed> {
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|error| SelfUpdateFailed {
        message: format!("Could not read release archive: {error}"),
    })?;

    for entry in entries {
        let mut entry = entry.map_err(|error| SelfUpdateFailed {
            message: format!("Could not read release archive entry: {error}"),
        })?;
        let path = entry.path().map_err(|error| SelfUpdateFailed {
            message: format!("Could not read release archive entry path: {error}"),
        })?;
        if path.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
            let mut binary = Vec::new();
            entry
                .read_to_end(&mut binary)
                .map_err(|error| SelfUpdateFailed {
                    message: format!(
                        "Could not extract {binary_name} from release archive: {error}"
                    ),
                })?;
            return Ok(binary);
        }
    }

    Err(SelfUpdateFailed {
        message: format!("Release archive did not contain {binary_name}."),
    })
}

fn extract_from_zip(archive: &[u8], binary_name: &str) -> Result<Vec<u8>, SelfUpdateFailed> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(archive)).map_err(|error| SelfUpdateFailed {
            message: format!("Could not read release archive: {error}"),
        })?;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|error| SelfUpdateFailed {
            message: format!("Could not read release archive entry: {error}"),
        })?;
        let path = Path::new(file.name());
        if path.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
            let mut binary = Vec::new();
            file.read_to_end(&mut binary)
                .map_err(|error| SelfUpdateFailed {
                    message: format!(
                        "Could not extract {binary_name} from release archive: {error}"
                    ),
                })?;
            return Ok(binary);
        }
    }

    Err(SelfUpdateFailed {
        message: format!("Release archive did not contain {binary_name}."),
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

fn self_update_error(message: impl Into<String>) -> SelfUpdateFailed {
    SelfUpdateFailed {
        message: message.into(),
    }
}

#[cfg(not(tarpaulin_include))]
struct SystemCommandRunner;

#[cfg(not(tarpaulin_include))]
impl CommandProbe for SystemCommandRunner {
    fn command_exists(&self, command: &str) -> bool {
        command_on_path(command)
    }

    fn command_succeeds(&self, command: &str, args: &[&str]) -> bool {
        Command::new(command)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

#[cfg(not(tarpaulin_include))]
impl CommandRunner for SystemCommandRunner {
    fn run_command(&self, command: &str, args: &[&str]) -> Result<(), String> {
        let status = Command::new(command)
            .args(args)
            .status()
            .map_err(|error| format!("Failed to run {command}: {error}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("{command} exited with status {status}"))
        }
    }
}

#[cfg(not(tarpaulin_include))]
fn command_on_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        candidate_command_names(command)
            .iter()
            .any(|candidate| dir.join(candidate).is_file())
    })
}

#[cfg(not(tarpaulin_include))]
fn candidate_command_names(command: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let mut names = vec![command.to_string()];
        if Path::new(command).extension().is_none() {
            let path_ext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.BAT;.CMD".to_string());
            names.extend(
                path_ext
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| format!("{command}{ext}")),
            );
        }
        names
    }

    #[cfg(not(windows))]
    {
        vec![command.to_string()]
    }
}

#[cfg(not(tarpaulin_include))]
struct UreqReleaseClient {
    agent: ureq::Agent,
}

#[cfg(not(tarpaulin_include))]
impl UreqReleaseClient {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl ReleaseClient for UreqReleaseClient {
    fn latest_release_tag(&self) -> Result<String, String> {
        let mut response = self
            .agent
            .get(LATEST_RELEASE_URL)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", concat!("ktesio/", env!("CARGO_PKG_VERSION")))
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

    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| error.to_string())?;
        if response.status() != 200 {
            return Err(format!("{url} returned HTTP {}", response.status()));
        }

        response
            .body_mut()
            .read_to_vec()
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct GitHubLatestRelease {
    tag_name: String,
}

#[cfg(not(tarpaulin_include))]
struct FileBinaryInstaller;

#[cfg(not(tarpaulin_include))]
impl BinaryInstaller for FileBinaryInstaller {
    fn replace_current_exe(&self, current_exe: &Path, binary: &[u8]) -> Result<(), String> {
        let install_dir = current_exe
            .parent()
            .ok_or_else(|| "Could not find current executable directory.".to_string())?;
        if !current_exe.is_file() {
            return Err(format!(
                "Refusing to replace missing kt executable at {}.",
                current_exe.display()
            ));
        }

        let file_name = current_exe
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Current executable path is not valid UTF-8.".to_string())?;
        let temp_path = install_dir.join(format!(
            ".{file_name}.self-update-{}.tmp",
            std::process::id()
        ));

        if let Err(error) = write_replacement(&temp_path, binary) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        if let Err(error) = fs::rename(&temp_path, current_exe) {
            let _ = fs::remove_file(&temp_path);
            return Err(format!(
                "Could not replace {}: {error}",
                current_exe.display()
            ));
        }

        Ok(())
    }
}

#[cfg(not(tarpaulin_include))]
fn write_replacement(temp_path: &Path, binary: &[u8]) -> Result<(), String> {
    fs::write(temp_path, binary)
        .map_err(|error| format!("Could not write {}: {error}", temp_path.display()))?;
    make_executable(temp_path)
}

#[cfg(all(unix, not(tarpaulin_include)))]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("Could not read permissions for {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("Could not set permissions for {}: {error}", path.display()))
}

#[cfg(all(not(unix), not(tarpaulin_include)))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::io::Write;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeRunner {
        commands: HashSet<String>,
        successes: RefCell<HashSet<String>>,
        failures: RefCell<HashMap<String, String>>,
        runs: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn with_failure(self, command: &str, args: &[&str], message: &str) -> Self {
            self.failures
                .borrow_mut()
                .insert(command_key(command, args), message.to_string());
            self
        }
    }

    impl CommandProbe for FakeRunner {
        fn command_exists(&self, command: &str) -> bool {
            self.commands.contains(command)
        }

        fn command_succeeds(&self, command: &str, args: &[&str]) -> bool {
            self.successes
                .borrow()
                .contains(&command_key(command, args))
        }
    }

    impl CommandRunner for FakeRunner {
        fn run_command(&self, command: &str, args: &[&str]) -> Result<(), String> {
            let key = command_key(command, args);
            self.runs.borrow_mut().push(key.clone());
            self.failures
                .borrow()
                .get(&key)
                .cloned()
                .map(Err)
                .unwrap_or(Ok(()))
        }
    }

    struct FakeReleaseClient {
        latest_tag: Result<String, String>,
        downloads: RefCell<HashMap<String, Vec<u8>>>,
        urls: RefCell<Vec<String>>,
    }

    impl FakeReleaseClient {
        fn new(tag: &str) -> Self {
            Self {
                latest_tag: Ok(tag.to_string()),
                downloads: RefCell::new(HashMap::new()),
                urls: RefCell::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                latest_tag: Err(message.to_string()),
                downloads: RefCell::new(HashMap::new()),
                urls: RefCell::new(Vec::new()),
            }
        }

        fn with_download(self, url: &str, bytes: Vec<u8>) -> Self {
            self.downloads.borrow_mut().insert(url.to_string(), bytes);
            self
        }
    }

    impl ReleaseClient for FakeReleaseClient {
        fn latest_release_tag(&self) -> Result<String, String> {
            self.latest_tag.clone()
        }

        fn download(&self, url: &str) -> Result<Vec<u8>, String> {
            self.urls.borrow_mut().push(url.to_string());
            self.downloads
                .borrow()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("missing fake download for {url}"))
        }
    }

    #[derive(Default)]
    struct FakeBinaryInstaller {
        replacements: RefCell<Vec<(PathBuf, Vec<u8>)>>,
        failure: Option<String>,
    }

    impl FakeBinaryInstaller {
        fn failing(message: &str) -> Self {
            Self {
                replacements: RefCell::new(Vec::new()),
                failure: Some(message.to_string()),
            }
        }
    }

    impl BinaryInstaller for FakeBinaryInstaller {
        fn replace_current_exe(&self, current_exe: &Path, binary: &[u8]) -> Result<(), String> {
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            self.replacements
                .borrow_mut()
                .push((current_exe.to_path_buf(), binary.to_vec()));
            Ok(())
        }
    }

    fn command_key(command: &str, args: &[&str]) -> String {
        format!("{command} {}", args.join(" "))
    }

    fn platform(os: &str, arch: &str) -> Platform {
        Platform {
            os: os.to_string(),
            arch: arch.to_string(),
        }
    }

    fn tar_gz_with_binary(name: &str, bytes: &[u8]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, name, Cursor::new(bytes))
            .unwrap();
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn zip_with_binary(name: &str, bytes: &[u8]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(bytes).unwrap();
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn test_platform_current_uses_rust_target_constants() {
        assert_eq!(
            Platform::current(),
            Platform {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
            }
        );
    }

    #[test]
    fn test_self_update_homebrew_runs_brew_upgrade() {
        let runner = FakeRunner::default();
        let release = FakeReleaseClient::new("v9.9.9");
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_dependencies(
            Path::new("/opt/homebrew/Cellar/ktesio/0.3.1/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &runner,
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(
            outcome,
            SelfUpdateOutcome::Updated(InstallChannel::Homebrew)
        );
        assert_eq!(
            runner.runs.borrow().as_slice(),
            &[command_key("brew", &["upgrade", TAP])]
        );
        assert!(installer.replacements.borrow().is_empty());
    }

    #[test]
    fn test_self_update_cargo_runs_cargo_install_force() {
        let runner = FakeRunner::default();
        let release = FakeReleaseClient::new("v9.9.9");
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_channel(
            InstallChannel::Cargo,
            Path::new("/Users/alice/.cargo/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &runner,
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(outcome, SelfUpdateOutcome::Updated(InstallChannel::Cargo));
        assert_eq!(
            runner.runs.borrow().as_slice(),
            &[command_key("cargo", &["install", CRATE, "--force"])]
        );
    }

    #[test]
    fn test_self_update_cargo_failure_is_returned() {
        let runner = FakeRunner::default().with_failure(
            "cargo",
            &["install", CRATE, "--force"],
            "cargo failed",
        );
        let error = run_with_channel(
            InstallChannel::Cargo,
            Path::new("/Users/alice/.cargo/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &runner,
            &FakeReleaseClient::new("v9.9.9"),
            &FakeBinaryInstaller::default(),
        )
        .unwrap_err();

        assert_eq!(error.message, "cargo failed");
    }

    #[test]
    fn test_self_update_manual_downloads_verifies_and_replaces_binary() {
        let binary = b"new kt binary";
        let archive = tar_gz_with_binary("kt", binary);
        let checksum = format!("{}  archive.tar.gz\n", sha256_hex(&archive));
        let asset_url =
            format!("{RELEASE_BASE_URL}/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz");
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0")
            .with_download(&asset_url, archive)
            .with_download(&checksum_url, checksum.into_bytes());
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(outcome, SelfUpdateOutcome::Updated(InstallChannel::Manual));
        assert_eq!(release.urls.borrow().as_slice(), &[asset_url, checksum_url]);
        assert_eq!(
            installer.replacements.borrow().as_slice(),
            &[(PathBuf::from("/usr/local/bin/kt"), binary.to_vec())]
        );
    }

    #[test]
    fn test_self_update_manual_windows_downloads_zip_asset() {
        let binary = b"windows kt binary";
        let archive = zip_with_binary("kt.exe", binary);
        let checksum = format!("{}  archive.zip\n", sha256_hex(&archive));
        let asset_url =
            format!("{RELEASE_BASE_URL}/v0.4.0/ktesio-v0.4.0-x86_64-pc-windows-msvc.zip");
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0")
            .with_download(&asset_url, archive)
            .with_download(&checksum_url, checksum.into_bytes());
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_channel(
            InstallChannel::Manual,
            Path::new("C:/Users/Alice/bin/kt.exe"),
            "0.3.1",
            &platform("windows", "x86_64"),
            &FakeRunner::default(),
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(outcome, SelfUpdateOutcome::Updated(InstallChannel::Manual));
        assert_eq!(release.urls.borrow().as_slice(), &[asset_url, checksum_url]);
        assert_eq!(
            installer.replacements.borrow().as_slice(),
            &[(PathBuf::from("C:/Users/Alice/bin/kt.exe"), binary.to_vec())]
        );
    }

    #[test]
    fn test_self_update_manual_detection_wrapper_updates_binary() {
        let binary = b"new kt binary";
        let archive = tar_gz_with_binary("kt", binary);
        let checksum = format!("{}  archive.tar.gz\n", sha256_hex(&archive));
        let asset_url =
            format!("{RELEASE_BASE_URL}/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz");
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0")
            .with_download(&asset_url, archive)
            .with_download(&checksum_url, checksum.into_bytes());
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_dependencies(
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(outcome, SelfUpdateOutcome::Updated(InstallChannel::Manual));
        assert_eq!(
            installer.replacements.borrow().as_slice(),
            &[(PathBuf::from("/usr/local/bin/kt"), binary.to_vec())]
        );
    }

    #[test]
    fn test_self_update_manual_latest_release_failure_is_returned() {
        let error = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &FakeReleaseClient::failing("offline"),
            &FakeBinaryInstaller::default(),
        )
        .unwrap_err();

        assert_eq!(error.message, "offline");
    }

    #[test]
    fn test_self_update_manual_download_failure_is_returned() {
        let release = FakeReleaseClient::new("v0.4.0");
        let error = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &FakeBinaryInstaller::default(),
        )
        .unwrap_err();

        assert!(error.message.contains("missing fake download"));
    }

    #[test]
    fn test_self_update_manual_replace_failure_is_returned() {
        let archive = tar_gz_with_binary("kt", b"new kt binary");
        let checksum = format!("{}  archive.tar.gz\n", sha256_hex(&archive));
        let asset_url =
            format!("{RELEASE_BASE_URL}/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz");
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0")
            .with_download(&asset_url, archive)
            .with_download(&checksum_url, checksum.into_bytes());

        let error = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &FakeBinaryInstaller::failing("replace failed"),
        )
        .unwrap_err();

        assert_eq!(error.message, "replace failed");
    }

    #[test]
    fn test_self_update_manual_checksum_mismatch_fails() {
        let archive = tar_gz_with_binary("kt", b"new kt binary");
        let asset_url =
            format!("{RELEASE_BASE_URL}/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz");
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0")
            .with_download(&asset_url, archive)
            .with_download(
                &checksum_url,
                format!("{}  archive.tar.gz\n", "0".repeat(64)).into_bytes(),
            );

        let error = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &FakeBinaryInstaller::default(),
        )
        .unwrap_err();

        assert!(error.message.contains("Checksum verification failed"));
    }

    #[test]
    fn test_verify_checksum_rejects_invalid_checksum_file() {
        let error = verify_checksum(b"archive", b"not-a-sha", "ktesio.tar.gz").unwrap_err();

        assert!(error.message.contains("valid SHA-256"));
    }

    #[test]
    fn test_self_update_manual_unsupported_target_fails_with_cargo_hint() {
        let error = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "aarch64"),
            &FakeRunner::default(),
            &FakeReleaseClient::new("v0.4.0"),
            &FakeBinaryInstaller::default(),
        )
        .unwrap_err();

        assert!(error.message.contains("No prebuilt Ktesio binary"));
        assert!(error.message.contains("cargo install ktesio --force"));
    }

    #[test]
    fn test_self_update_manual_up_to_date_skips_download_and_replace() {
        let release = FakeReleaseClient::new("v0.3.1");
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(outcome, SelfUpdateOutcome::AlreadyCurrent);
        assert!(release.urls.borrow().is_empty());
        assert!(installer.replacements.borrow().is_empty());
    }

    #[test]
    fn test_self_update_manual_newer_prerelease_updates_release() {
        let binary = b"release candidate kt binary";
        let archive = tar_gz_with_binary("kt", binary);
        let checksum = format!("{}  archive.tar.gz\n", sha256_hex(&archive));
        let asset_url = format!(
            "{RELEASE_BASE_URL}/v0.4.0-rc.1/ktesio-v0.4.0-rc.1-x86_64-unknown-linux-gnu.tar.gz"
        );
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0-rc.1")
            .with_download(&asset_url, archive)
            .with_download(&checksum_url, checksum.into_bytes());
        let installer = FakeBinaryInstaller::default();

        let outcome = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &installer,
        )
        .unwrap();

        assert_eq!(outcome, SelfUpdateOutcome::Updated(InstallChannel::Manual));
        assert_eq!(release.urls.borrow().as_slice(), &[asset_url, checksum_url]);
        assert_eq!(
            installer.replacements.borrow().as_slice(),
            &[(PathBuf::from("/usr/local/bin/kt"), binary.to_vec())]
        );
    }

    #[cfg(not(tarpaulin_include))]
    #[test]
    fn test_file_binary_installer_replaces_current_exe() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("kt");
        fs::write(&exe, b"old kt binary").unwrap();

        FileBinaryInstaller
            .replace_current_exe(&exe, b"new kt binary")
            .unwrap();

        assert_eq!(fs::read(&exe).unwrap(), b"new kt binary");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[cfg(not(tarpaulin_include))]
    #[test]
    fn test_file_binary_installer_rejects_missing_exe() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("missing-kt");

        let error = FileBinaryInstaller
            .replace_current_exe(&exe, b"new kt binary")
            .unwrap_err();

        assert!(error.contains("Refusing to replace missing kt executable"));
    }

    #[test]
    fn test_version_comparison_rejects_invalid_versions() {
        assert!(!is_newer_version("not-a-version", "v0.4.0"));
        assert!(!is_newer_version("0.3.1", "not-a-version"));
    }

    #[test]
    fn test_display_version_trims_tags_and_whitespace() {
        assert_eq!(display_version(" v0.4.0 \n"), "0.4.0");
    }

    #[test]
    fn test_release_target_matrix() {
        assert_eq!(
            release_target(&platform("macos", "x86_64")).unwrap().triple,
            "x86_64-apple-darwin"
        );
        assert_eq!(
            release_target(&platform("macos", "aarch64"))
                .unwrap()
                .triple,
            "aarch64-apple-darwin"
        );
        assert_eq!(
            release_target(&platform("windows", "x86_64"))
                .unwrap()
                .triple,
            "x86_64-pc-windows-msvc"
        );
    }

    #[test]
    fn test_extract_binary_reports_missing_binary() {
        let archive = tar_gz_with_binary("not-kt", b"nope");
        let error = extract_binary(
            &archive,
            &ReleaseTarget {
                triple: "x86_64-unknown-linux-gnu",
                extension: "tar.gz",
                binary_name: "kt",
            },
        )
        .unwrap_err();

        assert!(error.message.contains("did not contain kt"));
    }

    #[test]
    fn test_extract_zip_reports_missing_binary() {
        let archive = zip_with_binary("not-kt.exe", b"nope");
        let error = extract_binary(
            &archive,
            &ReleaseTarget {
                triple: "x86_64-pc-windows-msvc",
                extension: "zip",
                binary_name: "kt.exe",
            },
        )
        .unwrap_err();

        assert!(error.message.contains("did not contain kt.exe"));
    }

    #[test]
    fn test_extract_zip_reports_invalid_archive() {
        let error = extract_binary(
            b"not a zip archive",
            &ReleaseTarget {
                triple: "x86_64-pc-windows-msvc",
                extension: "zip",
                binary_name: "kt.exe",
            },
        )
        .unwrap_err();

        assert!(error.message.contains("Could not read release archive"));
    }

    #[test]
    fn test_extract_binary_rejects_unknown_archive_extension() {
        let error = extract_binary(
            b"archive",
            &ReleaseTarget {
                triple: "x86_64-example",
                extension: "tar.xz",
                binary_name: "kt",
            },
        )
        .unwrap_err();

        assert!(error
            .message
            .contains("Unsupported release archive extension"));
    }

    #[test]
    fn test_extract_tar_gz_reports_invalid_archive() {
        let error = extract_binary(
            b"not a gzip archive",
            &ReleaseTarget {
                triple: "x86_64-unknown-linux-gnu",
                extension: "tar.gz",
                binary_name: "kt",
            },
        )
        .unwrap_err();

        assert!(error.message.contains("Could not read release archive"));
    }

    /// A payload that does not compress away, so truncating or corrupting the
    /// archive really does damage the entry BODY rather than vanishing into the
    /// container's framing.
    fn incompressible_binary(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                // A cheap LCG — deterministic (no rand dep) but high-entropy
                // enough that deflate cannot shrink it to nothing.
                ((i as u64)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1)
                    >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn test_truncated_tar_gz_download_fails_instead_of_yielding_a_partial_binary() {
        // A download cut short mid-transfer still has a readable gzip/tar HEADER,
        // so the entry for `kt` is found and extraction begins — the failure only
        // surfaces while reading the entry BODY. The contract that matters is that
        // this is an ERROR, not a short read: returning the bytes received so far
        // would hand `replace_current_exe` a truncated executable and brick the
        // user's `kt`. (The checksum gate would also catch this, but only because
        // extraction refused to invent a body first — both layers must hold.)
        let target = ReleaseTarget {
            triple: "x86_64-unknown-linux-gnu",
            extension: "tar.gz",
            binary_name: "kt",
        };
        let full = tar_gz_with_binary("kt", &incompressible_binary(64 * 1024));
        let truncated = full[..full.len() / 4].to_vec();

        let error = extract_binary(&truncated, &target).unwrap_err();

        assert!(
            error.message.contains("Could not extract kt"),
            "{}",
            error.message
        );
        // Sanity: the SAME archive intact extracts fine, so the failure is the
        // truncation and not a broken fixture.
        assert_eq!(
            extract_binary(&full, &target).unwrap(),
            incompressible_binary(64 * 1024)
        );
    }

    #[test]
    fn test_corrupted_zip_entry_fails_the_integrity_check_on_extract() {
        // The Windows release asset is a zip, whose per-entry CRC-32 is the
        // container's own integrity check. A bit flipped in the compressed data
        // leaves the central directory (and therefore `by_index`) intact, so the
        // corruption can ONLY be caught while reading the entry out. Extraction
        // must fail rather than return silently-wrong bytes for `kt.exe`.
        let target = ReleaseTarget {
            triple: "x86_64-pc-windows-msvc",
            extension: "zip",
            binary_name: "kt.exe",
        };
        let payload = incompressible_binary(32 * 1024);
        let mut archive = zip_with_binary("kt.exe", &payload);
        // Flip a bit well inside the local file DATA (past the 30-byte local
        // header + the file name), leaving the trailing central directory whole.
        let corrupt_at = archive.len() / 2;
        archive[corrupt_at] ^= 0xFF;

        let error = extract_binary(&archive, &target).unwrap_err();

        assert!(
            error.message.contains("Could not extract kt.exe"),
            "{}",
            error.message
        );
        assert_eq!(
            extract_binary(&zip_with_binary("kt.exe", &payload), &target).unwrap(),
            payload
        );
    }

    #[test]
    fn test_a_failed_checksum_never_reaches_the_installer() {
        // The ordering IS the security property: verify_checksum runs BEFORE
        // extract_binary and before replace_current_exe, so a tampered or
        // corrupted download can never touch the installed executable. Asserting
        // only the error message (as the mismatch test does) would still pass if
        // someone moved the verification AFTER the install — this pins the
        // installer as untouched.
        let archive = tar_gz_with_binary("kt", b"tampered kt binary");
        let asset_url =
            format!("{RELEASE_BASE_URL}/v0.4.0/ktesio-v0.4.0-x86_64-unknown-linux-gnu.tar.gz");
        let checksum_url = format!("{asset_url}.sha256");
        let release = FakeReleaseClient::new("v0.4.0")
            .with_download(&asset_url, archive)
            .with_download(
                &checksum_url,
                format!("{}  archive.tar.gz\n", "a".repeat(64)).into_bytes(),
            );
        let installer = FakeBinaryInstaller::default();

        let error = run_with_channel(
            InstallChannel::Manual,
            Path::new("/usr/local/bin/kt"),
            "0.3.1",
            &platform("linux", "x86_64"),
            &FakeRunner::default(),
            &release,
            &installer,
        )
        .unwrap_err();

        assert!(error.message.contains("Checksum verification failed"));
        assert!(
            installer.replacements.borrow().is_empty(),
            "a checksum failure must leave the installed binary untouched"
        );
    }

    #[test]
    fn test_checksum_accepts_an_uppercase_digest_from_the_sha256_file() {
        // Release tooling varies: `sha256sum` emits lowercase, several Windows and
        // CI helpers emit uppercase, and the file may carry a trailing
        // `  <filename>` field. Verification is case-insensitive on purpose — a
        // regression here would reject a perfectly good release as tampered, which
        // looks like a security incident rather than a formatting nit.
        let archive = b"release archive bytes";
        let digest = sha256_hex(archive);
        let uppercase = format!("{}  ktesio.tar.gz\n", digest.to_ascii_uppercase());

        verify_checksum(archive, uppercase.as_bytes(), "ktesio.tar.gz").unwrap();

        // A digest of the right SHAPE but the wrong value is still rejected, so the
        // case-insensitivity above is not masking a "anything 64 hex chars" hole.
        let wrong = format!("{}  ktesio.tar.gz\n", "A".repeat(64));
        let error = verify_checksum(archive, wrong.as_bytes(), "ktesio.tar.gz").unwrap_err();
        assert!(error.message.contains("Checksum verification failed"));
    }
}
