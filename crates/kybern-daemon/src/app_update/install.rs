//! The daemon's side of updating the desktop app while the app is closed: how the app was installed, its
//! release feed, the minisign check and the swap. Each step mirrors the app's own updater
//! (`apps/gpuix/src/platform/update.ts`): the same feed and platform keys, the same public key, the same
//! file-name binding of the signature, and the same staging (beside the target, so the final rename is atomic).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use kybern_protocol::UpdateTrack;
use kybern_protocol::process::NoWindow;
use serde_json::Value;

use crate::self_update::{newer, on_track};

/// The app's bundle identifier; nothing else is ever replaced.
pub const BUNDLE_ID: &str = "dev.kybern.gpuix";
/// The package name of the deb and rpm, and the folder they install to under `/usr/lib`.
pub const PACKAGE: &str = "kybern-gpuix";
/// `apps/gpuix/src/platform/updater.json`: the minisign key every GPUIX release is signed with, and its feeds.
/// A test keeps the two in step.
pub const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDJBNjc0NEY5RTE0N0ZEMTkKUldRWi9VZmgrVVJuS2l2ME96TEc2THdBcGNDZkVjUUtVR01VKzdiNmhKY2drYUN4bG1rRlRrc2IK";
pub const STABLE_FEED: &str = "https://github.com/Tresnanda/kybern/releases/latest/download/latest.json";
pub const DEV_FEED: &str = "https://github.com/Tresnanda/kybern/releases/download/dev-feed/latest-dev.json";

pub const DEVELOPMENT: &str = "Development build; rebuild it instead.";
pub const NOT_INSTALLED: &str = "This copy of Kybern can't update itself. Update it the way it was installed.";

/// Folders a daemon-side Windows install leaves in the temp folder (the installer cannot be deleted while it runs).
const STAGING_PREFIX: &str = "kybern-app-update-";

// ---- how the app was installed ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Deb,
    Rpm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopInstall {
    /// `Kybern GPUIX.app`; this daemon is its `Contents/MacOS/kybernd`.
    MacApp { bundle: PathBuf },
    /// The per-user NSIS install folder: `Kybern GPUIX.exe`, `uninstall.exe` and this `kybernd.exe`.
    Windows { dir: PathBuf },
    /// An AppImage. This daemon runs from the copy the app staged outside the image (`bin`), never from its mount.
    AppImage { image: PathBuf, bin: PathBuf },
    /// A deb or rpm under `/usr/lib/kybern-gpuix`.
    Package(PackageKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    Install(DesktopInstall),
    /// Not an installed app; the words say why.
    NotInstalled(String),
}

/// The file system, for detection; tests pass their own.
pub struct Probe<'a> {
    pub exists: &'a dyn Fn(&Path) -> bool,
    pub read: &'a dyn Fn(&Path) -> Option<String>,
}

#[cfg_attr(test, allow(dead_code))]
fn real_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg_attr(test, allow(dead_code))]
fn real_read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

impl Probe<'static> {
    #[cfg_attr(test, allow(dead_code))]
    pub const REAL: Probe<'static> = Probe { exists: &real_exists, read: &real_read };
}

/// How the daemon at `exe` was installed, on `os` (`std::env::consts::OS`).
pub fn detect(exe: &Path, os: &str, probe: &Probe) -> Detected {
    if crate::login_service::is_development_build(exe) {
        return Detected::NotInstalled(DEVELOPMENT.into());
    }
    let Some(dir) = exe.parent() else { return Detected::NotInstalled(NOT_INSTALLED.into()) };
    let name = |path: &Path| path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
    match os {
        "macos" => {
            let contents = dir.parent();
            let bundle = contents.and_then(Path::parent);
            match (contents, bundle) {
                (Some(contents), Some(bundle))
                    if name(dir) == "MacOS"
                        && name(contents) == "Contents"
                        && bundle.extension().is_some_and(|extension| extension == "app")
                        && (probe.read)(&contents.join("Info.plist"))
                            .is_some_and(|plist| plist.contains(&format!("<string>{BUNDLE_ID}</string>"))) =>
                {
                    Detected::Install(DesktopInstall::MacApp { bundle: bundle.to_path_buf() })
                }
                _ => Detected::NotInstalled(NOT_INSTALLED.into()),
            }
        }
        "windows" => {
            // What the installer writes (`packaging/windows/installer.nsi`); a loose copy has no uninstaller beside it.
            if (probe.exists)(&dir.join("uninstall.exe")) && (probe.exists)(&dir.join("Kybern GPUIX.exe")) {
                Detected::Install(DesktopInstall::Windows { dir: dir.to_path_buf() })
            } else {
                Detected::NotInstalled(NOT_INSTALLED.into())
            }
        }
        "linux" => {
            // `scripts/package-linux.ts` records the format of each package in `resources/package`.
            match (probe.read)(&dir.join("resources").join("package")).as_deref().map(str::trim) {
                Some("deb") => return Detected::Install(DesktopInstall::Package(PackageKind::Deb)),
                Some("rpm") => return Detected::Install(DesktopInstall::Package(PackageKind::Rpm)),
                _ => {}
            }
            // The AppImage stages its daemon here and names the image it came from (`linuxPackage.ts`).
            match (probe.read)(&dir.join("appimage")).map(|text| PathBuf::from(text.trim())) {
                // `has_root`, not `is_absolute`: the same on Linux, and the tests run these paths everywhere.
                Some(image) if image.has_root() && (probe.exists)(&image) => {
                    Detected::Install(DesktopInstall::AppImage { image, bin: dir.to_path_buf() })
                }
                _ => Detected::NotInstalled(NOT_INSTALLED.into()),
            }
        }
        _ => Detected::NotInstalled(NOT_INSTALLED.into()),
    }
}

/// What a deb or rpm install says instead of updating: those need root, so the user installs the release.
pub fn package_message(kind: PackageKind, version: Option<&str>, arch: &str) -> String {
    let arch = if arch == "aarch64" { "aarch64" } else { "x86_64" };
    let (extension, tool) = match kind {
        PackageKind::Deb => ("deb", "apt"),
        PackageKind::Rpm => ("rpm", "dnf"),
    };
    match version {
        Some(version) => {
            let file = format!("{PACKAGE}_{version}_{arch}.{extension}");
            format!("Needs an update — download {file} from the release page, then run sudo {tool} install ./{file}")
        }
        None => {
            format!("Needs an update — download the newest .{extension} from the release page, then install it with sudo {tool} install")
        }
    }
}

/// Whether a window of the app runs from this install although none attached: an app too old to take requests, or
/// one still starting. The daemon then waits rather than replace the app under it. Only macOS can tell; the Windows
/// installer waits for the app itself, and an AppImage can be renamed over while it runs.
pub fn app_running(install: &DesktopInstall) -> bool {
    #[cfg(target_os = "macos")]
    if let DesktopInstall::MacApp { bundle } = install {
        let Ok(output) = std::process::Command::new("/bin/ps").args(["-axo", "comm="]).output() else { return false };
        return running_from_bundle(&String::from_utf8_lossy(&output.stdout), bundle);
    }
    let _ = install;
    false
}

/// `ps -o comm=` lines that are a program of `bundle` other than its daemon.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn running_from_bundle(ps: &str, bundle: &Path) -> bool {
    let prefix = format!("{}/Contents/MacOS/", bundle.display());
    ps.lines().map(str::trim).any(|line| line.starts_with(&prefix) && !line.ends_with("/kybernd") && !line.ends_with("/kybern"))
}

// ---- the release feed ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feeds {
    pub stable: String,
    pub dev: Option<String>,
    pub pubkey: String,
}

/// The app's feeds and key. Checks may point at a loopback feed with `KYBERN_UPDATE_FEED` (and
/// `KYBERN_UPDATE_DEV_FEED`) and only then replace the key with `KYBERN_UPDATE_PUBKEY`, as the app allows.
pub fn feeds(env: &dyn Fn(&str) -> Option<String>) -> Result<Feeds> {
    let loopback = |name: &str| -> Result<Option<String>> {
        let Some(value) = env(name).filter(|value| !value.is_empty()) else { return Ok(None) };
        let url = reqwest::Url::parse(&value).with_context(|| format!("{name} is not a URL"))?;
        if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
            bail!("{name} must be a loopback http address");
        }
        Ok(Some(value))
    };
    match loopback("KYBERN_UPDATE_FEED")? {
        Some(stable) => Ok(Feeds {
            stable,
            dev: loopback("KYBERN_UPDATE_DEV_FEED")?,
            pubkey: env("KYBERN_UPDATE_PUBKEY").unwrap_or_else(|| PUBKEY.into()),
        }),
        None => Ok(Feeds { stable: STABLE_FEED.into(), dev: Some(DEV_FEED.into()), pubkey: PUBKEY.into() }),
    }
}

/// The feed entry of this build (`{os}-{arch}-gpuix`, Tauri's key form). Only the `-gpuix` keys are read.
pub fn platform_key(os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    match (os, arch) {
        ("macos", _) => Some(format!("darwin-{arch}-gpuix")),
        ("linux", _) => Some(format!("linux-{arch}-gpuix")),
        ("windows", "x86_64") => Some("windows-x86_64-gpuix".into()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub url: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// `None` when the release has no build for this platform yet.
    pub entry: Option<Entry>,
}

/// One `latest.json`.
pub fn parse_manifest(manifest: &Value, key: &str) -> Result<Release> {
    let version = manifest["version"].as_str().context("The update manifest is incomplete. Try again later.")?;
    let entry = &manifest["platforms"][key];
    let entry = match (entry["url"].as_str(), entry["signature"].as_str()) {
        (Some(url), Some(signature)) => Some(Entry { url: url.into(), signature: signature.into() }),
        _ => None,
    };
    Ok(Release { version: version.trim_start_matches('v').into(), entry })
}

/// The newest release `track` offers: Stable reads its own feed; Dev takes the newer of both, so a stable release that
/// overtakes the dev line arrives on Dev too, and one failing feed only matters when the other fails as well.
pub fn track_release(track: UpdateTrack, stable: Result<Option<Release>>, dev: Option<Result<Option<Release>>>) -> Result<Option<Release>> {
    match (track, dev) {
        (UpdateTrack::Stable, _) | (UpdateTrack::Dev, None) => stable,
        (UpdateTrack::Dev, Some(dev)) => match (stable, dev) {
            (Ok(a), Ok(b)) => Ok(match (a, b) {
                (Some(a), Some(b)) => Some(if newer(&b.version, &a.version) { b } else { a }),
                (a, b) => a.or(b),
            }),
            (Ok(found), Err(_)) | (Err(_), Ok(found)) => Ok(found),
            (Err(error), Err(_)) => Err(error),
        },
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("kybernd/{}", crate::self_update::CURRENT_VERSION))
        .timeout(Duration::from_secs(10 * 60))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

async fn read_release(client: &reqwest::Client, url: &str, key: &str) -> Result<Option<Release>> {
    let response = client.get(url).header("Accept", "application/json").send().await?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    let manifest: Value = response.error_for_status()?.json().await.context("read the update manifest")?;
    parse_manifest(&manifest, key).map(Some)
}

// ---- the signature ----

/// Verify a feed `signature` (base64 of a minisign signature file) of `data` with `pubkey` (base64 of the public key
/// file). Returns the trusted comment, which the signature covers.
pub fn verify(data: &[u8], signature: &str, pubkey: &str) -> Result<String> {
    let text = |value: &str| -> Result<String> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(value.trim()).context("The update signature is malformed")?;
        String::from_utf8(bytes).context("The update signature is malformed")
    };
    let key = minisign_verify::PublicKey::decode(&text(pubkey)?).map_err(|_| anyhow!("The update public key is invalid"))?;
    let signature = minisign_verify::Signature::decode(&text(signature)?).map_err(|_| anyhow!("The update signature is malformed"))?;
    key.verify(data, &signature, true).map_err(|error| match error {
        minisign_verify::Error::UnexpectedKeyId => anyhow!("The update was signed with a different key"),
        _ => anyhow!("The update signature does not match the download"),
    })?;
    Ok(signature.trusted_comment().to_string())
}

/// The `file:` field of a `tauri signer sign` trusted comment (`timestamp:<unix>\tfile:<name>`).
pub fn signed_file(trusted_comment: &str) -> Option<&str> {
    trusted_comment.split('\t').find_map(|field| field.strip_prefix("file:"))
}

/// The key also signs older releases and other artifacts, so the trusted comment must name exactly this release's file,
/// and the release must be newer than what runs. (The macOS archive is bound by its bundle identifier instead.)
pub fn check_signed_release(trusted_comment: &str, expected_file: &str, version: &str, current: &str) -> Result<()> {
    if signed_file(trusted_comment) != Some(expected_file) {
        bail!("The update is not the build for this release. Download it from the release page.");
    }
    if !newer(version, current) {
        bail!("The update is not newer than this version. Download it from the release page.");
    }
    Ok(())
}

pub fn installer_name(version: &str) -> String {
    format!("{PACKAGE}_{version}_x64-setup.exe")
}

pub fn appimage_name(version: &str, arch: &str) -> String {
    format!("{PACKAGE}_{version}_{}.AppImage", if arch == "aarch64" { "aarch64" } else { "x86_64" })
}

// ---- prepare: read the feed, download, verify ----

/// A verified download, ready to install.
pub struct Prepared {
    pub version: String,
    pub data: Vec<u8>,
    /// The update track it was read for and the version it replaces, which the macOS bundle is checked against once
    /// it is unpacked.
    pub track: UpdateTrack,
    pub current: String,
}

/// Read the feed of `track` and, when it has a newer release for this platform, download and verify it.
pub async fn prepare(install: &DesktopInstall, track: UpdateTrack, current: &str) -> Result<Option<Prepared>> {
    let feeds = feeds(&|name| std::env::var(name).ok())?;
    let key = platform_key(std::env::consts::OS, std::env::consts::ARCH).context("Kybern publishes no app for this platform")?;
    let client = http()?;
    let stable = read_release(&client, &feeds.stable, &key).await;
    let dev = match (track, &feeds.dev) {
        (UpdateTrack::Dev, Some(dev)) => Some(read_release(&client, dev, &key).await),
        _ => None,
    };
    let release = track_release(track, stable, dev).map_err(|error| anyhow!("Unable to read the release feed: {error:#}"))?;
    let Some(release) = release.filter(|release| newer(&release.version, current)) else { return Ok(None) };
    let entry = release.entry.with_context(|| format!("Kybern {} has no app for this platform yet. Try again later.", release.version))?;
    let data = client.get(&entry.url).send().await?.error_for_status()?.bytes().await.context("download the update")?.to_vec();
    let trusted = verify(&data, &entry.signature, &feeds.pubkey)?;
    match install {
        DesktopInstall::Windows { .. } => check_signed_release(&trusted, &installer_name(&release.version), &release.version, current)?,
        DesktopInstall::AppImage { .. } => {
            check_signed_release(&trusted, &appimage_name(&release.version, std::env::consts::ARCH), &release.version, current)?
        }
        DesktopInstall::MacApp { .. } => {}
        DesktopInstall::Package(_) => bail!("A package install is updated with its package manager"),
    }
    Ok(Some(Prepared { version: release.version, data, track, current: current.to_string() }))
}

/// Install a verified download. Returns the daemon binary to restart on, or `None` when the daemon cannot restart on
/// the new version until the app opens.
pub async fn apply(install: &DesktopInstall, prepared: Prepared) -> Result<Option<PathBuf>> {
    let install = install.clone();
    tokio::task::spawn_blocking(move || match install {
        DesktopInstall::MacApp { bundle } => {
            let expected = BundleCheck { version: &prepared.version, current: &prepared.current, track: prepared.track };
            replace_bundle(&bundle, &prepared.data, &expected)?;
            Ok(Some(bundle.join("Contents").join("MacOS").join("kybernd")))
        }
        DesktopInstall::Windows { dir } => {
            run_windows_installer(&prepared.data, &dir)?;
            Ok(Some(dir.join("kybernd.exe")))
        }
        DesktopInstall::AppImage { image, bin } => {
            check_appimage(&prepared.data, std::env::consts::ARCH)?;
            check_installed_appimage(&image, std::env::consts::ARCH)?;
            replace_appimage(&image, &prepared.data)?;
            match stage_appimage_commands(&image, &bin) {
                Ok(()) => Ok(Some(bin.join("kybernd"))),
                Err(error) => {
                    tracing::warn!(target: super::TARGET, error = format!("{error:#}"), "the AppImage is updated, but its daemon could not be copied out of it");
                    Ok(None)
                }
            }
        }
        DesktopInstall::Package(_) => bail!("A package install is updated with its package manager"),
    })
    .await
    .map_err(|error| anyhow!("The update stopped unexpectedly: {error}"))?
}

// ---- macOS: the .app bundle ----

fn run(command: std::process::Command, what: &str) -> Result<String> {
    let output = command.no_window().output().with_context(|| format!("run {what}"))?;
    if !output.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `CFBundleIdentifier` of an `.app`, read with PlistBuddy (binary or XML).
fn bundle_identifier(app: &Path) -> Result<String> {
    let mut command = std::process::Command::new("/usr/libexec/PlistBuddy");
    command.args(["-c", "Print :CFBundleIdentifier"]).arg(app.join("Contents").join("Info.plist"));
    run(command, "PlistBuddy")
}

/// `CFBundleShortVersionString` of an `.app`: the release version it was packaged as (`scripts/package-app.ts`).
fn bundle_version(app: &Path) -> Result<String> {
    let mut command = std::process::Command::new("/usr/libexec/PlistBuddy");
    command.args(["-c", "Print :CFBundleShortVersionString"]).arg(app.join("Contents").join("Info.plist"));
    run(command, "PlistBuddy")
}

/// What the `.app` in a macOS update must be. The archive's signature does not name a version (its file name is the
/// same in every release), so an older signed archive would otherwise install over a newer app.
#[derive(Debug, Clone, Copy)]
pub struct BundleCheck<'a> {
    /// The release the feed offered.
    pub version: &'a str,
    /// The version that runs now.
    pub current: &'a str,
    /// This machine's update track.
    pub track: UpdateTrack,
}

/// The unpacked bundle's version must be the release's, newer than what runs, and one this machine's track takes.
pub fn check_bundle_version(found: &str, expected: &BundleCheck) -> Result<()> {
    let found = found.trim();
    if found != expected.version {
        bail!(
            "The update contains Kybern {}, not {}. Download it from the release page.",
            if found.is_empty() { "of an unknown version" } else { found },
            expected.version
        );
    }
    if !newer(found, expected.current) {
        bail!("The update is not newer than this version. Download it from the release page.");
    }
    if !on_track(expected.track, found) {
        bail!("The update is a Dev build, and this machine takes Stable releases. Download it from the release page.");
    }
    Ok(())
}

/// Replace `bundle` with the `.app` inside a verified `.app.tar.gz`. Everything is staged beside the bundle (same
/// volume, so both renames are atomic) and the old bundle comes back if the swap fails. Both the installed and the new
/// bundle must be Kybern GPUIX, so a mislabelled artifact never replaces it, and nothing else is ever replaced. The new
/// bundle must also be the expected release ([`check_bundle_version`]), so an older signed build never replaces it.
pub fn replace_bundle(bundle: &Path, archive: &[u8], expected: &BundleCheck) -> Result<()> {
    let parent = bundle.parent().context("The app has no parent folder")?;
    if bundle_identifier(bundle)? != BUNDLE_ID {
        bail!("{} is not Kybern GPUIX", bundle.display());
    }
    let staging = parent.join(format!(".kybern-update-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&staging).with_context(|| format!("{} is not writable by this user", parent.display()))?;
    let result = (|| {
        let file = staging.join("update.app.tar.gz");
        std::fs::write(&file, archive)?;
        let unpacked = staging.join("unpacked");
        std::fs::create_dir(&unpacked)?;
        let mut tar = std::process::Command::new("/usr/bin/tar");
        tar.arg("-xzf").arg(&file).arg("-C").arg(&unpacked);
        run(tar, "tar")?;
        let apps: Vec<PathBuf> = std::fs::read_dir(&unpacked)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "app"))
            .collect();
        let [next] = apps.as_slice() else { bail!("The update archive does not contain one app") };
        let identifier = bundle_identifier(next).unwrap_or_default();
        if identifier != BUNDLE_ID {
            bail!("The update is for {}, not this app", if identifier.is_empty() { "another app" } else { &identifier });
        }
        check_bundle_version(&bundle_version(next).unwrap_or_default(), expected)?;
        let previous = staging.join("previous.app");
        std::fs::rename(bundle, &previous).with_context(|| format!("move {} aside", bundle.display()))?;
        if let Err(error) = std::fs::rename(next, bundle) {
            std::fs::rename(&previous, bundle).with_context(|| format!("put {} back", bundle.display()))?;
            return Err(error).with_context(|| format!("put the new app in place of {}", bundle.display()));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

// ---- Windows: the per-user installer ----

/// The installer's arguments. NSIS wants `/D=<dir>` last and unquoted, even with spaces; a drive root keeps its
/// backslash. No `/UPDATE`: the app is closed and opens on the new version the next time the user starts it.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn installer_arguments(dir: &str) -> String {
    let dir = if dir.len() == 3 && dir.ends_with(":\\") { dir.to_string() } else { dir.trim_end_matches('\\').to_string() };
    format!("/S /D={dir}")
}

/// Every PE file starts with "MZ"; anything else is refused before it runs.
pub fn check_installer(data: &[u8]) -> Result<()> {
    if data.len() < 2 || data[0] != b'M' || data[1] != b'Z' {
        bail!("The update is not a Windows installer");
    }
    Ok(())
}

/// Write a verified installer into its own scratch folder.
pub fn stage_installer(data: &[u8], parent: &Path) -> Result<PathBuf> {
    check_installer(data)?;
    let dir = parent.join(format!("{STAGING_PREFIX}{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("kybern-gpuix-setup.exe");
    std::fs::write(&file, data)?;
    Ok(file)
}

/// Run the installer silently into `dir` and wait for it. It replaces the files in place; the running daemon's locked
/// executable is moved aside (`.old`), not stopped, so this daemon restarts on the new one afterwards.
fn run_windows_installer(data: &[u8], dir: &Path) -> Result<()> {
    let file = stage_installer(data, &std::env::temp_dir())?;
    let result = run_installer(&file, dir);
    if let Some(folder) = file.parent() {
        let _ = std::fs::remove_dir_all(folder);
    }
    result
}

#[cfg(windows)]
fn run_installer(file: &Path, dir: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new(file).no_window();
    command.raw_arg(installer_arguments(&dir.to_string_lossy()));
    let status = command.status().context("start the Kybern installer")?;
    if !status.success() {
        bail!("The Kybern installer failed (exit code {}). Run the installer from the release page.", status.code().unwrap_or(-1));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_installer(_file: &Path, _dir: &Path) -> Result<()> {
    bail!("The Windows installer runs only on Windows")
}

/// Remove installer folders older than an hour; best effort.
pub fn remove_stale_staging(parent: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(STAGING_PREFIX) {
            continue;
        }
        let old = entry.metadata().and_then(|meta| meta.modified()).ok().and_then(|at| now.duration_since(at).ok());
        if old.is_some_and(|age| age > Duration::from_secs(60 * 60)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ---- Linux: the AppImage ----

/// The bytes must be a type 2 AppImage for this machine: an ELF runtime (64-bit, little endian, the right `e_machine`)
/// with `AI\x02` at offset 8.
pub fn check_appimage(data: &[u8], arch: &str) -> Result<()> {
    let elf = data.len() >= 20 && data[..4] == [0x7f, b'E', b'L', b'F'];
    if !elf || data[8..11] != [b'A', b'I', 0x02] {
        bail!("The update is not an AppImage");
    }
    let machine = u16::from_le_bytes([data[18], data[19]]);
    let expected = if arch == "aarch64" { 183 } else { 62 };
    if data[4] != 2 || data[5] != 1 || machine != expected {
        bail!("The update is not an AppImage for {}", if arch == "aarch64" { "arm64" } else { "x86-64" });
    }
    Ok(())
}

/// The file the swap replaces must be an AppImage for this machine too: `bin/appimage` names it, and nothing else is
/// ever renamed over.
pub fn check_installed_appimage(image: &Path, arch: &str) -> Result<()> {
    use std::io::Read;
    let mut header = Vec::with_capacity(20);
    std::fs::File::open(image)
        .and_then(|file| file.take(20).read_to_end(&mut header))
        .with_context(|| format!("read {}", image.display()))?;
    check_appimage(&header, arch).map_err(|_| {
        anyhow!("{} is not a Kybern AppImage, so it is not replaced. Download the update from the release page.", image.display())
    })
}

/// Write the new image beside `image` (created exclusively, with its mode) and rename it over, so a copy that runs
/// keeps its mounted old image until it restarts.
pub fn replace_appimage(image: &Path, data: &[u8]) -> Result<()> {
    let dir = image.parent().context("The AppImage has no folder")?;
    let next = dir.join(format!(".{}.update-{}", image.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let result = (|| {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mode = (std::fs::metadata(image)?.permissions().mode() & 0o777) | 0o100;
            options.mode(mode);
            let mut file = options.open(&next)?;
            file.write_all(data)?;
            file.sync_all()?;
            std::fs::set_permissions(&next, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        {
            let mut file = options.open(&next)?;
            file.write_all(data)?;
        }
        std::fs::rename(&next, image)?;
        Ok::<_, std::io::Error>(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&next);
        if matches!(error.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) {
            bail!(
                "Kybern can't write to {}. Move the AppImage to a folder you own, or download the update from the release page.",
                dir.display()
            );
        }
        return Err(error.into());
    }
    Ok(())
}

/// The folder inside the image that holds the daemon and the CLI (`scripts/package-linux.ts`).
const IMAGE_COMMANDS: &str = "usr/lib/kybern-gpuix";

/// Copy the new image's `kybernd` and `kybern` into `bin`, where the old ones run from (each renamed over, so the
/// running daemon keeps its file). The image's runtime extracts them (`--appimage-extract`, no FUSE needed): only
/// those two files when its runtime takes a pattern, everything otherwise.
pub fn stage_appimage_commands(image: &Path, bin: &Path) -> Result<()> {
    let scratch = bin.join(format!(".extract-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&scratch)?;
    let result = (|| {
        let extracted = scratch.join("squashfs-root").join(IMAGE_COMMANDS);
        let extract = |pattern: Option<&str>| {
            let mut command = std::process::Command::new(image);
            command.arg("--appimage-extract").current_dir(&scratch).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            if let Some(pattern) = pattern {
                command.arg(pattern);
            }
            command.status()
        };
        let have = || ["kybernd", "kybern"].iter().all(|name| extracted.join(name).is_file());
        let _ = extract(Some(&format!("{IMAGE_COMMANDS}/kybern*")));
        if !have() {
            extract(None).context("run the new AppImage")?;
        }
        if !have() {
            bail!("The new AppImage has no kybernd and kybern in {IMAGE_COMMANDS}");
        }
        for name in ["kybernd", "kybern"] {
            let staged = bin.join(format!(".{name}.{}.new", std::process::id()));
            std::fs::copy(extracted.join(name), &staged)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&staged, bin.join(name))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}
