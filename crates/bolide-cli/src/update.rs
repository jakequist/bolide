//! `bolide update` — replace this binary with the latest GitHub release.
//!
//! The release is a tarball per target triple, `bolide-<version>-<target>.tar.gz`, with
//! one `bolide` at its root; `install.sh` installs the same assets. `update` asks the
//! GitHub Releases API which tag is latest, compares it with the version this binary was
//! built as, and — when the release is newer — downloads the tarball for this platform,
//! unpacks it, and renames the new binary over the running one. The rename is atomic on
//! one filesystem, which is why the new binary is staged *beside* the old one rather than
//! in a temp dir: an interrupted update leaves the old binary, never half of a new one.
//!
//! **HTTP goes through `curl`**, behind the [`Fetch`] seam. bolide's own HTTP client
//! (`reqwest`, default features off) speaks plain HTTP to its loopback server and has no
//! TLS stack; pulling one in for a command run once a month would grow every build for
//! the sake of this one. `curl` is on every macOS and on nearly every Linux, and a
//! machine without it gets an error that says so. Tests drive [`update`] through a fake
//! `Fetch` and never touch a network.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::CliError;

/// The literal behind [`REPO`], so the URLs below can be `const`.
macro_rules! repo {
    () => {
        "jakequist/bolide"
    };
}

/// The GitHub repository releases come from.
pub const REPO: &str = repo!();

/// The human-readable release list, named in every error.
pub const RELEASES_PAGE: &str = concat!("https://github.com/", repo!(), "/releases");

/// The API document naming the latest release.
pub const LATEST_RELEASE_API: &str =
    concat!("https://api.github.com/repos/", repo!(), "/releases/latest");

/// The one command that installs bolide from scratch.
pub const INSTALL_ONELINER: &str = concat!(
    "curl -fsSL https://raw.githubusercontent.com/",
    repo!(),
    "/main/install.sh | sh"
);

/// The from-source alternative, for a platform with no prebuilt binary.
pub const CARGO_INSTALL: &str = concat!(
    "cargo install --git https://github.com/",
    repo!(),
    " bolide-cli"
);

/// The binary's name, inside the tarball and on disk.
pub const BINARY: &str = "bolide";

/// The release target triple for an `(arch, os)` pair, as `std::env::consts` spells
/// them, or `None` when no release is built for it.
///
/// Linux gets the musl builds: statically linked, so one binary runs on any distro.
pub fn release_target(arch: &str, os: &str) -> Option<&'static str> {
    match (arch, os) {
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("x86_64", "linux") => Some("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// [`release_target`] for the platform this binary was built for.
pub fn this_target() -> Option<&'static str> {
    release_target(std::env::consts::ARCH, std::env::consts::OS)
}

/// `bolide-<version>-<target>.tar.gz`.
///
/// Also stated in `install.sh` (which downloads it) and `.github/workflows/release.yml`
/// (which packs it): a shell script, a workflow and this binary cannot share a constant.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("{BINARY}-{version}-{target}.tar.gz")
}

/// The immutable, versioned URL of a release asset — the version that was just checked,
/// not whatever `latest` points at by the time the download starts.
pub fn asset_url(version: &str, target: &str) -> String {
    format!(
        "{RELEASES_PAGE}/download/v{version}/{}",
        asset_name(version, target)
    )
}

/// `v1.2.3` → `1.2.3`; surrounding whitespace is dropped. An error when nothing is left.
pub fn version_from_tag(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    let version = trimmed.strip_prefix('v').unwrap_or(trimmed);
    if version.is_empty() {
        return Err(format!("release tag {tag:?} does not contain a version"));
    }
    Ok(version.to_string())
}

/// The version in a `GET /repos/…/releases/latest` body.
pub fn parse_latest_release(body: &str) -> Result<String, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("the GitHub API answered with something that is not JSON ({e})"))?;
    let tag = json
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            let said = json
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(|m| format!(" (GitHub said: {m})"))
                .unwrap_or_default();
            format!("the GitHub API answer carried no tag_name{said}")
        })?;
    version_from_tag(tag)
}

/// Semver precedence between two `MAJOR.MINOR.PATCH[-PRE][+BUILD]` versions: numeric
/// fields compare as numbers, a prerelease sorts before its release, and build metadata
/// is ignored. `None` when either is not a version of that shape.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (a_core, a_pre) = split_version(a)?;
    let (b_core, b_pre) = split_version(b)?;
    Some(a_core.cmp(&b_core).then_with(|| match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => compare_prerelease(a, b),
    }))
}

fn split_version(v: &str) -> Option<([u64; 3], Option<&str>)> {
    let v = v.split_once('+').map_or(v, |(v, _)| v);
    let (core, pre) = match v.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (v, None),
    };
    let mut parts = core.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((out, pre))
}

fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let mut a = a.split('.');
    let mut b = b.split('.');
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

/// The checkout an executable was built in, when it is a `cargo build` artifact — a
/// `target/` directory in its ancestry. Replacing that binary would be undone by the
/// next build, so `update` points at git instead.
pub fn is_source_build(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .skip(1)
        .find(|dir| dir.file_name().is_some_and(|name| name == "target"))
        .map(|target| target.parent().unwrap_or(target).to_path_buf())
}

/// `<exe>.new`, beside the binary it will replace.
pub fn staged_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_os_string();
    name.push(".new");
    PathBuf::from(name)
}

/// What `bolide update --check` prints.
pub fn check_lines(current: &str, latest: &str) -> Vec<String> {
    let verdict = match compare_versions(current, latest) {
        Some(Ordering::Less) => format!("run `bolide update` to install {latest}"),
        Some(Ordering::Equal) => "up to date".to_string(),
        Some(Ordering::Greater) => "this build is newer than the latest release".to_string(),
        None => format!("cannot compare {current:?} with {latest:?}"),
    };
    vec![
        format!("installed: {current}"),
        format!("latest:    {latest}"),
        verdict,
    ]
}

/// How `update` reaches GitHub. [`Curl`] in the binary; a fake in the tests.
pub trait Fetch {
    /// The body at `url`, or a sentence saying why not.
    fn text(&self, url: &str) -> Result<String, String>;
    /// Write the body at `url` to `dest`, or a sentence saying why not.
    fn download(&self, url: &str, dest: &Path) -> Result<(), String>;
}

/// [`Fetch`] by running `curl`.
pub struct Curl;

impl Curl {
    fn run(args: &[&str]) -> Result<Vec<u8>, String> {
        let output = Command::new("curl")
            .args(["-fsSL", "--proto", "=https", "--tlsv1.2"])
            .args(args)
            .output()
            .map_err(|e| format!("could not run curl ({e}); bolide update needs curl on PATH"))?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            match output.status.code() {
                Some(code) => format!("curl exited {code}"),
                None => "curl was killed by a signal".to_string(),
            }
        } else {
            stderr
        })
    }
}

impl Fetch for Curl {
    fn text(&self, url: &str) -> Result<String, String> {
        let body = Curl::run(&[
            "--max-time",
            "15",
            "-H",
            "Accept: application/vnd.github+json",
            url,
        ])?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), String> {
        let dest = dest.to_string_lossy();
        Curl::run(&["--max-time", "300", "-o", &dest, url]).map(|_| ())
    }
}

/// The latest released version, per the GitHub API.
pub fn latest_version(fetch: &dyn Fetch) -> Result<String, CliError> {
    fetch
        .text(LATEST_RELEASE_API)
        .and_then(|body| parse_latest_release(&body))
        .map_err(|detail| {
            CliError::failure(format!(
                "could not ask GitHub for the latest bolide release: {detail}\n\
                 Check the network and try again, or see {RELEASES_PAGE}"
            ))
        })
}

/// What an update did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing: this build is the latest release, or newer.
    UpToDate {
        /// This binary's version.
        current: String,
        /// The latest release's.
        latest: String,
    },
    /// The binary was replaced.
    Updated {
        /// The version that was running.
        from: String,
        /// The version now installed.
        to: String,
    },
}

/// Replace `exe` with the latest release for `target`, if it is newer than `current`.
///
/// Every failure leaves `exe` exactly as it was.
pub fn update(
    fetch: &dyn Fetch,
    exe: &Path,
    current: &str,
    target: Option<&str>,
) -> Result<Outcome, CliError> {
    // Before the network, so a dev checkout fails fast and offline.
    if let Some(checkout) = is_source_build(exe) {
        return Err(CliError::failure(format!(
            "this bolide is a cargo build in {}, not an installed binary; the next build \
             would undo an update. Update the checkout with git instead:\n  git -C {} pull",
            checkout.display(),
            checkout.display()
        )));
    }

    let latest = latest_version(fetch)?;
    if compare_versions(current, &latest) != Some(Ordering::Less) {
        return Ok(Outcome::UpToDate {
            current: current.to_string(),
            latest,
        });
    }

    let target = target.ok_or_else(|| {
        CliError::failure(format!(
            "bolide {latest} has no prebuilt binary for {}-{}; build it from source:\n  {CARGO_INSTALL}",
            std::env::consts::ARCH,
            std::env::consts::OS
        ))
    })?;

    let work = WorkDir::new()?;
    let url = asset_url(&latest, target);
    let tarball = work.0.join(asset_name(&latest, target));
    fetch.download(&url, &tarball).map_err(|detail| {
        CliError::failure(format!(
            "could not download {url}: {detail}\nNothing was changed. See {RELEASES_PAGE}"
        ))
    })?;
    unpack(&tarball, &work.0)?;
    let unpacked = work.0.join(BINARY);
    if !unpacked.is_file() {
        return Err(CliError::failure(format!(
            "the {latest} release archive holds no {BINARY} binary; nothing was changed. \
             See {RELEASES_PAGE}"
        )));
    }

    swap_in(&unpacked, exe)?;
    Ok(Outcome::Updated {
        from: current.to_string(),
        to: latest,
    })
}

fn unpack(tarball: &Path, into: &Path) -> Result<(), CliError> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(into)
        .status()
        .map_err(|e| CliError::failure(format!("could not run tar to unpack the release: {e}")))?;
    if !status.success() {
        return Err(CliError::failure(format!(
            "could not unpack the release archive (tar exited {}); nothing was changed",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Copy `new` to `<exe>.new`, make it executable, rename it over `exe`.
fn swap_in(new: &Path, exe: &Path) -> Result<(), CliError> {
    let staged = staged_path(exe);
    let cannot = |e: std::io::Error| {
        let _ = std::fs::remove_file(&staged);
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "\nRe-run with the permissions that installed it (e.g. sudo bolide update), \
                 or reinstall:\n  {INSTALL_ONELINER}"
            )
        } else {
            String::new()
        };
        CliError::failure(format!(
            "could not replace {}: {e}; nothing was changed{hint}",
            exe.display()
        ))
    };
    std::fs::copy(new, &staged).map_err(cannot)?;
    make_executable(&staged).map_err(cannot)?;
    std::fs::rename(&staged, exe).map_err(cannot)?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A scratch directory for one update, removed however the update ends.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> Result<WorkDir, CliError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bolide-update-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| {
            CliError::failure(format!(
                "could not create a scratch directory at {}: {e}",
                dir.display()
            ))
        })?;
        Ok(WorkDir(dir))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `bolide update [--check]`: the wiring over [`Curl`] and this process.
pub fn run(check: bool) -> Result<(), CliError> {
    let current = env!("CARGO_PKG_VERSION");
    if check {
        let latest = latest_version(&Curl)?;
        for line in check_lines(current, &latest) {
            println!("{line}");
        }
        return Ok(());
    }

    // Resolve a symlink (a package manager's, say) so the rename replaces the real file
    // rather than the link to it.
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| {
            CliError::failure(format!(
                "could not find the running bolide binary ({e}); reinstall with:\n  {INSTALL_ONELINER}"
            ))
        })?;
    match update(&Curl, &exe, current, this_target())? {
        Outcome::UpToDate { current, latest } => {
            if current == latest {
                println!("bolide {current} is up to date");
            } else {
                println!("bolide {current} is newer than the latest release ({latest})");
            }
        }
        Outcome::Updated { from, to } => {
            println!("updated bolide {from} -> {to} ({})", exe.display());
        }
    }
    Ok(())
}
