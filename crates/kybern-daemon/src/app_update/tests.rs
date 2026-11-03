use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kybern_protocol::methods::{AppUpdatePhase, AppUpdaterReportParams};
use kybern_protocol::{DaemonUpdateStatus, UpdateTrack};

use super::install::{self, DesktopInstall, Detected, PackageKind, Probe};
use super::*;

// A key and an `ED` (BLAKE2b-prehashed) signature made the way the app's release tooling signs
// (`apps/gpuix/src/platform/verify.feed.ts`, `tauri signer sign`): "kybern update payload", trusted comment
// `timestamp:…\tfile:kybern-gpuix_0.99.0_x64-setup.exe`.
const TEST_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEFFNEI5M0U1N0VCNkQyOUMKUldTdVM1UGxmcmJTbko1ZENTZW8xQ2lXT2poejdaZzdtK3VSYlVJVXJweXordjdJY21CUmx2RmIK";
const OTHER_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEI5RDBEN0FCRTlBNzc4QTEKUldTNTBOZXI2YWQ0b1dzbU56dklTcUY0Q2d2TzFoeVcxdExVUHpmcGpjZFlLckM0NzJ3Yy9hY1gK";
const TEST_SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTdVM1UGxmcmJTbk10UVA1cnBqaWlsZWNmdWVMTzY0cGY2L0ZvS0hjZzRtT0ZMamVIdU5DMld4V2hRTWQrRi8wVS8xbGlvcVBlU05GQVdGWk5PMmZ4enJMK0dJVCtwVHdrPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzNjc2MzgxCWZpbGU6a3liZXJuLWdwdWl4XzAuOTkuMF94NjQtc2V0dXAuZXhlCkVwdjJ6d0kvaUtpWk96cDBhdU1IWWRJVGFaLzVOckhReXJKTHF2TnJRNUYrdU5QdjlaRzRyclk5WW1FSmd4dHl0aFJJWm9XeUtzRXhrZmhod1kvbEFRPT0K";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kybern-app-update-test-{name}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A file system of exactly these files.
struct Files(HashMap<PathBuf, String>);

impl Files {
    fn new(files: &[(&Path, &str)]) -> Self {
        Self(files.iter().map(|(path, text)| (path.to_path_buf(), text.to_string())).collect())
    }
    fn detect(&self, exe: &Path, os: &str) -> Detected {
        let exists = |path: &Path| self.0.contains_key(path);
        let read = |path: &Path| self.0.get(path).cloned();
        install::detect(exe, os, &Probe { exists: &exists, read: &read })
    }
}

const PLIST: &str = "<plist><dict><key>CFBundleIdentifier</key><string>dev.kybern.gpuix</string></dict></plist>";

#[test]
fn the_daemon_knows_how_the_app_was_installed_on_each_system() {
    // macOS: the daemon is the bundle's Contents/MacOS/kybernd, and the bundle is Kybern GPUIX.
    let bundle = Path::new("/Applications/Kybern GPUIX.app");
    let exe = bundle.join("Contents/MacOS/kybernd");
    let files = Files::new(&[(&bundle.join("Contents/Info.plist"), PLIST)]);
    assert_eq!(files.detect(&exe, "macos"), Detected::Install(DesktopInstall::MacApp { bundle: bundle.into() }));
    let tauri = Files::new(&[(&bundle.join("Contents/Info.plist"), "<string>dev.kybern.desktop</string>")]);
    assert!(matches!(tauri.detect(&exe, "macos"), Detected::NotInstalled(_)), "another app's bundle is never ours");
    assert!(matches!(files.detect(Path::new("/usr/local/bin/kybernd"), "macos"), Detected::NotInstalled(_)));

    // Windows: the installer's folder has the app and its uninstaller beside the daemon.
    let dir = Path::new("/Users/u/AppData/Local/Programs/Kybern GPUIX");
    let exe = dir.join("kybernd.exe");
    let files = Files::new(&[(&dir.join("uninstall.exe"), ""), (&dir.join("Kybern GPUIX.exe"), "")]);
    assert_eq!(files.detect(&exe, "windows"), Detected::Install(DesktopInstall::Windows { dir: dir.into() }));
    let loose = Files::new(&[(&dir.join("Kybern GPUIX.exe"), "")]);
    assert!(matches!(loose.detect(&exe, "windows"), Detected::NotInstalled(_)), "a loose copy has no uninstaller");

    // Linux: a deb or rpm records its format; the AppImage's staged daemon names its image.
    let lib = Path::new("/usr/lib/kybern-gpuix");
    let deb = Files::new(&[(&lib.join("resources/package"), "deb\n")]);
    assert_eq!(deb.detect(&lib.join("kybernd"), "linux"), Detected::Install(DesktopInstall::Package(PackageKind::Deb)));
    let rpm = Files::new(&[(&lib.join("resources/package"), "rpm")]);
    assert_eq!(rpm.detect(&lib.join("kybernd"), "linux"), Detected::Install(DesktopInstall::Package(PackageKind::Rpm)));
    let bin = Path::new("/home/u/.local/share/kybern-gpuix/bin");
    let image = Path::new("/home/u/Apps/kybern-gpuix_0.21.3_x86_64.AppImage");
    let appimage = Files::new(&[(&bin.join("appimage"), "/home/u/Apps/kybern-gpuix_0.21.3_x86_64.AppImage\n"), (image, "")]);
    assert_eq!(
        appimage.detect(&bin.join("kybernd"), "linux"),
        Detected::Install(DesktopInstall::AppImage { image: image.into(), bin: bin.into() })
    );
    let gone = Files::new(&[(&bin.join("appimage"), "/home/u/Apps/kybern-gpuix_0.21.3_x86_64.AppImage")]);
    assert!(matches!(gone.detect(&bin.join("kybernd"), "linux"), Detected::NotInstalled(_)), "the image was moved or deleted");

    // A development build is rebuilt, on every system.
    let dev = Path::new("/src/kybern/target/debug/kybernd");
    assert_eq!(Files::new(&[]).detect(dev, "macos"), Detected::NotInstalled(install::DEVELOPMENT.into()));
}

#[test]
fn a_package_install_says_what_to_run() {
    assert_eq!(
        install::package_message(PackageKind::Deb, Some("0.21.4"), "x86_64"),
        "Needs an update — download kybern-gpuix_0.21.4_x86_64.deb from the release page, then run sudo apt install ./kybern-gpuix_0.21.4_x86_64.deb"
    );
    assert!(
        install::package_message(PackageKind::Rpm, Some("0.21.4"), "x86_64").contains("sudo dnf install ./kybern-gpuix_0.21.4_x86_64.rpm")
    );
    assert!(install::package_message(PackageKind::Rpm, None, "x86_64").contains(".rpm"));
}

#[test]
fn the_open_app_takes_the_update_when_it_can_and_the_daemon_when_it_is_closed() {
    let window = |can_install| Attached { connection: uuid::Uuid::now_v7(), version: "0.21.3".into(), can_install, reason: None };
    let mac = Detected::Install(DesktopInstall::MacApp { bundle: "/Applications/Kybern GPUIX.app".into() });
    let deb = Detected::Install(DesktopInstall::Package(PackageKind::Deb));
    let open = window(true);
    assert_eq!(route(Some(&open), &mac), Route::App(open.clone()));
    assert_eq!(route(None, &mac), Route::Daemon(DesktopInstall::MacApp { bundle: "/Applications/Kybern GPUIX.app".into() }));
    // A system package is never replaced from here, open app or not.
    assert_eq!(route(Some(&open), &deb), Route::Package(PackageKind::Deb));
    assert_eq!(route(None, &deb), Route::Package(PackageKind::Deb));
    // An app that cannot install (a development run, updates turned off) says why.
    let cannot = Attached { reason: Some("Development build; rebuild it instead.".into()), ..window(false) };
    assert_eq!(route(Some(&cannot), &mac), Route::Manual("Development build; rebuild it instead.".into()));
    assert_eq!(route(None, &Detected::NotInstalled("x".into())), Route::Manual("x".into()));
}

#[test]
fn feeds_are_the_apps_and_only_a_loopback_check_may_change_the_key() {
    let env = |vars: &'static [(&'static str, &'static str)]| {
        move |name: &str| vars.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    };
    let real = install::feeds(&env(&[])).unwrap();
    assert_eq!(
        (real.stable.as_str(), real.dev.as_deref(), real.pubkey.as_str()),
        (install::STABLE_FEED, Some(install::DEV_FEED), install::PUBKEY)
    );
    // The key alone never changes.
    assert_eq!(install::feeds(&env(&[("KYBERN_UPDATE_PUBKEY", "evil")])).unwrap().pubkey, install::PUBKEY);
    let local = install::feeds(&env(&[("KYBERN_UPDATE_FEED", "http://127.0.0.1:9/latest.json"), ("KYBERN_UPDATE_PUBKEY", "k")])).unwrap();
    assert_eq!((local.stable.as_str(), local.dev, local.pubkey.as_str()), ("http://127.0.0.1:9/latest.json", None, "k"));
    assert!(install::feeds(&env(&[("KYBERN_UPDATE_FEED", "https://example.com/latest.json")])).is_err());
    assert!(
        install::feeds(&env(&[("KYBERN_UPDATE_FEED", "http://127.0.0.1:9/a"), ("KYBERN_UPDATE_DEV_FEED", "http://10.0.0.1/b")])).is_err()
    );
}

#[test]
fn the_key_matches_the_apps_updater_config() {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/gpuix/src/platform/updater.json");
    let Ok(text) = std::fs::read_to_string(config) else { return };
    let config: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(config["pubkey"], install::PUBKEY);
    assert_eq!(config["endpoints"][0], install::STABLE_FEED);
    assert_eq!(config["devEndpoints"][0], install::DEV_FEED);
}

#[test]
fn the_feed_entry_is_this_platforms_gpuix_build_on_its_own_track() {
    assert_eq!(install::platform_key("macos", "aarch64").as_deref(), Some("darwin-aarch64-gpuix"));
    assert_eq!(install::platform_key("macos", "x86_64").as_deref(), Some("darwin-x86_64-gpuix"));
    assert_eq!(install::platform_key("windows", "x86_64").as_deref(), Some("windows-x86_64-gpuix"));
    assert_eq!(install::platform_key("windows", "aarch64"), None);
    assert_eq!(install::platform_key("linux", "x86_64").as_deref(), Some("linux-x86_64-gpuix"));

    let manifest = serde_json::json!({ "version": "v0.21.4", "platforms": {
        "darwin-aarch64": { "url": "https://x/desktop.tar.gz", "signature": "tauri" },
        "darwin-aarch64-gpuix": { "url": "https://x/gpuix.tar.gz", "signature": "s" },
    }});
    let release = install::parse_manifest(&manifest, "darwin-aarch64-gpuix").unwrap();
    assert_eq!(release.version, "0.21.4");
    assert_eq!(release.entry.unwrap().url, "https://x/gpuix.tar.gz");
    assert_eq!(install::parse_manifest(&manifest, "linux-x86_64-gpuix").unwrap().entry, None, "no build for this platform yet");
    assert!(install::parse_manifest(&serde_json::json!({}), "x").is_err());

    let release = |version: &str| Ok::<_, anyhow::Error>(Some(install::Release { version: version.into(), entry: None }));
    let version = |found: anyhow::Result<Option<install::Release>>| found.unwrap().map(|release| release.version);
    assert_eq!(
        version(install::track_release(UpdateTrack::Stable, release("0.21.3"), Some(release("0.22.0-dev.1")))).as_deref(),
        Some("0.21.3")
    );
    assert_eq!(
        version(install::track_release(UpdateTrack::Dev, release("0.21.3"), Some(release("0.22.0-dev.1")))).as_deref(),
        Some("0.22.0-dev.1")
    );
    assert_eq!(
        version(install::track_release(UpdateTrack::Dev, release("0.22.0"), Some(release("0.22.0-dev.1")))).as_deref(),
        Some("0.22.0")
    );
    assert_eq!(
        version(install::track_release(UpdateTrack::Dev, release("0.21.3"), Some(Err(anyhow::anyhow!("404"))))).as_deref(),
        Some("0.21.3"),
        "before the first dev release the dev feed is missing"
    );
}

#[test]
fn a_download_installs_only_with_a_valid_signature_for_this_release() {
    let data = b"kybern update payload";
    let trusted = install::verify(data, TEST_SIGNATURE, TEST_KEY).unwrap();
    assert_eq!(install::signed_file(&trusted), Some("kybern-gpuix_0.99.0_x64-setup.exe"));
    assert!(install::verify(b"kybern update payloaD", TEST_SIGNATURE, TEST_KEY).unwrap_err().to_string().contains("does not match"));
    assert!(install::verify(data, TEST_SIGNATURE, OTHER_KEY).unwrap_err().to_string().contains("different key"));
    assert!(install::verify(data, "garbage", TEST_KEY).is_err());
    // An edited trusted comment breaks the global signature.
    use base64::Engine;
    let text = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(TEST_SIGNATURE).unwrap()).unwrap();
    let edited = text.replace("file:kybern-gpuix_0.99.0_x64-setup.exe", "file:kybern-gpuix_9.99.0_x64-setup.exe");
    assert!(install::verify(data, &base64::engine::general_purpose::STANDARD.encode(edited), TEST_KEY).is_err());

    // The key signs older releases and other artifacts too: the file name and a newer version are required.
    install::check_signed_release(&trusted, &install::installer_name("0.99.0"), "0.99.0", "0.21.3").unwrap();
    assert!(install::check_signed_release(&trusted, &install::installer_name("1.0.0"), "1.0.0", "0.21.3").is_err());
    assert!(install::check_signed_release(&trusted, &install::installer_name("0.99.0"), "0.99.0", "0.99.0").is_err());
    assert_eq!(install::appimage_name("0.21.4", "x86_64"), "kybern-gpuix_0.21.4_x86_64.AppImage");
}

#[test]
fn the_windows_installer_runs_silently_into_the_install_folder() {
    assert_eq!(
        install::installer_arguments(r"C:\Users\Bagus\AppData\Local\Programs\Kybern GPUIX\"),
        r"/S /D=C:\Users\Bagus\AppData\Local\Programs\Kybern GPUIX"
    );
    assert_eq!(install::installer_arguments(r"D:\"), r"/S /D=D:\");
    assert!(!install::installer_arguments(r"C:\x").contains("/UPDATE"), "the closed app is not started");
    assert!(install::check_installer(b"MZ\x90\x00").is_ok());
    assert!(install::check_installer(b"\x7fELF").is_err());

    let dir = scratch("installer");
    let file = install::stage_installer(b"MZrest", &dir).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"MZrest");
    assert!(install::stage_installer(b"PK", &dir).is_err());
    // Folders older than an hour go; a fresh one stays.
    install::remove_stale_staging(&dir, std::time::SystemTime::now());
    assert!(file.exists());
    install::remove_stale_staging(&dir, std::time::SystemTime::now() + std::time::Duration::from_secs(2 * 60 * 60));
    assert!(!file.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// The first 20 bytes of a type 2 AppImage for `machine`.
fn appimage_header(machine: u16) -> Vec<u8> {
    let mut data = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, b'A', b'I', 2, 0, 0, 0, 0, 0, 2, 0];
    data.extend(machine.to_le_bytes());
    data.extend(b"rest of the image");
    data
}

#[test]
fn an_appimage_update_must_be_an_appimage_for_this_machine() {
    assert!(install::check_appimage(&appimage_header(62), "x86_64").is_ok());
    assert!(install::check_appimage(&appimage_header(183), "aarch64").is_ok());
    assert!(install::check_appimage(&appimage_header(183), "x86_64").unwrap_err().to_string().contains("x86-64"));
    let mut plain_elf = appimage_header(62);
    plain_elf[8] = 0;
    assert!(install::check_appimage(&plain_elf, "x86_64").is_err());
    assert!(install::check_appimage(b"MZ", "x86_64").is_err());
}

#[cfg(unix)]
#[test]
fn an_appimage_is_renamed_over_and_its_daemon_copied_out() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("appimage");
    let image = dir.join("kybern-gpuix_0.21.3_x86_64.AppImage");
    // A stand-in image whose runtime extracts the commands the way `--appimage-extract` does.
    let script = "#!/bin/sh\n[ \"$1\" = --appimage-extract ] || exit 2\nmkdir -p squashfs-root/usr/lib/kybern-gpuix\nprintf new-daemon > squashfs-root/usr/lib/kybern-gpuix/kybernd\nprintf new-cli > squashfs-root/usr/lib/kybern-gpuix/kybern\n";
    std::fs::write(&image, "old image").unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o750)).unwrap();
    install::replace_appimage(&image, script.as_bytes()).unwrap();
    assert_eq!(std::fs::read_to_string(&image).unwrap(), script);
    assert_eq!(std::fs::metadata(&image).unwrap().permissions().mode() & 0o777, 0o750, "the image keeps its mode");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "nothing is left beside it");

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("kybernd"), "old-daemon").unwrap();
    install::stage_appimage_commands(&image, &bin).unwrap();
    assert_eq!(std::fs::read_to_string(bin.join("kybernd")).unwrap(), "new-daemon");
    assert_eq!(std::fs::read_to_string(bin.join("kybern")).unwrap(), "new-cli");
    assert_eq!(std::fs::metadata(bin.join("kybernd")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 2, "the extraction folder is gone");
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "macos")]
fn fake_bundle(dir: &Path, identifier: &str, version: &str) -> PathBuf {
    let app = dir.join("Kybern GPUIX.app");
    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{identifier}</string><key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>\n"),
    )
    .unwrap();
    std::fs::write(app.join("Contents/MacOS/kybernd"), version).unwrap();
    app
}

#[cfg(target_os = "macos")]
fn tar_gz(app: &Path, out: &Path) -> Vec<u8> {
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("-czf")
        .arg(out)
        .arg("-C")
        .arg(app.parent().unwrap())
        .arg(app.file_name().unwrap())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(out).unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn the_app_bundle_is_swapped_in_place_and_only_for_kybern() {
    let root = scratch("bundle");
    let installed = fake_bundle(&root.join("Applications"), "dev.kybern.gpuix", "0.21.3");
    let next = tar_gz(&fake_bundle(&root.join("next"), "dev.kybern.gpuix", "0.21.4"), &root.join("next.tar.gz"));
    let wrong = tar_gz(&fake_bundle(&root.join("wrong"), "dev.kybern.desktop", "9.0.0"), &root.join("wrong.tar.gz"));

    let expected = install::BundleCheck { version: "0.21.4", current: "0.21.3", track: UpdateTrack::Stable };
    assert!(install::replace_bundle(&installed, &wrong, &expected).unwrap_err().to_string().contains("dev.kybern.desktop"));
    assert_eq!(
        std::fs::read_to_string(installed.join("Contents/MacOS/kybernd")).unwrap(),
        "0.21.3",
        "a refused update leaves the app alone"
    );
    // An older signed build, or one that is not the release the feed named, is refused and leaves the app alone.
    let older = tar_gz(&fake_bundle(&root.join("older"), "dev.kybern.gpuix", "0.21.2"), &root.join("older.tar.gz"));
    let older_expected = install::BundleCheck { version: "0.21.2", ..expected };
    assert!(install::replace_bundle(&installed, &older, &older_expected).unwrap_err().to_string().contains("not newer"));
    assert!(install::replace_bundle(&installed, &older, &expected).unwrap_err().to_string().contains("contains Kybern 0.21.2, not 0.21.4"));
    assert_eq!(std::fs::read_to_string(installed.join("Contents/MacOS/kybernd")).unwrap(), "0.21.3");
    install::replace_bundle(&installed, &next, &expected).unwrap();
    assert_eq!(std::fs::read_to_string(installed.join("Contents/MacOS/kybernd")).unwrap(), "0.21.4");
    let left: Vec<_> = std::fs::read_dir(root.join("Applications")).unwrap().flatten().map(|entry| entry.file_name()).collect();
    assert_eq!(left, vec![std::ffi::OsString::from("Kybern GPUIX.app")], "the staging folder is gone");

    // Something that is not Kybern GPUIX is never replaced.
    let other = fake_bundle(&root.join("Other"), "com.example.other", "1");
    assert!(install::replace_bundle(&other, &next, &expected).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_macos_bundle_must_be_the_release_newer_than_this_one_and_on_this_track() {
    let stable = install::BundleCheck { version: "0.21.4", current: "0.21.3", track: UpdateTrack::Stable };
    assert!(install::check_bundle_version("0.21.4\n", &stable).is_ok());
    assert!(install::check_bundle_version("0.21.5", &stable).unwrap_err().to_string().contains("not 0.21.4"));
    assert!(install::check_bundle_version("", &stable).unwrap_err().to_string().contains("unknown version"));
    let same = install::BundleCheck { current: "0.21.4", ..stable };
    assert!(install::check_bundle_version("0.21.4", &same).unwrap_err().to_string().contains("not newer"));
    let older = install::BundleCheck { version: "0.21.2", ..stable };
    assert!(install::check_bundle_version("0.21.2", &older).unwrap_err().to_string().contains("not newer"));
    let dev_build = install::BundleCheck { version: "0.21.5-dev.1", ..stable };
    assert!(install::check_bundle_version("0.21.5-dev.1", &dev_build).unwrap_err().to_string().contains("Stable"));
    let on_dev = install::BundleCheck { track: UpdateTrack::Dev, ..dev_build };
    assert!(install::check_bundle_version("0.21.5-dev.1", &on_dev).is_ok());
}

#[test]
fn an_appimage_is_swapped_only_over_an_appimage() {
    let dir = scratch("appimage-target");
    let image = dir.join("Kybern.AppImage");
    std::fs::write(&image, appimage_header(62)).unwrap();
    assert!(install::check_installed_appimage(&image, "x86_64").is_ok());
    assert!(install::check_installed_appimage(&image, "aarch64").is_err(), "an image for another machine");
    std::fs::write(&image, "#!/bin/sh\necho not an image\n").unwrap();
    assert!(install::check_installed_appimage(&image, "x86_64").unwrap_err().to_string().contains("is not a Kybern AppImage"));
    std::fs::write(&image, "").unwrap();
    assert!(install::check_installed_appimage(&image, "x86_64").is_err(), "an empty file");
    assert!(install::check_installed_appimage(&dir.join("missing"), "x86_64").is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_unattached_window_of_the_app_keeps_the_daemon_from_replacing_it() {
    let bundle = Path::new("/Applications/Kybern GPUIX.app");
    let daemon_only = "/Applications/Kybern GPUIX.app/Contents/MacOS/kybernd\n/usr/sbin/cfprefsd\n";
    assert!(!install::running_from_bundle(daemon_only, bundle));
    let app = "/Applications/Kybern GPUIX.app/Contents/MacOS/Kybern GPUIX\n";
    assert!(install::running_from_bundle(app, bundle));
    assert!(!install::running_from_bundle("/Applications/Kybern GPUIX Old.app/Contents/MacOS/Kybern GPUIX\n", bundle));
}

#[test]
fn requests_keep_the_highest_version_and_a_closed_window_hands_the_update_back() {
    let updates = AppUpdates::default();
    updates.request(Some("0.21.4".into()), true);
    assert!(updates.pending().unwrap().automatic);
    updates.request(Some("0.21.3".into()), true);
    assert_eq!(updates.pending().unwrap().version.as_deref(), Some("0.21.4"));
    assert!(updates.pending().unwrap().automatic, "following twice is still automatic");
    updates.request(None, false);
    assert!(!updates.pending().unwrap().automatic, "someone asked for it");
    updates.request(None, true);
    assert!(!updates.pending().unwrap().automatic, "and following again does not change that");
    assert_eq!(updates.pending().unwrap().version.as_deref(), Some("0.21.4"));

    let first = uuid::Uuid::now_v7();
    let second = uuid::Uuid::now_v7();
    assert!(updates.hand_to(first).is_some());
    assert!(updates.hand_to(first).is_none(), "handed over once");
    updates.detach(second);
    assert_eq!(updates.pending().unwrap().handed_to, Some(first), "another window closing changes nothing");
    updates.detach(first);
    let pending = updates.pending().unwrap();
    assert_eq!(pending.handed_to, None);
    assert!(pending.released_at.is_some(), "the daemon waits before installing it itself");
}

struct Daemon {
    state: AppState,
    root: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn desktop_daemon(detected: Detected) -> Daemon {
    let root = scratch("state");
    let state = AppState::initialize(&crate::config::Paths::resolve(Some(root.clone())).unwrap()).unwrap();
    state.desktop_managed.store(true, Ordering::SeqCst);
    *state.app_updates.test_install.lock().unwrap() = Some(detected);
    Daemon { state, root }
}

fn attach(state: &AppState, can_install: bool) -> uuid::Uuid {
    let connection = uuid::Uuid::now_v7();
    state.app_updates.attach(Attached { connection, version: CURRENT_VERSION.into(), can_install, reason: None });
    connection
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_app_is_asked_to_update_once_nothing_runs_and_reports_back() {
    let daemon = desktop_daemon(Detected::Install(DesktopInstall::MacApp { bundle: "/nowhere/Kybern GPUIX.app".into() }));
    let state = &daemon.state;
    let mut notices = state.app_updates.subscribe();
    let window = attach(state, true);
    assert_eq!(tick(state).await, Step::Idle, "nothing asked for");

    let answer = crate::network::updates::start_self(state, None).await;
    assert!(answer.started, "{answer:?}");
    // Never in the middle of work.
    state.app_updates.test_busy.store(true, Ordering::SeqCst);
    assert_eq!(tick(state).await, Step::Wait);
    assert_eq!(state.daemon_updates.get().status, DaemonUpdateStatus::Waiting);
    assert!(notices.try_recv().is_err(), "the app hears nothing while work runs");
    state.app_updates.test_busy.store(false, Ordering::SeqCst);

    assert_eq!(tick(state).await, Step::Handed(window));
    let notice = notices.try_recv().unwrap();
    assert_eq!(notice.connection, window);
    assert_eq!(tick(state).await, Step::WithWindow, "asked once");

    report(state, window, AppUpdaterReportParams { phase: AppUpdatePhase::Restarting, version: Some("0.99.0".into()), message: None });
    assert_eq!(state.daemon_updates.get().status, DaemonUpdateStatus::Restarting);
    assert_eq!(state.app_updates.pending(), None, "the app's restart ends it; the daemon does not install it again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_that_fails_or_finds_nothing_closes_the_request() {
    let daemon = desktop_daemon(Detected::Install(DesktopInstall::MacApp { bundle: "/nowhere/Kybern GPUIX.app".into() }));
    let state = &daemon.state;
    let window = attach(state, true);
    request(state, Some("0.99.0".into()), false);
    assert_eq!(tick(state).await, Step::Handed(window));
    report(
        state,
        window,
        AppUpdaterReportParams {
            phase: AppUpdatePhase::Failed,
            version: None,
            message: Some("The update signature does not match the download".into()),
        },
    );
    assert_eq!(state.app_updates.pending(), None);
    let record = state.daemon_updates.get();
    assert_eq!((record.status, record.message.as_str()), (DaemonUpdateStatus::Failed, "The update signature does not match the download"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_app_is_updated_by_the_daemon_but_not_right_after_its_window_quit() {
    let daemon = desktop_daemon(Detected::Install(DesktopInstall::MacApp { bundle: "/nowhere/Kybern GPUIX.app".into() }));
    let state = &daemon.state;
    let window = attach(state, true);
    request(state, None, false);
    assert_eq!(tick(state).await, Step::Handed(window));
    // The window quit without a word, perhaps to restart on the new version: the daemon waits before doing it itself.
    state.app_updates.detach(window);
    assert_eq!(tick(state).await, Step::Wait);
    assert!(state.app_updates.pending().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_package_install_and_a_development_build_are_not_started() {
    let deb = desktop_daemon(Detected::Install(DesktopInstall::Package(PackageKind::Deb)));
    let answer = crate::network::updates::start_self(&deb.state, Some("99.0.0")).await;
    assert!(!answer.started);
    assert!(
        answer.reason.as_deref().is_some_and(|reason| reason.starts_with("Needs an update") && reason.contains("kybern-gpuix_99.0.0")),
        "{answer:?}"
    );
    attach(&deb.state, true);
    assert!(!crate::network::updates::start_self(&deb.state, Some("99.0.0")).await.started, "an open app does not change that");

    let dev = desktop_daemon(Detected::NotInstalled(install::DEVELOPMENT.into()));
    let answer = crate::network::updates::start_self(&dev.state, Some("99.0.0")).await;
    assert_eq!(answer.reason.as_deref(), Some(install::DEVELOPMENT));
    // An app that cannot install says why itself.
    dev.state.app_updates.attach(Attached {
        connection: uuid::Uuid::now_v7(),
        version: "x".into(),
        can_install: false,
        reason: Some("Updates are off".into()),
    });
    assert_eq!(crate::network::updates::start_self(&dev.state, Some("99.0.0")).await.reason.as_deref(), Some("Updates are off"));
    // Already there wins.
    assert_eq!(crate::network::updates::start_self(&dev.state, Some(CURRENT_VERSION)).await.reason.as_deref(), Some("Already up to date"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_followed_update_also_waits_for_terminals_and_says_so_to_the_window() {
    let daemon = desktop_daemon(Detected::Install(DesktopInstall::MacApp { bundle: "/nowhere/Kybern GPUIX.app".into() }));
    let state = &daemon.state;
    let mut notices = state.app_updates.subscribe();
    let window = attach(state, true);
    state.app_updates.test_terminals.store(true, Ordering::SeqCst);

    // Following another machine: an open terminal (a dev server) keeps it waiting.
    request(state, Some("0.99.0".into()), true);
    assert_eq!(tick(state).await, Step::Wait);
    assert!(notices.try_recv().is_err());
    state.app_updates.test_terminals.store(false, Ordering::SeqCst);
    assert_eq!(tick(state).await, Step::Handed(window));
    assert!(notices.try_recv().unwrap().automatic, "the window waits for terminals too");

    // An update someone asked for does not wait for terminals.
    state.app_updates.finish();
    state.app_updates.test_terminals.store(true, Ordering::SeqCst);
    request(state, Some("0.99.0".into()), false);
    assert_eq!(tick(state).await, Step::Handed(window));
    assert!(!notices.try_recv().unwrap().automatic);
}
