//! FPGA discovery and per-device enforcement.
//!
//! Enforcement here is deliberately *not* the bind-mount trick `hog.rs` uses for ssh keys.
//! hosthog bind-mounts because authorized_keys is persistent configuration where a mistake
//! could lock somebody out for good. Device nodes are the opposite: /dev is devtmpfs and is
//! rebuilt from scratch every boot, so chown+chmod carries none of that risk, and it avoids
//! a hazard bind-mounting would introduce (a node carrying a stale major:minor after a
//! driver reload could route i/o at the wrong device).
//!
//! The flip side is that nothing makes our mode stick: drivers create their nodes 0666
//! themselves and a reload silently undoes enforcement. `sync_locks` is therefore the single
//! convergence point and is expected to run on every maintenance pass.

use crate::diskstate::{DeviceLock, DiskState, FpgaSpec, ResourceId};
use crate::util;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;

/// PCI vendors whose cards we treat as FPGAs worth offering.
const FPGA_PCI_VENDORS: [&str; 2] = [
    "0x10ee", // AMD / Xilinx
    "0x1172", // Intel / Altera
];

const PCI_DEVICES: &str = "/sys/bus/pci/devices";

/// Mode a claimed device node is set to: owner (the claimant) only.
pub const LOCKED_MODE: u32 = 0o600;

/// Driver name -> prefix of the /dev entries it creates. Only ever used as a hint when
/// exactly one discovered board uses that driver; with two, the index in the node name
/// cannot be mapped back to a PCI address and a human has to say which is which.
const DRIVER_DEV_HINTS: [(&str, &str); 5] = [
    ("coyote_driver", "coyote_fpga_"),
    ("xdma", "xdma"),
    ("ami", "ami"),
    ("xocl", "xocl"),
    ("xclmgmt", "xclmgmt"),
];

#[derive(Debug, Clone)]
pub struct DiscoveredFpga {
    pub bdf: String,
    pub pci_id: String,
    pub driver: Option<String>,
    pub numa_node: Option<String>,
}

fn read_trim(path: &std::path::Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn strip_0x(value: &str) -> String {
    value.trim_start_matches("0x").to_string()
}

/// Every FPGA-looking PCI function on this host, ordered by PCI address.
pub fn discover() -> Vec<DiscoveredFpga> {
    let mut found = vec![];
    let entries = match fs::read_dir(PCI_DEVICES) {
        Ok(entries) => entries,
        Err(err) => {
            println!("WARN: cannot read {}: {}", PCI_DEVICES, err);
            return found;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let vendor = match read_trim(&path.join("vendor")) {
            Some(vendor) => vendor,
            None => continue,
        };
        if !FPGA_PCI_VENDORS.contains(&vendor.as_str()) {
            continue;
        }
        let device = read_trim(&path.join("device")).unwrap_or_default();
        let driver = fs::read_link(path.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        found.push(DiscoveredFpga {
            bdf: entry.file_name().to_string_lossy().into_owned(),
            pci_id: format!("{}:{}", strip_0x(&vendor), strip_0x(&device)),
            driver,
            numa_node: read_trim(&path.join("numa_node")),
        });
    }

    found.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    return found;
}

/// /dev entries starting with `prefix`.
fn devices_with_prefix(prefix: &str) -> Vec<String> {
    let mut out = vec![];
    let entries = match fs::read_dir("/dev") {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) {
            out.push(format!("/dev/{}", name));
        }
    }
    out.sort();
    return out;
}

/// The /dev prefix a driver's nodes start with. Coyote names its nodes after the platform
/// its driver serves (`coyote_driver_versal` creates `coyote_versal_fpga_0_v0`), so that family
/// is derived rather than listed; the older single-driver build used `coyote_fpga_`.
fn device_prefix(driver: &str) -> Option<String> {
    if let Some(platform) = driver.strip_prefix("coyote_driver_") {
        return Some(format!("coyote_{}_fpga_", platform));
    }
    DRIVER_DEV_HINTS
        .iter()
        .find(|(name, _)| *name == driver)
        .map(|(_, prefix)| prefix.to_string())
}

/// Best-effort guess of the device nodes belonging to one discovered board. Returns an
/// empty list whenever the answer would be a coin flip.
fn guess_devices(target: &DiscoveredFpga, all: &[DiscoveredFpga]) -> Vec<String> {
    let driver = match &target.driver {
        Some(driver) => driver,
        None => return vec![],
    };
    let prefix = match device_prefix(driver) {
        Some(prefix) => prefix,
        None => return vec![],
    };
    let sharing = all
        .iter()
        .filter(|d| d.driver.as_deref() == Some(driver.as_str()))
        .count();
    if sharing != 1 {
        // More than one card on this driver: the index in the node name says nothing about
        // which PCI address it belongs to.
        return vec![];
    }
    devices_with_prefix(&prefix)
}

/// A placeholder name no configured board is using yet.
fn next_free_alias(taken: &[String]) -> String {
    for index in 0.. {
        let candidate = format!("fpga{}", index);
        if !taken.iter().any(|t| *t == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Merge discovery results into the configured inventory, keeping any alias and device list
/// an admin already set. Returns (added, updated) counts.
pub fn merge_into_settings(state: &mut DiskState, discovered: &[DiscoveredFpga]) -> (usize, usize) {
    let mut added = 0;
    let mut updated = 0;
    for found in discovered.iter() {
        let guessed = guess_devices(found, discovered);
        // Look the board up by index: holding an iter_mut() borrow across the insert in the
        // other branch is exactly the pattern the borrow checker refuses.
        let existing = state.settings.fpgas.iter().position(|f| f.bdf == found.bdf);
        match existing {
            Some(index) => {
                let spec = &mut state.settings.fpgas[index];
                if spec.pci_id != found.pci_id {
                    println!(
                        "NOTE: {} now reports {} (was {}). Check that this is the same card.",
                        spec.alias, found.pci_id, spec.pci_id
                    );
                    spec.pci_id = found.pci_id.clone();
                    updated += 1;
                }
                // Replace the node list only when none of it exists any more, which is what a
                // driver rename or reload looks like. A partly valid list was probably edited by
                // hand, so it is reported rather than overwritten.
                let present = spec
                    .devices
                    .iter()
                    .filter(|d| std::path::Path::new(d.as_str()).exists())
                    .count();
                if present == 0 && !guessed.is_empty() && spec.devices != guessed {
                    if !spec.devices.is_empty() {
                        println!(
                            "NOTE: none of {}'s configured device nodes exist any more. Replacing {:?} with {:?}.",
                            spec.alias, spec.devices, guessed
                        );
                    }
                    spec.devices = guessed;
                    updated += 1;
                } else if present < spec.devices.len() {
                    println!(
                        "NOTE: {} has {} configured device node(s) that do not exist.",
                        spec.alias,
                        spec.devices.len() - present
                    );
                }
            }
            None => {
                let alias = next_free_alias(&state.settings.aliases());
                state.settings.fpgas.push(FpgaSpec {
                    alias,
                    bdf: found.bdf.clone(),
                    pci_id: found.pci_id.clone(),
                    devices: guessed,
                    cables: vec![],
                });
                added += 1;
            }
        }
    }
    (added, updated)
}

#[derive(Debug, Clone)]
pub struct Holder {
    pub pid: u32,
    pub uid: u32,
    pub user: String,
    pub cmdline: String,
    pub device: String,
}

fn proc_uid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn proc_cmdline(pid: u32) -> String {
    match fs::read_to_string(format!("/proc/{}/cmdline", pid)) {
        Ok(raw) => {
            let text = raw
                .trim_end_matches(char::from(0))
                .replace(char::from(0), " ");
            if text.is_empty() {
                format!("<pid {}>", pid)
            } else {
                text
            }
        }
        Err(_) => format!("<pid {}>", pid),
    }
}

/// Processes currently holding any of `devices` open. Needs root to see other users' fds.
pub fn holders(devices: &[String]) -> Vec<Holder> {
    let mut out: Vec<Holder> = vec![];
    let procs = match fs::read_dir("/proc") {
        Ok(procs) => procs,
        Err(_) => return out,
    };

    for entry in procs.flatten() {
        let name = entry.file_name();
        let pid: u32 = match name.to_string_lossy().parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        let fds = match fs::read_dir(format!("/proc/{}/fd", pid)) {
            Ok(fds) => fds,
            Err(_) => continue, // process gone, or not ours to look at
        };
        for fd in fds.flatten() {
            let target = match fs::read_link(fd.path()) {
                Ok(target) => target.to_string_lossy().into_owned(),
                Err(_) => continue,
            };
            if !devices.iter().any(|d| *d == target) {
                continue;
            }
            let uid = proc_uid(pid).unwrap_or(0);
            out.push(Holder {
                pid,
                uid,
                user: util::get_username(uid),
                cmdline: proc_cmdline(pid),
                device: target,
            });
            break; // one report per process is enough
        }
    }

    out.sort_by_key(|h| h.pid);
    return out;
}

pub(crate) fn set_ownership(path: &str, uid: u32, gid: u32, mode: u32) -> Result<(), String> {
    let cpath = CString::new(path).map_err(|e| format!("bad path: {}", e))?;
    if unsafe { libc::chown(cpath.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
        return Err(format!("chown: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::chmod(cpath.as_ptr(), mode as libc::mode_t) } != 0 {
        return Err(format!("chmod: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Hand the board's device nodes to `claimant_uid`. Returns human readable failures; an
/// empty vec means every node of the spec is now locked.
pub fn lock_devices(spec: &FpgaSpec, claimant_uid: u32, state: &mut DiskState) -> Vec<String> {
    let mut failures = vec![];
    for path in &spec.devices {
        if state.device_locks.iter().any(|l| &l.path == path) {
            continue; // already ours
        }
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) => {
                failures.push(format!("{}: {}", path, err));
                continue;
            }
        };
        let lock = DeviceLock {
            path: path.clone(),
            alias: spec.alias.clone(),
            orig_uid: meta.uid(),
            orig_gid: meta.gid(),
            orig_mode: meta.mode() & 0o7777,
            claimant_uid,
        };
        // Write down what we are about to change *before* changing it: chown can succeed
        // while chmod fails, and without the record the original ownership is lost for good.
        // A lock whose application failed is retried by sync_locks and reported by `status`.
        let gid = lock.orig_gid;
        state.device_locks.push(lock);
        match set_ownership(path, claimant_uid, gid, LOCKED_MODE) {
            Ok(_) => println!("locked {} for uid {}", path, claimant_uid),
            Err(err) => failures.push(format!("{}: {}", path, err)),
        }
    }
    failures
}

/// True when the node is still owned and moded the way we left it.
pub fn lock_is_intact(lock: &DeviceLock) -> Option<bool> {
    let meta = fs::metadata(&lock.path).ok()?;
    Some(meta.uid() == lock.claimant_uid && (meta.mode() & 0o7777) == LOCKED_MODE)
}

/// Which user each board should belong to, according to the live exclusive claims. Shared
/// claims enforce nothing, and `host` owns no device nodes, so neither appears here.
pub fn intended_owners(state: &DiskState) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for claim in state.claims.iter().filter(|c| c.exclusive) {
        for resource in &claim.resources {
            if let Some(alias) = resource.alias() {
                if !out.iter().any(|(known, _)| known == alias) {
                    out.push((alias.to_string(), claim.user.clone()));
                }
            }
        }
    }
    out
}

/// Converge everything a claim protects: device files, then cables. The single entry point for
/// claims, releases, hogs, expiry and the re-check job.
pub fn sync_locks(state: &mut DiskState) {
    sync_device_locks(state);
    crate::cable::sync(
        state,
        std::path::Path::new(crate::cable::SYSFS),
        std::path::Path::new(crate::cable::DEV),
        std::path::Path::new(crate::cable::RULES_FILE),
    );
}

/// Converge device ownership onto what the claims say it should be. Safe to call at any
/// point; it is what makes crashes, expiries and driver reloads recoverable.
pub fn sync_device_locks(state: &mut DiskState) {
    let intended = intended_owners(state);

    // 1. Hand back boards that no live exclusive claim covers, and re-point boards that
    //    changed hands. Checking only *whether* a claim exists, without checking whose it
    //    is, would leave a board chowned to the previous holder.
    let existing = std::mem::take(&mut state.device_locks);
    let mut keep: Vec<DeviceLock> = vec![];
    for mut lock in existing {
        let owner = intended
            .iter()
            .find(|(alias, _)| *alias == lock.alias)
            .map(|(_, user)| (user.clone(), util::get_uid(user)));

        match owner {
            Some((_, Some(uid))) => {
                if lock.claimant_uid != uid {
                    println!(
                        "{} changed hands: re-pointing {} at uid {}",
                        lock.alias, lock.path, uid
                    );
                    lock.claimant_uid = uid;
                }
                keep.push(lock);
            }
            Some((user, None)) => {
                println!(
                    "WARN: {} is claimed by unknown user {}. Leaving {} untouched.",
                    lock.alias, user, lock.path
                );
                keep.push(lock);
            }
            None => match set_ownership(&lock.path, lock.orig_uid, lock.orig_gid, lock.orig_mode) {
                Ok(_) => println!("released {} ({})", lock.path, lock.alias),
                Err(err) => {
                    println!("WARN: could not restore {}: {}. Will retry.", lock.path, err);
                    keep.push(lock);
                }
            },
        }
    }
    state.device_locks = keep;

    // 2. Take over nodes a live exclusive claim covers but we do not hold yet. This also
    //    picks up nodes that appeared after the claim was made (driver loaded later).
    for (alias, user) in &intended {
        let uid = match util::get_uid(user) {
            Some(uid) => uid,
            None => continue, // already warned about above
        };
        let spec = match state.settings.fpga(alias) {
            Some(spec) => spec.clone(),
            None => continue, // alias vanished from the config
        };
        for failure in lock_devices(&spec, uid, state) {
            println!("WARN: enforcement failed for {}: {}", alias, failure);
        }
    }

    // 3. Re-assert nodes that drifted. Drivers recreate their nodes world-writable, so a
    //    reload silently unlocks a board until we get here. A lock re-pointed in step 1 is
    //    picked up here too, since it no longer matches the node on disk.
    let drifted: Vec<DeviceLock> = state
        .device_locks
        .iter()
        .filter(|lock| lock_is_intact(lock) == Some(false))
        .cloned()
        .collect();
    for lock in drifted {
        match set_ownership(&lock.path, lock.claimant_uid, lock.orig_gid, LOCKED_MODE) {
            Ok(_) => println!("re-asserted lock on {} ({})", lock.path, lock.alias),
            Err(err) => println!("WARN: could not re-assert {}: {}", lock.path, err),
        }
    }
}

/// Locks currently held for one alias.
pub fn locks_for<'a>(state: &'a DiskState, alias: &str) -> Vec<&'a DeviceLock> {
    state
        .device_locks
        .iter()
        .filter(|l| l.alias == alias)
        .collect()
}

/// Resolve a resource list to the FPGA specs it names, erroring on unknown aliases.
pub fn resolve<'a>(
    state: &'a DiskState,
    resources: &[ResourceId],
) -> Result<Vec<&'a FpgaSpec>, String> {
    let mut specs = vec![];
    for resource in resources {
        let alias = match resource.alias() {
            Some(alias) => alias,
            None => continue,
        };
        match state.settings.fpga(alias) {
            Some(spec) => specs.push(spec),
            None => {
                let mut known = vec![crate::diskstate::HOST_RESOURCE.to_string()];
                known.extend(state.settings.aliases());
                return Err(format!(
                    "unknown resource `{}`. Known resources: {}.\nRun `{} discover --write` to register this host's FPGAs.",
                    alias,
                    known.join(", "),
                    util::prog_name()
                ));
            }
        }
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diskstate::{load_default, Claim, ResourceId};
    use chrono::Local;

    fn claim(user: &str, resources: &[&str], exclusive: bool) -> Claim {
        Claim {
            id: 1,
            timeout: Local::now() + chrono::Duration::hours(1),
            soft_timeout: None,
            exclusive,
            user: user.into(),
            comment: String::new(),
            resources: resources.iter().map(|r| ResourceId::parse(r)).collect(),
        }
    }

    #[test]
    fn intended_owners_names_the_holder_of_each_board() {
        let mut state = load_default();
        state.claims = vec![
            claim("anubhav", &["u280"], true),
            claim("colleague", &["v80"], true),
        ];
        let owners = intended_owners(&state);
        assert_eq!(owners.len(), 2);
        assert!(owners.contains(&("u280".to_string(), "anubhav".to_string())));
        assert!(owners.contains(&("v80".to_string(), "colleague".to_string())));
    }

    /// Shared claims enforce nothing and `host` owns no device nodes, so neither may make
    /// sync_locks touch a board.
    #[test]
    fn shared_and_host_claims_own_no_devices() {
        let mut state = load_default();
        state.claims = vec![
            claim("anubhav", &["u280"], false),
            claim("colleague", &["host"], true),
        ];
        assert!(intended_owners(&state).is_empty());
    }

    /// Regression: sync_locks used to ask only *whether* a claim covered a board, not whose
    /// it was, which would leave the node chowned to the previous holder.
    #[test]
    fn a_board_changing_hands_is_re_pointed_not_left_alone() {
        let mut state = load_default();
        state.claims = vec![claim("newcomer", &["u280"], true)];
        let owners = intended_owners(&state);
        let owner = owners.iter().find(|(alias, _)| alias == "u280").unwrap();
        assert_eq!(owner.1, "newcomer", "the new holder must be the intended owner");
    }

    /// The enforcement mechanism end to end on a scratch file: capture the original mode,
    /// lock it, then hand it back byte for byte. No device nodes involved.
    #[test]
    fn locking_captures_and_restores_the_original_mode() {
        let me = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let dir = std::env::temp_dir().join(format!("fpgahog-lock-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake_device");
        fs::write(&path, b"").unwrap();
        let path_str = path.to_string_lossy().into_owned();
        set_ownership(&path_str, me, gid, 0o666).unwrap();

        let spec = FpgaSpec {
            alias: "scratch".into(),
            bdf: "0000:00:00.0".into(),
            pci_id: String::new(),
            devices: vec![path_str.clone()],
            cables: vec![],
        };
        let mut state = load_default();
        state.settings.fpgas = vec![spec.clone()];

        let failures = lock_devices(&spec, me, &mut state);
        assert!(failures.is_empty(), "locking failed: {:?}", failures);
        assert_eq!(state.device_locks.len(), 1);
        assert_eq!(state.device_locks[0].orig_mode, 0o666, "original mode must be recorded");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, LOCKED_MODE);

        // No claim backs it, so converging must hand it back exactly as it was.
        sync_device_locks(&mut state);
        assert!(state.device_locks.is_empty(), "the lock should be gone");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o666);

        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod device_prefix_tests {
    use super::*;

    /// rose moved from one `coyote_driver` to per-platform drivers, whose nodes are named
    /// after the platform. Without this, discover guessed nothing and could not heal the
    /// configured paths the rename orphaned.
    #[test]
    fn coyote_platform_drivers_map_to_their_node_names() {
        assert_eq!(device_prefix("coyote_driver_versal").as_deref(), Some("coyote_versal_fpga_"));
        assert_eq!(
            device_prefix("coyote_driver_ultrascale_plus").as_deref(),
            Some("coyote_ultrascale_plus_fpga_")
        );
        assert_eq!(device_prefix("coyote_driver").as_deref(), Some("coyote_fpga_"));
        assert_eq!(device_prefix("xdma").as_deref(), Some("xdma"));
        assert_eq!(device_prefix("nvme"), None);
    }
}
