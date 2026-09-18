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

/// Where `just install` pins fpgahog: host-local, stable across rebuilds, safe from GC.
pub fn get_gid(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let group = unsafe { libc::getgrnam(cname.as_ptr()) };
    if group.is_null() {
        return None;
    }
    return Some(unsafe { (*group).gr_gid });
}

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
    command_for_later(&installed_binary(), Path::new(&prog()))
        .to_string_lossy()
        .into_owned()
}

/// Whether `path` sits in a cargo build directory.
pub fn is_build_output(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts
        .windows(2)
        .any(|pair| pair[0] == "target" && (pair[1] == "release" || pair[1] == "debug"))
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
    fn cargo_build_outputs_are_recognised() {
        assert!(is_build_output(Path::new("/scratch/someone/fpgahog/target/release/fpgahog")));
        assert!(is_build_output(Path::new("/scratch/someone/fpgahog/target/debug/fpgahog")));
        assert!(!is_build_output(Path::new("/var/lib/fpgahog/pkg/bin/fpgahog")));
        assert!(!is_build_output(Path::new("/nix/store/abc-fpgahog/bin/fpgahog")));
    }
}
