//! `bolide update`: which release asset this platform takes, whether it is newer, and
//! the swap itself — driven through a fake [`Fetch`], so nothing here touches a network.

mod common;

use std::cell::RefCell;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;

use bolide_cli::error::EXIT_FAILURE;
use bolide_cli::update::{
    self, asset_name, asset_url, compare_versions, is_source_build, parse_latest_release,
    release_target, staged_path, version_from_tag, Fetch, Outcome, LATEST_RELEASE_API,
    RELEASES_PAGE,
};

use common::TempDir;

// ------------------------------------------------------------------ pure decisions

#[test]
fn every_released_platform_has_its_target_triple() {
    assert_eq!(
        release_target("aarch64", "macos"),
        Some("aarch64-apple-darwin")
    );
    assert_eq!(
        release_target("x86_64", "macos"),
        Some("x86_64-apple-darwin")
    );
    assert_eq!(
        release_target("x86_64", "linux"),
        Some("x86_64-unknown-linux-musl")
    );
    assert_eq!(
        release_target("aarch64", "linux"),
        Some("aarch64-unknown-linux-musl")
    );
}

#[test]
fn an_unreleased_platform_has_none() {
    assert_eq!(release_target("x86_64", "windows"), None);
    assert_eq!(release_target("riscv64", "linux"), None);
    assert_eq!(release_target("x86", "freebsd"), None);
}

#[test]
fn this_build_s_platform_is_a_released_one() {
    // CI runs on linux x86_64 and macOS arm64; both must resolve.
    let target = update::this_target().expect("the test runner is a released platform");
    assert!(target.starts_with(std::env::consts::ARCH), "{target}");
}

#[test]
fn the_asset_is_a_versioned_per_target_tarball() {
    assert_eq!(
        asset_name("1.2.3", "aarch64-apple-darwin"),
        "bolide-1.2.3-aarch64-apple-darwin.tar.gz"
    );
    assert_eq!(
        asset_url("1.2.3", "aarch64-apple-darwin"),
        "https://github.com/jakequist/bolide/releases/download/v1.2.3/bolide-1.2.3-aarch64-apple-darwin.tar.gz"
    );
}

#[test]
fn the_urls_point_at_the_public_repository() {
    assert_eq!(
        RELEASES_PAGE,
        "https://github.com/jakequist/bolide/releases"
    );
    assert_eq!(
        LATEST_RELEASE_API,
        "https://api.github.com/repos/jakequist/bolide/releases/latest"
    );
}

#[test]
fn a_tag_loses_its_leading_v_and_nothing_else() {
    assert_eq!(version_from_tag("v1.2.3").unwrap(), "1.2.3");
    assert_eq!(version_from_tag("0.1.0").unwrap(), "0.1.0");
    assert_eq!(version_from_tag("  v1.0.0-rc.1\n").unwrap(), "1.0.0-rc.1");
    assert!(version_from_tag("v").is_err());
    assert!(version_from_tag("").is_err());
}

#[test]
fn versions_compare_numerically_not_as_strings() {
    assert_eq!(compare_versions("0.10.0", "0.9.0"), Some(Ordering::Greater));
    assert_eq!(compare_versions("1.0.0", "1.0.0"), Some(Ordering::Equal));
    assert_eq!(compare_versions("0.1.0", "0.2.0"), Some(Ordering::Less));
    assert_eq!(compare_versions("2.0.0", "10.0.0"), Some(Ordering::Less));
}

#[test]
fn a_prerelease_sorts_before_its_release_and_build_metadata_is_ignored() {
    assert_eq!(
        compare_versions("1.0.0-rc.1", "1.0.0"),
        Some(Ordering::Less)
    );
    assert_eq!(
        compare_versions("1.0.0-rc.2", "1.0.0-rc.10"),
        Some(Ordering::Less)
    );
    assert_eq!(
        compare_versions("1.0.0+abc", "1.0.0"),
        Some(Ordering::Equal)
    );
}

#[test]
fn a_version_that_is_not_one_does_not_compare() {
    assert_eq!(compare_versions("banana", "1.0.0"), None);
    assert_eq!(compare_versions("1.0", "1.0.0"), None);
}

#[test]
fn the_latest_release_response_yields_its_version() {
    let body = r#"{"url":"…","tag_name":"v0.2.0","name":"v0.2.0","assets":[]}"#;
    assert_eq!(parse_latest_release(body).unwrap(), "0.2.0");
}

#[test]
fn a_latest_release_response_without_a_tag_is_an_error_that_says_so() {
    let err = parse_latest_release(r#"{"message":"Not Found"}"#).unwrap_err();
    assert!(err.contains("tag_name"), "{err}");
    assert!(parse_latest_release("<html>rate limited</html>").is_err());
}

#[test]
fn a_cargo_build_is_a_source_build_and_an_install_is_not() {
    assert_eq!(
        is_source_build(Path::new("/home/me/bolide/target/release/bolide")),
        Some(PathBuf::from("/home/me/bolide"))
    );
    assert_eq!(is_source_build(Path::new("/usr/local/bin/bolide")), None);
    assert_eq!(
        is_source_build(Path::new("/home/me/.local/bin/bolide")),
        None
    );
    // `cargo install` puts a binary in ~/.cargo/bin; that is an install, and update
    // replaces it like any other.
    assert_eq!(
        is_source_build(Path::new("/home/me/.cargo/bin/bolide")),
        None
    );
}

#[test]
fn the_new_binary_is_staged_beside_the_old_one() {
    assert_eq!(
        staged_path(Path::new("/usr/local/bin/bolide")),
        PathBuf::from("/usr/local/bin/bolide.new")
    );
}

#[test]
fn check_lines_say_what_to_do() {
    let lines = update::check_lines("0.1.0", "0.2.0");
    assert_eq!(lines[0], "installed: 0.1.0");
    assert_eq!(lines[1], "latest:    0.2.0");
    assert!(lines[2].contains("bolide update"), "{lines:?}");

    let lines = update::check_lines("0.2.0", "0.2.0");
    assert!(lines[2].contains("up to date"), "{lines:?}");

    let lines = update::check_lines("0.3.0", "0.2.0");
    assert!(lines[2].contains("newer"), "{lines:?}");
}

// ------------------------------------------------------------------ the swap, faked

/// A fake GitHub: a canned latest-release body and a tarball on local disk.
struct FakeGitHub {
    latest: Result<String, String>,
    tarball: Option<PathBuf>,
    asked: RefCell<Vec<String>>,
}

impl Fetch for FakeGitHub {
    fn text(&self, url: &str) -> Result<String, String> {
        self.asked.borrow_mut().push(url.to_string());
        self.latest.clone()
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        self.asked.borrow_mut().push(url.to_string());
        let from = self.tarball.as_ref().ok_or("404 Not Found")?;
        std::fs::copy(from, dest).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A release tarball the way release.yml packs one: a single `bolide` at the root.
fn a_release_tarball(dir: &Path, contents: &str) -> PathBuf {
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("bolide"), contents).unwrap();
    let tarball = dir.join("release.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .arg("bolide")
        .status()
        .expect("tar runs");
    assert!(status.success());
    tarball
}

fn an_installed_binary(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("bolide");
    std::fs::write(&exe, "old").unwrap();
    exe
}

#[test]
fn update_replaces_the_binary_with_the_newer_release() {
    let dir = TempDir::new("update-swap");
    let exe = an_installed_binary(dir.path());
    let github = FakeGitHub {
        latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
        tarball: Some(a_release_tarball(dir.path(), "new")),
        asked: RefCell::new(Vec::new()),
    };

    let outcome = update::update(&github, &exe, "0.1.0", Some("aarch64-apple-darwin")).unwrap();

    assert_eq!(
        outcome,
        Outcome::Updated {
            from: "0.1.0".into(),
            to: "0.2.0".into()
        }
    );
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
    assert!(
        !staged_path(&exe).exists(),
        "the staged copy was left behind"
    );
    assert_eq!(
        github.asked.borrow().as_slice(),
        [
            LATEST_RELEASE_API.to_string(),
            asset_url("0.2.0", "aarch64-apple-darwin")
        ]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}

#[test]
fn an_up_to_date_binary_downloads_nothing() {
    let dir = TempDir::new("update-current");
    let exe = an_installed_binary(dir.path());
    for current in ["0.2.0", "0.3.0"] {
        let github = FakeGitHub {
            latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
            tarball: None,
            asked: RefCell::new(Vec::new()),
        };
        let outcome =
            update::update(&github, &exe, current, Some("x86_64-unknown-linux-musl")).unwrap();
        assert_eq!(
            outcome,
            Outcome::UpToDate {
                current: current.into(),
                latest: "0.2.0".into()
            }
        );
        assert_eq!(github.asked.borrow().len(), 1, "it downloaded anyway");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }
}

#[test]
fn a_failed_download_changes_nothing_and_says_where_to_look() {
    let dir = TempDir::new("update-404");
    let exe = an_installed_binary(dir.path());
    let github = FakeGitHub {
        latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
        tarball: None,
        asked: RefCell::new(Vec::new()),
    };
    let err =
        update::update(&github, &exe, "0.1.0", Some("x86_64-unknown-linux-musl")).unwrap_err();
    assert_eq!(err.code, EXIT_FAILURE);
    assert!(err.message.contains("404"), "{}", err.message);
    assert!(err.message.contains(RELEASES_PAGE), "{}", err.message);
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
}

#[test]
fn an_archive_without_a_bolide_binary_changes_nothing() {
    let dir = TempDir::new("update-empty-archive");
    let exe = an_installed_binary(dir.path());
    let staging = dir.path().join("other");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("README"), "no binary here").unwrap();
    let tarball = dir.path().join("wrong.tar.gz");
    assert!(Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .arg("README")
        .status()
        .unwrap()
        .success());
    let github = FakeGitHub {
        latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
        tarball: Some(tarball),
        asked: RefCell::new(Vec::new()),
    };
    let err =
        update::update(&github, &exe, "0.1.0", Some("x86_64-unknown-linux-musl")).unwrap_err();
    assert!(err.message.contains("bolide"), "{}", err.message);
    assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
}

#[test]
fn an_unreachable_github_is_an_error_naming_the_releases_page() {
    let dir = TempDir::new("update-offline");
    let exe = an_installed_binary(dir.path());
    let github = FakeGitHub {
        latest: Err("curl: (6) Could not resolve host".into()),
        tarball: None,
        asked: RefCell::new(Vec::new()),
    };
    let err =
        update::update(&github, &exe, "0.1.0", Some("x86_64-unknown-linux-musl")).unwrap_err();
    assert!(
        err.message.contains("Could not resolve host"),
        "{}",
        err.message
    );
    assert!(err.message.contains(RELEASES_PAGE), "{}", err.message);
}

#[test]
fn a_platform_without_a_release_is_told_so_before_anything_is_downloaded() {
    let dir = TempDir::new("update-no-target");
    let exe = an_installed_binary(dir.path());
    let github = FakeGitHub {
        latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
        tarball: None,
        asked: RefCell::new(Vec::new()),
    };
    let err = update::update(&github, &exe, "0.1.0", None).unwrap_err();
    assert!(err.message.contains("cargo install"), "{}", err.message);
    assert_eq!(github.asked.borrow().len(), 1);
}

#[test]
fn a_source_build_is_pointed_at_git_and_nothing_is_fetched() {
    let exe = Path::new("/home/me/bolide/target/debug/bolide");
    let github = FakeGitHub {
        latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
        tarball: None,
        asked: RefCell::new(Vec::new()),
    };
    let err = update::update(&github, exe, "0.1.0", Some("x86_64-unknown-linux-musl")).unwrap_err();
    assert!(err.message.contains("git"), "{}", err.message);
    assert!(
        github.asked.borrow().is_empty(),
        "went to the network anyway"
    );
}

#[test]
fn check_asks_once_and_reports_both_versions() {
    let github = FakeGitHub {
        latest: Ok(r#"{"tag_name":"v0.2.0"}"#.into()),
        tarball: None,
        asked: RefCell::new(Vec::new()),
    };
    assert_eq!(update::latest_version(&github).unwrap(), "0.2.0");
    assert_eq!(github.asked.borrow().as_slice(), [LATEST_RELEASE_API]);
}
