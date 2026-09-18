use nix;
use crate::cable;
use crate::claims;
use crate::diskstate;
use crate::diskstate::ResourceId;
use crate::fpga;
use crate::systemd_units;
use crate::users;
use once_cell::sync::Lazy;
use crate::util;
use std::fs;

static OVERLAY_PATH: Lazy<String> = Lazy::new(|| format!("{}/overlay", util::STATE_PATH));

pub fn ssh_hogged_message(claim: &diskstate::Claim) -> String {
    let duration = util::format_timeout_abs(claim.timeout);
    vec![
        format!("This system has been hogged by {}.", claim.user),
        format!("Comment: {}", claim.comment),
        format!("This claim will time out in {}.", duration),
    ].join("\n")
}

fn ssh_hogged_command() -> String {
    format!("sudo {} status", util::prog_for_later())
}

fn escape(input: &str) -> String {
    input.chars().map(|c| match c {
        '/' => String::from("_"),
        '_' => String::from("__"),
        _ => format!("{}", c),
    }).collect()
}

/// Bind-mount a restricted copy of `file` over it. Err(None) means there is no such file,
/// Err(Some(..)) a real failure. Nothing here panics: a panic part-way through a hog would leave
/// the mounts made so far unrecorded.
fn overmount(file: &str) -> Result<(), Option<String>> {
    if !std::path::Path::new(file).is_file() {
        return Err(None);
    }

    let authorized_keys = fs::read_to_string(file)
        .map_err(|e| Some(format!("cannot read {}: {}", file, e)))?;
    let command = ssh_hogged_command();
    let overlay_keys = authorized_keys.lines().map(|line|
        if !line.is_empty() {
            format!("restrict,command=\"{}\" {}", command, line)
        } else {
            String::from(line)
        }
    ).collect::<Vec<String>>().join("\n");
    let overlay_file = format!("{}/{}", OVERLAY_PATH.as_str(), escape(file));
    fs::create_dir_all(OVERLAY_PATH.as_str())
        .map_err(|e| Some(format!("cannot create {}: {}", OVERLAY_PATH.as_str(), e)))?;
    fs::write(overlay_file.as_str(), overlay_keys)
        .map_err(|e| Some(format!("cannot write {}: {}", overlay_file, e)))?;

    nix::mount::mount(
        Some(overlay_file.as_str()),
        file,
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| Some(format!("cannot mount over {}: {}", file, e)))?;

    Ok(())
}

#[derive(Debug)]
pub struct User {
    pub name: String,
    pub home: String,
}

fn list_users() -> Vec<User> {
    let mut users = vec![];

    loop {
        // safe because we null check before accessing it
        let passwd = unsafe {
            let passwd = libc::getpwent();
            if passwd.is_null() { break };
            *passwd
        };
        if passwd.pw_dir.is_null() { continue };
        // safe because we null check before accessing it
        let home = unsafe { std::ffi::CStr::from_ptr(passwd.pw_dir).to_string_lossy().into_owned() };
        // safe because we null check before accessing it
        if passwd.pw_name.is_null() { continue };
        let name = unsafe { std::ffi::CStr::from_ptr(passwd.pw_name).to_string_lossy().into_owned() };

        users.push(User { name, home });
    }

    // safe because i dont know what might be unsafe about it
    unsafe { libc::endpwent() };

    return users;
}

fn hog_ssh(exclude_users: Vec<String>, state: &mut diskstate::DiskState) {
    let users = list_users().into_iter().filter(|u| !exclude_users.contains(&u.name)).collect::<Vec<User>>();
    let all_auth_key_files: Vec<String> = diskstate::expand_authorized_keys_file(&state.settings, users);
    let all_files_len = all_auth_key_files.len();
    let auth_key_files: Vec<String> = 
        all_auth_key_files.into_iter()
        // filter out files that we recorded as overmounted
        .filter(|f| !state.overmounts.contains(f))
        // filter out files that have an unknown overmount
        .filter(|f| !is_overmounted(f)).collect();
    let mut locked = 0;
    let mut absent = 0;
    let mut failed = 0;
    for file in &auth_key_files {
        match overmount(&file) {
            Ok(_) => { locked += 1; state.overmounts.push(file.clone()); },
            Err(None) => { absent += 1; }, // ignore files that dont exist
            Err(Some(err)) => { failed += 1; println!("WARN: {}", err); },
        }
    }
    // Report what actually happened. hosthog printed the number of candidate paths here, which
    // read "162 users locked out of ssh" on a run that had locked out nobody.
    println!("{} authorized_keys files locked ({} already locked, {} do not exist, {} failed)", locked, all_files_len - auth_key_files.len(), absent, failed);
    warn_if_sshd_reads_elsewhere(&state.settings);
    if state.overmounts.is_empty() {
        println!("WARN: no key file is locked, so nobody has been locked out of ssh.");
    }
}

/// Hogging the host means taking everything on it, FPGAs included.
pub fn do_hog(mut users: Vec<String>, force: bool, state: &mut diskstate::DiskState) {
    let me = match users::my_username() {
        Some(me) => me,
        None => {
            eprintln!("Cannot determine your username, refusing to hog.");
            std::process::exit(1);
        }
    };
    let claim_index = match state
        .claims
        .iter()
        .position(|c| c.user == me && c.exclusive && c.covers(&ResourceId::Host))
    {
        Some(index) => index,
        None => {
            eprintln!(
                "Hogging not allowed. Claim exclusive access first: {} claim host <timeout> --exclusive",
                util::prog_name()
            );
            std::process::exit(1);
        }
    };

    // Sanity check:
    if let Some(hogger) = &state.hogger {
        if hogger.user != me {
            eprintln!("Hogging not allowed. The host is already hogged by {}.", hogger.user);
            std::process::exit(1);
        }
    }

    // Pull every configured FPGA into the claim, but never silently over somebody else.
    let wanted: Vec<ResourceId> = state
        .settings
        .aliases()
        .into_iter()
        .map(ResourceId::Fpga)
        .collect();
    let conflicts = claims::find_conflicts(state, &me, &wanted, true);
    if !conflicts.is_empty() {
        eprintln!("Cannot hog: these FPGAs are claimed by someone else:");
        for conflict in &conflicts {
            eprintln!(
                "  {:<10} {} for another {}",
                conflict.resource.to_string(),
                conflict.user,
                util::format_timeout_abs(conflict.until)
            );
        }
        eprintln!("Wait for them to expire, or ask. Nothing was changed.");
        std::process::exit(1);
    }
    for resource in wanted {
        if !state.claims[claim_index].resources.contains(&resource) {
            state.claims[claim_index].resources.push(resource);
        }
    }
    let claim = state.claims[claim_index].clone();

    // `me` is moved into the exclusion list below, so resolve the uid first.
    let me_uid = util::get_uid(&me);

    println!("hog users:");
    if users.len() == 0 {
        users.push(String::from("root"));
        users.push(me);
    }
    users.as_slice().into_iter().for_each(|i| print!("{} ", i));
    println!("");
    apply_hog(
        state,
        claim,
        |state| hog_ssh(users, state),
        diskstate::store,
        systemd_units::disable_resource,
        |state| {
            fpga::sync_locks(state);
            claims::keep_locks_asserted(state);
            // A hog takes every board. Cutting other people off their cables is disruptive, so
            // like `claim` it happens only when asked for.
            let boards = state.settings.fpgas.clone();
            let held = cable::foreign_cable_holders(&boards, me_uid);
            if force {
                cable::evict_held(state, &held);
            } else if !held.is_empty() {
                cable::report_foreign_holders(&held);
                println!("These keep working until they close the cable. Re-run `hog --force` to cut them off.");
            }
            fpga::sync_locks(state);
        },
    );
}

/// The side effects of a hog, in the order they must happen. Taking them as arguments lets the
/// ordering be tested without mounting anything.
fn apply_hog(
    state: &mut diskstate::DiskState,
    claim: diskstate::Claim,
    lock_ssh: impl FnOnce(&mut diskstate::DiskState),
    persist: impl FnOnce(&diskstate::DiskState),
    stop_units: impl FnOnce(&mut diskstate::DiskState) -> Result<(), String>,
    lock_boards: impl FnOnce(&mut diskstate::DiskState),
) {
    lock_ssh(state);
    state.hogger = Some(claim);
    // Write the lockout down before anything that can fail. Mounts nobody recorded cannot be
    // released, and keep everyone out of ssh until a reboot.
    persist(state);
    if let Err(err) = stop_units(state) {
        println!("WARN: {}. ssh is hogged regardless, and `release` undoes it.", err);
    }
    lock_boards(state);
}

pub fn release_ssh(state: &mut diskstate::DiskState) {
    let mut overmounts: Vec<String> = vec![];
    for file in &state.overmounts {
        let path = std::path::Path::new(file);
        if !is_overmounted(file) {
            continue;
        }
        match nix::mount::umount(path) {
            Err(err) => {
                println!("failed to release {}: {:?}", file, err);
                overmounts.push(file.clone());
            },
            Ok(_) => {
                println!("released {}", file);
            },
        }
    }
    // Only a host that was actually hogged has an overlay directory. Warning about its
    // absence on every plain `release` is just noise.
    if std::path::Path::new(OVERLAY_PATH.as_str()).is_dir() {
        if let Err(err) = util::remove_dir_contents(OVERLAY_PATH.as_str()) {
            println!("WARN: could not remove overlayed files: {}", err);
        }
    }
    state.overmounts = overmounts;
}

/// Undo the host-wide part of a hog: ssh keys and systemd units. Always safe to call, even
/// when we believe the host is not hogged, so that we converge on the intended state.
pub fn release_host(state: &mut diskstate::DiskState) {
    release_ssh(state);
    if let Some(hogger) = state.hogger.clone() {
        state.claims.retain(|claim| claim.id != hogger.id);
        state.hogger = None;
    }
    if let Err(err) = systemd_units::enable_resource(state) {
        println!("WARN: {}. They stay recorded, so the next release retries them.", err);
    }
}

/// Give back resources. With no selection, everything the caller holds.
pub fn do_release(selected: Vec<String>, state: &mut diskstate::DiskState) {
    let me = match users::my_username() {
        Some(me) => me,
        None => {
            eprintln!("Cannot determine your username, refusing to release.");
            std::process::exit(1);
        }
    };

    let selected: Vec<ResourceId> = selected
        .iter()
        .flat_map(|text| ResourceId::parse_list(text))
        .collect();
    let release_everything = selected.is_empty();
    let releasing_host = release_everything || selected.contains(&ResourceId::Host);

    let released = release_claims(&mut state.claims, &me, &selected);

    // The hogger may have been dropped above; the host-wide undo is idempotent either way.
    // Match on id, not on content: a claim we just trimmed a resource out of is still the
    // same claim, and treating it as a different one would leave the host hogged for good.
    let hogger_is_gone = match &state.hogger {
        Some(hogger) => !state.claims.iter().any(|claim| claim.id == hogger.id),
        None => true,
    };
    if releasing_host && hogger_is_gone {
        release_host(state);
    }

    // Hand the device nodes back.
    fpga::sync_locks(state);

    if released.is_empty() {
        println!("Nothing to release (you hold no matching claims).");
    } else {
        println!("Released: {}", released.join(", "));
    }
}

/// Drop or trim `me`'s claims, returning the resources given back. With nothing selected this
/// is hosthog's `release`: your exclusive claims go (the one behind a hog among them) and plain
/// announcements stay. Naming resources gives them back whatever kind of claim holds them.
fn release_claims(claims: &mut Vec<diskstate::Claim>, me: &str, selected: &[ResourceId]) -> Vec<String> {
    let mut released: Vec<String> = vec![];
    if selected.is_empty() {
        for claim in claims.iter().filter(|c| c.user == me && c.exclusive) {
            for resource in &claim.resources {
                let name = resource.to_string();
                if !released.contains(&name) {
                    released.push(name);
                }
            }
        }
        claims.retain(|claim| !(claim.user == me && claim.exclusive));
    } else {
        let mut trimmed = false;
        for claim in claims.iter_mut().filter(|c| c.user == me) {
            let before = claim.resources.len();
            claim.resources.retain(|r| !selected.contains(r));
            if claim.resources.len() != before {
                trimmed = true;
            }
        }
        if trimmed {
            released.push(
                selected
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<String>>()
                    .join(","),
            );
        }
        // A claim that covers nothing any more is not a claim.
        claims.retain(|claim| !claim.resources.is_empty());
    }
    released
}

/// Whether `file` is currently a mount point. The mount point field is compared exactly: the
/// substring match hosthog used reports `/etc/ssh/authorized_keys.d/chris` as mounted whenever
/// `.../christianK` is, and rose has both that pair and `martin`/`martinLi`.
pub fn is_overmounted(file: &str) -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").expect("Cant read /proc/mounts");
    return mount_points_contain(&mounts, file);
}

fn mount_points_contain(mounts: &str, file: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .any(|point| unescape_mount_path(point) == file)
}

/// /proc/mounts writes whitespace and backslashes in paths as octal escapes.
fn unescape_mount_path(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Key patterns sshd reads that `configured` does not cover, from `sshd -T` output. sshd takes a
/// relative path as relative to the user's home, and `none` as no file at all.
fn uncovered_key_patterns(sshd_config: &str, configured: &[String]) -> Vec<String> {
    let mut uncovered = vec![];
    for line in sshd_config.lines() {
        let mut words = line.split_whitespace();
        // Older OpenSSH prints `authorizedkeysfile`; current releases, including rose's, print
        // `AuthorizedKeysFile`. A case-sensitive match silently disabled this whole check.
        match words.next() {
            Some(keyword) if keyword.eq_ignore_ascii_case("authorizedkeysfile") => {}
            _ => continue,
        }
        for pattern in words {
            if pattern == "none" {
                continue;
            }
            let pattern = if pattern.starts_with('/') || pattern.starts_with('%') {
                pattern.to_string()
            } else {
                format!("%h/{}", pattern)
            };
            if !configured.contains(&pattern) && !uncovered.contains(&pattern) {
                uncovered.push(pattern);
            }
        }
    }
    uncovered
}

/// A hog can report success and lock out nobody: on rose sshd reads only
/// /etc/ssh/authorized_keys.d/%u, so a key list without that entry does nothing at all.
fn warn_if_sshd_reads_elsewhere(settings: &diskstate::Settings) {
    let output = match std::process::Command::new("sshd").arg("-T").output() {
        Ok(output) if output.status.success() => output,
        _ => return, // no sshd to ask, so nothing to compare against
    };
    let config = String::from_utf8_lossy(&output.stdout);
    for pattern in uncovered_key_patterns(&config, &settings.authorized_keys_file) {
        println!(
            "WARN: sshd reads keys from {} but settings.authorized_keys_file does not cover it, so logins that way still work.",
            pattern
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "zroot/root/nixos /nix/store zfs ro 0 0
tmpfs /etc/ssh/authorized_keys.d/christianK tmpfs rw 0 0
nfs:/export/home /home/some\\040one/.ssh/authorized_keys nfs4 rw 0 0
";

    /// Regression: `chris` read as mounted because `christianK` was.
    #[test]
    fn a_prefix_of_a_mounted_name_is_not_itself_mounted() {
        assert!(mount_points_contain(MOUNTS, "/etc/ssh/authorized_keys.d/christianK"));
        assert!(!mount_points_contain(MOUNTS, "/etc/ssh/authorized_keys.d/chris"));
    }

    #[test]
    fn escaped_whitespace_in_mount_points_still_matches() {
        assert!(mount_points_contain(MOUNTS, "/home/some one/.ssh/authorized_keys"));
    }

    /// Regression: with only `%h/.ssh/authorized_keys` configured, a hog on rose locked out
    /// nobody, because sshd there reads /etc/ssh/authorized_keys.d/%u alone. These are the
    /// lines rose's `sshd -T` actually prints: CamelCase, with a similarly named keyword that
    /// must not be mistaken for the key file setting.
    #[test]
    fn a_key_path_sshd_reads_but_hog_skips_is_reported() {
        let sshd = "Port 22\nAuthorizedKeysCommand none\nAuthorizedKeysCommandUser none\nAuthorizedKeysFile /etc/ssh/authorized_keys.d/%u\n";
        let partial = vec![String::from("%h/.ssh/authorized_keys")];
        assert_eq!(uncovered_key_patterns(sshd, &partial), vec!["/etc/ssh/authorized_keys.d/%u"]);

        let defaults = vec![
            String::from("%h/.ssh/authorized_keys"),
            String::from("/etc/ssh/authorized_keys.d/%u"),
        ];
        assert!(uncovered_key_patterns(sshd, &defaults).is_empty());
    }

    #[test]
    fn relative_and_none_patterns_follow_sshd_semantics() {
        let sshd = "authorizedkeysfile .ssh/authorized_keys none\n";
        assert_eq!(uncovered_key_patterns(sshd, &[]), vec!["%h/.ssh/authorized_keys"]);
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;
    use crate::diskstate::Claim;
    use chrono::Local;

    fn claim(user: &str, resources: &str, exclusive: bool) -> Claim {
        Claim {
            id: 0,
            timeout: Local::now() + chrono::Duration::hours(1),
            soft_timeout: None,
            exclusive,
            user: user.into(),
            comment: String::new(),
            resources: ResourceId::parse_list(resources),
        }
    }

    /// hosthog's bare `release` takes back your hogs and exclusive claims and leaves plain
    /// announcements alone, as its help text says. fpgahog used to drop every claim you held.
    #[test]
    fn bare_release_keeps_shared_claims_like_hosthog() {
        let mut claims = vec![
            claim("me", "host", true),
            claim("me", "u280", false),
            claim("other", "v80", true),
        ];
        let released = release_claims(&mut claims, "me", &[]);
        assert_eq!(released, vec!["host"]);
        assert_eq!(claims.len(), 2);
        assert!(claims.iter().any(|c| c.user == "me" && !c.exclusive && c.covers_alias("u280")));
        assert!(claims.iter().any(|c| c.user == "other"));
    }

    /// Existing behaviour, kept: naming a resource gives it back whatever kind of claim holds it.
    #[test]
    fn naming_a_resource_releases_it_even_from_a_shared_claim() {
        let mut claims = vec![claim("me", "host,u280", true), claim("me", "v80", false)];
        let released = release_claims(&mut claims, "me", &[ResourceId::parse("u280"), ResourceId::parse("v80")]);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].resources, vec![ResourceId::Host]);
        assert!(!released.is_empty());
    }
}

#[cfg(test)]
mod hog_order_tests {
    use super::*;
    use std::cell::RefCell;

    /// Regression: key files were bind-mounted first and state was saved only when the whole
    /// command finished, so a systemd error in between left mounts nobody had recorded, which
    /// `release` could not undo. The hog must be written down before anything that can fail.
    #[test]
    fn a_hog_is_recorded_before_anything_that_can_fail() {
        let log = RefCell::new(Vec::<String>::new());
        let mut state = diskstate::load_default();
        let claim = diskstate::Claim {
            id: 1,
            timeout: chrono::Local::now(),
            soft_timeout: None,
            exclusive: true,
            user: "me".into(),
            comment: String::new(),
            resources: vec![ResourceId::Host],
        };
        apply_hog(
            &mut state,
            claim,
            |state| {
                state.overmounts.push("/etc/ssh/authorized_keys.d/theo".into());
                log.borrow_mut().push("ssh".into());
            },
            |state| {
                log.borrow_mut().push(format!(
                    "persist overmounts={} hogged={}",
                    state.overmounts.len(),
                    state.hogger.is_some()
                ))
            },
            |_| {
                log.borrow_mut().push("units".into());
                Err(String::from("system bus unreachable"))
            },
            |_| log.borrow_mut().push("boards".into()),
        );
        assert_eq!(*log.borrow(), vec!["ssh", "persist overmounts=1 hogged=true", "units", "boards"]);
        assert!(state.hogger.is_some());
    }
}
