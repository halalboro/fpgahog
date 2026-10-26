use std::fs;
use std::io;
use std::path::Path;
use chrono;
use chrono::{DateTime, Local};

/// fpgahog's own state directory. It runs next to the cluster's hosthog rather than replacing
/// it, and hosthog cannot read fpgahog's newer statefile format.
pub const STATE_PATH: &str = "/var/lib/fpgahog";

pub fn prog() -> String {
    std::env::current_exe()
        .ok()
        .expect("Cant look up your binary name.")
        .to_str()
        .expect("Your binary name looks very unexpected")
        .to_owned()
}

pub fn prog_name() -> String {
    std::env::current_exe()
        .ok()
        .expect("Cant look up your binary name.")
        .file_name()
        .expect("Your binary does not seem to have a name.")
        .to_str()
        .expect("Your binary name looks very unexpected.")
        .to_owned()
}

pub fn remove_dir_contents<P: AsRef<Path>>(path: P) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        fs::remove_file(entry?.path())?;
    }
    Ok(())
}

pub fn format_timeout_abs(timeout: DateTime<Local>) -> String {
    let now = DateTime::from(Local::now());
    let duration = timeout - now;
    format_timeout(duration)
}

pub fn format_timeout(duration: chrono::Duration) -> String {
    // Pick the unit from the magnitude but keep the sign, so a claim that is already past
    // its timeout reads as "-2h" until the next maintenance pass removes it, rather than as
    // several thousand negative seconds.
    let seconds = duration.num_seconds().abs();

    if seconds < 60 {
        return format!("{}s", duration.num_seconds());
    }
    if seconds < 60 * 60 {
        return format!("{}m", duration.num_minutes());
    }
    if seconds < 24 * 60 * 60 {
        return format!("{}h", duration.num_hours());
    }
    if seconds < 7 * 24 * 60 * 60 {
        return format!("{}d", duration.num_days());
    }
    if seconds < 365 * 24 * 60 * 60 {
        return format!("{}w", duration.num_weeks());
    }
    // hosthog's ladder stopped at four weeks and fell through to unreachable!(), so `status`
    // panicked outright on any reservation lasting longer than a month -- which is exactly
    // the long-running-project case the tool is meant to serve.
    format!("{}y", duration.num_days() / 365)
}

pub fn get_username(uid: u32) -> String {
    let passwd = unsafe { libc::getpwuid(uid) };
    if passwd.is_null() {
        return "<unknown>".to_string();
    }
    let passwd = unsafe { &*passwd };
    let name = unsafe { std::ffi::CStr::from_ptr(passwd.pw_name) };
    return name.to_str().unwrap().to_string();
}

pub fn get_uid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let passwd = unsafe { libc::getpwnam(cname.as_ptr()) };
    if passwd.is_null() {
        return None;
    }
    return Some(unsafe { (*passwd).pw_uid });
}

pub fn get_gid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let group = unsafe { libc::getgrnam(cname.as_ptr()) };
    if group.is_null() {
        return None;
    }
    return Some(unsafe { (*group).gr_gid });
}

/// Where `just install` pins fpgahog: host-local, stable across rebuilds, safe from GC.
pub fn installed_binary() -> std::path::PathBuf {
    Path::new(STATE_PATH).join("pkg/bin/fpgahog")
}

/// The fpgahog that runs later, from an `at` job or a locked-out user's ssh login. The running
/// binary is often target/release/fpgahog, which a rebuild or `cargo clean` can take away
/// before the job fires, so an installed copy wins whenever there is one.
pub fn command_for_later(installed: &Path, running: &Path) -> std::path::PathBuf {
    if installed.is_file() {
        installed.to_path_buf()
    } else {
        running.to_path_buf()
    }
}

pub fn prog_for_later() -> String {
    let installed = installed_binary();
    let running = prog();
    if !installed.is_file() {
        return running;
    }
    // Only an *older* install is skipped: it cannot read the statefile this version writes.
    // A newer one reads it fine, and preferring it keeps the stable path. Every rejection is
    // announced, because the binary we fall back to is exactly the one that may not last.
    match installed_version(&installed) {
        Some(version) if !is_older(&version, env!("CARGO_PKG_VERSION")) => {
            command_for_later(&installed, Path::new(&running)).to_string_lossy().into_owned()
        }
        Some(version) => {
            eprintln!(
                "WARN: the pinned install at {} is fpgahog {}, older than this {}, so it cannot read the state this writes. Falling back to {}; run `just install`.",
                installed.display(), version, env!("CARGO_PKG_VERSION"), running
            );
            running
        }
        None => {
            eprintln!(
                "WARN: {} would not report a version, so it is not trusted to run scheduled work. Falling back to {}; run `just install`.",
                installed.display(), running
            );
            running
        }
    }
}

/// What the installed copy calls itself. It is asked rather than compared by path, because
/// `just install` replaces a symlink, not the file behind it.
fn installed_version(installed: &Path) -> Option<String> {
    let output = std::process::Command::new(installed).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .map(str::to_string)
}

/// Dotted versions compared numerically, so 0.10.0 is not older than 0.9.0.
fn is_older(version: &str, than: &str) -> bool {
    fn parts(version: &str) -> Vec<u64> {
        version
            .split('.')
            .map(|part| part.trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0))
            .collect()
    }
    parts(version) < parts(than)
}

/// Logger candidates for the scheduled job, by absolute path and most specific first.
const LOGGER_PATHS: [&str; 3] = [
    "/run/current-system/sw/bin/logger",
    "/usr/bin/logger",
    "/bin/logger",
];

/// The redirection a scheduled `maintenance` run appends to its command line, so `at` does not
/// mail root every five minutes. Falls back to a file where the host has no `logger`.
pub fn maintenance_sink() -> String {
    maintenance_sink_from(&LOGGER_PATHS, STATE_PATH)
}

fn maintenance_sink_from(loggers: &[&str], state_path: &str) -> String {
    match loggers.iter().find(|path| Path::new(path).is_file()) {
        Some(logger) => format!("2>&1 | {} -t fpgahog", logger),
        None => format!(">> {}/maintenance.log 2>&1", state_path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn root_group_resolves() {
        assert_eq!(get_gid("root"), Some(0));
        assert_eq!(get_gid("no-such-group-fpgahog"), None);
    }

    #[test]
    fn every_magnitude_formats_without_panicking() {
        assert_eq!(format_timeout(Duration::seconds(5)), "5s");
        assert_eq!(format_timeout(Duration::minutes(5)), "5m");
        assert_eq!(format_timeout(Duration::hours(5)), "5h");
        assert_eq!(format_timeout(Duration::days(3)), "3d");
        assert_eq!(format_timeout(Duration::days(14)), "2w");
    }

    /// Regression: a claim more than four weeks out fell off the end of the ladder and hit
    /// unreachable!(), killing `status` for everyone on the host.
    #[test]
    fn long_reservations_do_not_panic() {
        assert_eq!(format_timeout(Duration::days(60)), "8w");
        assert_eq!(format_timeout(Duration::days(400)), "1y");
        assert_eq!(format_timeout(Duration::days(365 * 4)), "4y");
    }

    /// Expired claims are shown until maintenance removes them, so negative durations must
    /// pick a sensible unit too.
    #[test]
    fn expired_claims_read_naturally() {
        assert_eq!(format_timeout(Duration::seconds(-30)), "-30s");
        assert_eq!(format_timeout(Duration::hours(-2)), "-2h");
        assert_eq!(format_timeout(Duration::days(-10)), "-1w");
    }
}

#[cfg(test)]
mod later_tests {
    use super::*;

    /// Regression: the `at` job that ends a claim called the binary it was scheduled from,
    /// often target/release/fpgahog, so after a `cargo clean` a claim, and a hog, never ended.
    #[test]
    fn an_installed_binary_is_preferred_for_later() {
        let dir = std::env::temp_dir().join(format!("fpgahog-later-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let installed = dir.join("fpgahog");
        std::fs::write(&installed, "").unwrap();
        let running = Path::new("/scratch/someone/fpgahog/target/release/fpgahog");
        assert_eq!(command_for_later(&installed, running), installed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_an_install_the_running_binary_is_used() {
        let running = Path::new("/scratch/someone/fpgahog/target/release/fpgahog");
        assert_eq!(command_for_later(Path::new("/nonexistent/fpgahog"), running), running);
    }

    #[test]
    fn versions_are_compared_numerically() {
        assert!(is_older("0.4.0", "0.5.0"));
        assert!(!is_older("0.5.0", "0.5.0"));
        assert!(!is_older("0.5.1", "0.5.0"));
        // A newer install is kept, so the ordering must not be lexicographic.
        assert!(!is_older("0.10.0", "0.9.0"));
        assert!(is_older("0.9.0", "0.10.0"));
    }

    #[test]
    fn the_scheduled_job_names_its_logger_absolutely() {
        let dir = std::env::temp_dir().join("fpgahog-sink-test");
        std::fs::create_dir_all(&dir).unwrap();
        let logger = dir.join("logger");
        std::fs::write(&logger, "").unwrap();
        let found = maintenance_sink_from(&[logger.to_str().unwrap()], "/var/lib/fpgahog");
        assert_eq!(found, format!("2>&1 | {} -t fpgahog", logger.display()));
        assert!(!found.contains("PATH"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_logger_the_job_appends_to_a_file() {
        assert_eq!(
            maintenance_sink_from(&["/nonexistent/logger"], "/var/lib/fpgahog"),
            ">> /var/lib/fpgahog/maintenance.log 2>&1"
        );
    }
}
