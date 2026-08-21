//! Update check: is a newer release published than the one that's running?
//!
//! Nothing here downloads or installs anything. The check asks GitHub for the
//! newest published release and reports whether it is ahead of the version
//! passed in; the UI decides how to say so, and sends the user to the release
//! page to fetch it themselves.
//!
//! This is the only outbound network request pigtail makes, so it is opt-out
//! (`settings.check_updates`) and it never blocks the UI — see [`spawn_check`].

use crate::wake::Wake;
use crossbeam_channel::Receiver;
use std::time::Duration;

/// The repository releases are published from.
const GITHUB_REPO: &str = "rustypig91/pigtail-serial-console";

/// Fallback download target, used when the API response carries no page link.
pub const RELEASES_PAGE_URL: &str =
    "https://github.com/rustypig91/pigtail-serial-console/releases/latest";

/// Cap on the whole request. A check that can't finish promptly is not worth
/// keeping a thread — and possibly the process — alive for.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The newest release GitHub knows about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestRelease {
    /// The release tag exactly as published, e.g. `v0.2.0`.
    pub version: String,
    /// The page to open to download it.
    pub url: String,
}

/// What a finished check produces: the newest release, or why we can't tell.
/// The error is a ready-to-show sentence, because displaying it in a dialog is
/// the only thing anyone does with it.
pub type CheckResult = Result<LatestRelease, String>;

/// Split a `X.Y.Z` version into comparable numbers, tolerating a leading `v`
/// and discarding any pre-release/build suffix (`0.1.0-rc1`, `0.1.0+git`).
fn semver_triple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// True when `latest` is worth telling someone running `current` about.
///
/// Both sides are normally clean `X.Y.Z` tags, and then this is a numeric
/// comparison: *newer*, not merely *different*, so a locally-built version that
/// runs ahead of the published tag is left alone, and a pre-release of the same
/// version as the published one (`0.2.0-rc1` vs `v0.2.0`) does not count as an
/// update either. If either side doesn't parse we fall back to treating any
/// difference as newer, which is safe because the API only ever hands us the
/// newest published release.
pub fn is_newer(current: &str, latest: &str) -> bool {
    if latest.trim().is_empty() {
        return false;
    }
    match (semver_triple(current), semver_triple(latest)) {
        (Some(cur), Some(new)) => new > cur,
        _ => current.trim().trim_start_matches('v') != latest.trim().trim_start_matches('v'),
    }
}

/// What the UI should say once a check has finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// A newer release exists. The UI offers to open `url` and to skip
    /// `version`.
    Available { version: String, url: String },
    /// Nothing newer is published.
    UpToDate,
    /// The check couldn't complete; the string is why.
    Failed(String),
}

/// Decide what to say about a finished check — the whole notification policy.
///
/// `manual` marks the explicit "check for updates" action: it always produces a
/// notice, and it ignores `skipped` because asking is a clear signal the user
/// wants to know. The startup check stays quiet (`None`) unless there is a new
/// release the user hasn't already skipped: nobody wants "up to date" or a
/// network error thrown at them for a check they didn't ask for.
pub fn notice_for(
    result: CheckResult,
    current: &str,
    skipped: Option<&str>,
    manual: bool,
) -> Option<Notice> {
    let latest = match result {
        Ok(latest) => latest,
        Err(e) => return manual.then_some(Notice::Failed(e)),
    };

    if !is_newer(current, &latest.version) {
        return manual.then_some(Notice::UpToDate);
    }
    // A skip silences this release until something newer than it is published.
    if !manual && skipped == Some(latest.version.as_str()) {
        return None;
    }
    Some(Notice::Available {
        version: latest.version,
        url: latest.url,
    })
}

/// Ask GitHub for the newest release. Blocks; call it off the UI thread.
pub fn fetch_latest() -> CheckResult {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let mut resp = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        // The API rejects requests with no User-Agent outright.
        .header(
            "User-Agent",
            concat!("pigtail/", env!("CARGO_PKG_VERSION")),
        )
        .config()
        .timeout_global(Some(TIMEOUT))
        // Statuses are inspected below: a 404 here means "no release published
        // yet", which is a different thing to say than a transport failure.
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;

    match resp.status().as_u16() {
        200 => {}
        404 => return Err("No release has been published yet.".into()),
        403 | 429 => return Err("GitHub is rate-limiting update checks — try again later.".into()),
        other => return Err(format!("GitHub returned HTTP {other}.")),
    }

    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Could not read GitHub's response: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Unexpected response from GitHub: {e}"))?;

    let version = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if version.is_empty() {
        return Err("Could not determine the latest version.".into());
    }
    let url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(RELEASES_PAGE_URL);

    Ok(LatestRelease {
        version: version.to_string(),
        url: url.to_string(),
    })
}

/// Run [`fetch_latest`] on a background thread. The single result arrives on the
/// returned channel; `wake` brings an idle UI back to read it. Dropping the
/// receiver is fine — the send simply fails and the thread exits.
pub fn spawn_check(wake: Wake) -> Receiver<CheckResult> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(fetch_latest());
        wake.signal();
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_minor_and_major_are_updates() {
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("0.9.9", "v1.0.0"));
    }

    #[test]
    fn same_or_older_is_not_an_update() {
        assert!(!is_newer("0.1.0", "v0.1.0"));
        assert!(!is_newer("0.2.0", "v0.1.0"));
        assert!(!is_newer("1.0.0", "v0.9.9"));
        // A leading `v` on either side must not make them look different.
        assert!(!is_newer("v0.1.0", "0.1.0"));
    }

    #[test]
    fn prerelease_of_the_published_version_is_not_an_update() {
        assert!(!is_newer("0.2.0-rc1", "v0.2.0"));
        assert!(is_newer("0.2.0-rc1", "v0.2.1"));
    }

    #[test]
    fn missing_or_unparseable_latest() {
        assert!(!is_newer("0.1.0", ""));
        assert!(!is_newer("0.1.0", "   "));
        // Unparseable tags fall back to "different means newer".
        assert!(is_newer("0.1.0", "nightly"));
        assert!(!is_newer("nightly", "nightly"));
    }

    fn release(version: &str) -> CheckResult {
        Ok(LatestRelease {
            version: version.to_string(),
            url: format!("https://example.invalid/{version}"),
        })
    }

    #[test]
    fn startup_check_announces_a_new_release() {
        let notice = notice_for(release("v0.2.0"), "0.1.0", None, false);
        assert_eq!(
            notice,
            Some(Notice::Available {
                version: "v0.2.0".into(),
                url: "https://example.invalid/v0.2.0".into(),
            })
        );
    }

    #[test]
    fn startup_check_is_silent_when_there_is_nothing_to_say() {
        // Up to date, and failures, must not interrupt a launch.
        assert_eq!(notice_for(release("v0.1.0"), "0.1.0", None, false), None);
        assert_eq!(
            notice_for(Err("Could not reach GitHub.".into()), "0.1.0", None, false),
            None
        );
    }

    #[test]
    fn a_skipped_release_is_not_announced_again_at_startup() {
        assert_eq!(
            notice_for(release("v0.2.0"), "0.1.0", Some("v0.2.0"), false),
            None
        );
    }

    #[test]
    fn skipping_one_release_does_not_silence_the_next() {
        let notice = notice_for(release("v0.3.0"), "0.1.0", Some("v0.2.0"), false);
        assert!(matches!(notice, Some(Notice::Available { .. })));
    }

    #[test]
    fn manual_check_always_reports_and_ignores_a_skip() {
        assert_eq!(
            notice_for(release("v0.1.0"), "0.1.0", None, true),
            Some(Notice::UpToDate)
        );
        assert_eq!(
            notice_for(Err("Offline.".into()), "0.1.0", None, true),
            Some(Notice::Failed("Offline.".into()))
        );
        // Asking explicitly overrides a previous "skip this version".
        let notice = notice_for(release("v0.2.0"), "0.1.0", Some("v0.2.0"), true);
        assert!(matches!(notice, Some(Notice::Available { .. })));
    }
}
