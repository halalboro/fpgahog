use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use chrono::prelude::*;
use crate::users;
use crate::util;
use once_cell::sync::Lazy;

static STATE_FILE: Lazy<String> = Lazy::new(|| format!("{}/fpgahog.json", util::STATE_PATH));
const SUPPORTED_STATE_VERSIONS: [u32; 1] = [ 3 ];
const DEFAULT_STATE_VERSION: u32 = 3;
/// Versions we know how to read and silently upgrade: hosthog's formats, where claims covered
/// the whole host and there was no notion of an FPGA. v1 differs from v2 only in calling the
/// stopped systemd units `disabled_systemd_timers`.
const MIGRATABLE_STATE_VERSIONS: [u32; 2] = [ 1, 2 ];

pub const HOST_RESOURCE: &str = "host";

/// Something that can be claimed. `host` is the whole machine (ssh logins and systemd
/// units, exactly as hosthog always meant it); every other id names an FPGA by the alias
/// given to it in `settings.fpgas`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceId {
    Host,
    Fpga(String),
}

impl ResourceId {
    pub fn parse(text: &str) -> ResourceId {
        if text.eq_ignore_ascii_case(HOST_RESOURCE) {
            ResourceId::Host
        } else {
            ResourceId::Fpga(text.to_string())
        }
    }

    /// Parse a comma separated resource list, dropping empties and duplicates.
    pub fn parse_list(text: &str) -> Vec<ResourceId> {
        let mut out: Vec<ResourceId> = vec![];
        for part in text.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let id = ResourceId::parse(part);
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// The FPGA alias, or None for the host pseudo-resource.
    pub fn alias(&self) -> Option<&str> {
        match self {
            ResourceId::Fpga(alias) => Some(alias.as_str()),
            ResourceId::Host => None,
        }
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceId::Host => write!(f, "{}", HOST_RESOURCE),
            ResourceId::Fpga(alias) => write!(f, "{}", alias),
        }
    }
}

impl Serialize for ResourceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ResourceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<ResourceId, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() {
            return Err(D::Error::custom("empty resource id"));
        }
        Ok(ResourceId::parse(&text))
    }
}

fn default_host_resources() -> Vec<ResourceId> {
    vec![ResourceId::Host]
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Claim {
    /// Stable identity, assigned on creation and never reused. The hogger is remembered as
    /// a snapshot of a claim, so comparing whole structs would break the moment a claim is
    /// edited -- which `release <resource>` does.
    #[serde(default)]
    pub id: u64,
    pub timeout: DateTime<Local>,
    pub soft_timeout: Option<DateTime<Local>>,
    pub exclusive: bool,
    pub user: String,
    pub comment: String,
    /// Resources this claim covers. v2 statefiles have no such field: those claims were
    /// always about the whole host, which is what the default reproduces.
    #[serde(default = "default_host_resources")]
    pub resources: Vec<ResourceId>,
}

impl Claim {
    pub fn covers(&self, resource: &ResourceId) -> bool {
        self.resources.contains(resource)
    }

    pub fn covers_alias(&self, alias: &str) -> bool {
        self.covers(&ResourceId::Fpga(alias.to_string()))
    }

    pub fn resources_str(&self) -> String {
        self.resources
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<String>>()
            .join(",")
    }
}

/// An FPGA this host offers for claiming. Written by `discover --write`, then edited by
/// an admin: only a human can reliably say which character device belongs to which card.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct FpgaSpec {
    /// Short name used on the command line, e.g. "u280".
    pub alias: String,
    /// PCI address. The stable identity of the board, e.g. "0000:c1:00.0".
    pub bdf: String,
    /// "vendor:device" as seen at discovery time, kept so we can warn when a slot changed.
    #[serde(default)]
    pub pci_id: String,
    /// Character devices that hand out access to this board. An empty list means a claim
    /// can be recorded but not enforced.
    #[serde(default)]
    pub devices: Vec<String>,
    /// USB serials of the board's JTAG cables (see `discover`). An exclusive claim locks each
    /// cable's JTAG node and serial consoles.
    #[serde(default)]
    pub cables: Vec<String>,
}

/// Ownership and mode of a device node before we took it over, so release restores
/// exactly what was there rather than a guess.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct DeviceLock {
    pub path: String,
    pub alias: String,
    pub orig_uid: u32,
    pub orig_gid: u32,
    pub orig_mode: u32,
    pub claimant_uid: u32,
}

/// Owner, group and permission bits of a device file.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
pub struct Ownership {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

/// A cable handed to a claimant. Original ownership is kept per cable, not per file: the files
/// are re-created with new names on every re-enumeration, and one re-created under our udev rule
/// would otherwise be remembered as already locked.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct CableLock {
    pub serial: String,
    pub alias: String,
    /// user name, written into the udev rule's OWNER
    pub claimant: String,
    pub claimant_uid: u32,
    pub jtag_orig: Ownership,
    /// None when the cable had no consoles when it was locked
    pub console_orig: Option<Ownership>,
    /// the hub port that re-plugs this cable; recorded because the path cannot be resolved
    /// once the device is gone
    #[serde(default)]
    pub port_disable: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Settings {
    /// This should be the same as AuthorizedKeysFile in /etc/ssh/sshd_config (see man
    /// sshd_config)
    pub authorized_keys_file: Vec<String>,
    /// FPGAs this host offers. See `discover`.
    #[serde(default)]
    pub fpgas: Vec<FpgaSpec>,
}

impl Settings {
    pub fn fpga(&self, alias: &str) -> Option<&FpgaSpec> {
        self.fpgas.iter().find(|f| f.alias == alias)
    }

    pub fn aliases(&self) -> Vec<String> {
        self.fpgas.iter().map(|f| f.alias.clone()).collect()
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct DiskState {
    // Claim under which the system is currently hogged
    pub hogger: Option<Claim>,
    /// paths of all files that are bind-mounted to /dev/null
    /// If unmounting failed for some, the system may not be hogged but this list may contain
    /// items.
    pub overmounts: Vec<String>,
    /// current claims
    pub claims: Vec<Claim>,
    /// settings to be modified by users
    pub settings: Settings,
    /// hosthog v1 named this `disabled_systemd_timers`
    #[serde(alias = "disabled_systemd_timers")]
    pub disabled_systemd_units: Vec<String>,
    /// device nodes we took over, with the ownership to restore on release
    #[serde(default)]
    pub device_locks: Vec<DeviceLock>,
    /// cables we took over, with the ownership to restore on release
    #[serde(default)]
    pub cable_locks: Vec<CableLock>,
    /// hub ports an eviction disabled and has not re-enabled yet. Written before the port is
    /// disabled, so a killed eviction is repaired by the next run instead of stranding a cable.
    #[serde(default)]
    pub pending_replug: Vec<String>,
    /// when the queued `at` job that re-applies board locks is due, if there is one
    #[serde(default)]
    pub recheck_at: Option<DateTime<Local>>,
    pub state_version: u32,
}

impl DiskState {
    /// An id no claim in this statefile uses. Derived from the file rather than the clock so
    /// it stays deterministic and testable.
    pub fn next_claim_id(&self) -> u64 {
        let highest = self
            .claims
            .iter()
            .chain(self.hogger.iter())
            .map(|c| c.id)
            .max()
            .unwrap_or(0);
        highest + 1
    }

    /// Claims by someone other than `me` that cover `resource`.
    pub fn foreign_claims_on<'a>(&'a self, resource: &ResourceId, me: &str) -> Vec<&'a Claim> {
        self.claims
            .iter()
            .filter(|c| c.user != me && c.covers(resource))
            .collect()
    }
}

pub fn check_version(state: &DiskState) -> Result<(), String> {
    if SUPPORTED_STATE_VERSIONS.iter().any(|v| *v == state.state_version) {
        Ok(())
    } else {
        let str = format!("Statefile is of version {}. Supported versions: {:?}. ({})", state.state_version, SUPPORTED_STATE_VERSIONS, STATE_FILE.as_str());
        Err(str.to_string())
    }
}

/// The statefile exactly as it sits on disk, with no upgrading applied. `main` keeps one of
/// these as the baseline for its dirty check, so that a migration counts as a change and
/// actually gets written back.
pub fn load_raw() -> DiskState {
    if !std::path::Path::new(STATE_FILE.as_str()).is_file() {
        // Nothing recorded yet. Only root may create the file; anyone else just sees defaults.
        if !users::is_root() {
            return load_default();
        }
        store(&load_default());
    }

    let text = match std::fs::read_to_string(STATE_FILE.as_str()) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("cannot read {}: {}", STATE_FILE.as_str(), err);
            std::process::exit(1);
        }
    };
    match serde_json::from_str(&text) {
        Ok(state) => state,
        Err(err) => {
            eprintln!("cannot parse {}: {}", STATE_FILE.as_str(), err);
            std::process::exit(1);
        }
    }
}

pub fn load() -> DiskState {
    let mut state = load_raw();
    migrate(&mut state);
    ensure_claim_ids(&mut state);
    return state;
}

/// Give an id to any claim that lacks one. Covers claims migrated from v2 and any statefile
/// written before ids existed; both deserialize with id 0.
fn ensure_claim_ids(state: &mut DiskState) {
    let mut next = state.next_claim_id();
    for index in 0..state.claims.len() {
        if state.claims[index].id == 0 {
            state.claims[index].id = next;
            next += 1;
        }
    }
    if let Some(hogger) = &mut state.hogger {
        if hogger.id == 0 {
            // A hogger with no id can no longer be matched to its claim. Re-link it by
            // content, which is exactly what the old whole-struct comparison did.
            let matched = state
                .claims
                .iter()
                .find(|c| c.user == hogger.user && c.timeout == hogger.timeout)
                .map(|c| c.id);
            hogger.id = matched.unwrap_or(next);
        }
    }
}

/// Upgrade an older statefile in place. Rejecting it outright would strand claims that are
/// live right now on this host, so we read what we can and move the version forward.
fn migrate(state: &mut DiskState) {
    if !MIGRATABLE_STATE_VERSIONS.iter().any(|v| *v == state.state_version) {
        return;
    }
    let from = state.state_version;
    // Claims and the hogger already defaulted to `[host]` while deserializing, and the new
    // collections defaulted to empty, so there is nothing else to fix up.
    state.state_version = DEFAULT_STATE_VERSION;
    println!(
        "Migrated statefile from version {} to {} ({} claims are now `{}` claims).",
        from, DEFAULT_STATE_VERSION, state.claims.len(), HOST_RESOURCE
    );
}

/// How long a command waits for another fpgahog to finish before giving up.
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Serialise commands that change state. Each one loads, changes and rewrites the whole file,
/// so two at once dropped whichever change was written first: on rose nine of ten simultaneous
/// claims vanished. The lock is held until the returned file is dropped.
pub fn lock() -> Result<std::fs::File, String> {
    std::fs::create_dir_all(util::STATE_PATH)
        .map_err(|e| format!("cannot create {}: {}", util::STATE_PATH, e))?;
    lock_path(&std::path::Path::new(util::STATE_PATH).join("fpgahog.lock"), LOCK_WAIT)
}

pub fn lock_path(path: &std::path::Path, wait: std::time::Duration) -> Result<std::fs::File, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // the lock file's contents do not matter, only the lock on it
        .open(path)
        .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            // Leave our pid behind so the next waiter can say who is holding things up.
            use std::io::Write;
            let mut file = file;
            let _ = file.set_len(0);
            let _ = write!(file, "{}", std::process::id());
            let _ = file.flush();
            return Ok(file);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(format!("cannot lock {}: {}", path.display(), err));
        }
        if std::time::Instant::now() >= deadline {
            let holder = std::fs::read_to_string(path)
                .map(|pid| format!(" (pid {})", pid.trim()))
                .unwrap_or_default();
            return Err(format!(
                "another fpgahog is still running{}; gave up after {}s waiting for {}",
                holder,
                wait.as_secs(),
                path.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Replace `path` in one step, so a reader never sees a half-written statefile.
pub fn write_atomically(path: &std::path::Path, contents: &str) -> Result<(), String> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, contents)
        .map_err(|e| format!("cannot write {}: {}", temporary.display(), e))?;
    std::fs::rename(&temporary, path)
        .map_err(|e| format!("cannot replace {}: {}", path.display(), e))
}

pub fn store(state: &DiskState) {
    if !users::is_root() {
        eprintln!("must be root to update {}", STATE_FILE.as_str());
        std::process::exit(1);
    }
    let json = serde_json::to_string(&state).unwrap();
    let path = std::path::Path::new(STATE_FILE.as_str());
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!("cannot create {}: {}", parent.display(), err);
            std::process::exit(1);
        }
    }
    if let Err(err) = write_atomically(path, &json) {
        eprintln!("{}", err);
        std::process::exit(1);
    }
}

pub fn load_default() -> DiskState {
    let state = DiskState {
        hogger: None,
        overmounts: vec![],
        claims: vec![],
        settings: Settings {
            authorized_keys_file: vec![
                String::from("%h/.ssh/authorized_keys"),
                String::from("/etc/ssh/authorized_keys.d/%u"),
            ],
            fpgas: vec![],
        },
        disabled_systemd_units: vec![],
        device_locks: vec![],
        cable_locks: vec![],
        pending_replug: vec![],
        recheck_at: None,
        state_version: DEFAULT_STATE_VERSION,
    };

    return state;
}

use crate::hog;

pub fn expand_authorized_keys_file(settings: &Settings, users: Vec<hog::User>) -> Vec<String> {
    let mut files = vec![];
    for user in users {
        for file in &settings.authorized_keys_file {
            // replace %h with home directory
            let file = file.replacen("%h", &user.home, 1);
            // replace %u with username
            let file = file.replacen("%u", &user.name, 1);
            files.push(file);
            // TODO other replacements and respect escaped %: %%
        }
    }
    return files;
}

/// remove all claims that have timed out. Returns the claims that were dropped.
pub fn maintenance(state: &mut DiskState, needs_release: &mut bool) -> Vec<Claim> {
    let now = Local::now();
    let mut new_claims = vec![];
    let mut dropped_claims = vec![];
    for claim in &state.claims {
        if claim.timeout > now {
            new_claims.push(claim.clone());
        } else {
            dropped_claims.push(claim.clone());
        }
    }
    if let Some(hogger) = &state.hogger {
        for claim in &dropped_claims {
            if claim.id == hogger.id {
                // we just dropped the claim responsible for a current hogging
                *needs_release = true;
            }
        }
    }
    state.claims = new_claims;
    println!("Maintenance: {} claims expired, {} hogs released", dropped_claims.len(), if *needs_release { 1 } else { 0 });
    return dropped_claims;
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2_STATEFILE: &str = r#"{
        "hogger": null,
        "overmounts": [],
        "claims": [{
            "timeout": "2030-01-01T00:00:00+00:00",
            "soft_timeout": null,
            "exclusive": true,
            "user": "someone",
            "comment": "benchmarks"
        }],
        "settings": { "authorized_keys_file": ["%h/.ssh/authorized_keys"] },
        "disabled_systemd_units": [],
        "state_version": 2
    }"#;

    #[test]
    fn resource_ids_round_trip_as_plain_strings() {
        assert_eq!(ResourceId::parse("host"), ResourceId::Host);
        assert_eq!(ResourceId::parse("HOST"), ResourceId::Host);
        assert_eq!(ResourceId::parse("u280"), ResourceId::Fpga("u280".into()));
        assert_eq!(ResourceId::Host.to_string(), "host");
        assert_eq!(ResourceId::Fpga("v80".into()).to_string(), "v80");

        let encoded = serde_json::to_string(&ResourceId::Fpga("v80".into())).unwrap();
        assert_eq!(encoded, "\"v80\"");
        let decoded: ResourceId = serde_json::from_str("\"host\"").unwrap();
        assert_eq!(decoded, ResourceId::Host);
    }

    #[test]
    fn resource_lists_drop_blanks_and_duplicates() {
        let parsed = ResourceId::parse_list("host, u280 ,,u280");
        assert_eq!(parsed, vec![ResourceId::Host, ResourceId::Fpga("u280".into())]);
        assert!(ResourceId::parse_list("  ,, ").is_empty());
    }

    /// A v2 statefile has no `resources`, `fpgas` or `device_locks`. It has to keep meaning
    /// what it meant: a claim on the whole host.
    #[test]
    fn v2_statefile_migrates_to_host_claims() {
        let v2 = V2_STATEFILE;

        let mut state: DiskState = serde_json::from_str(v2).unwrap();
        assert_eq!(state.state_version, 2);
        assert_eq!(state.claims[0].resources, vec![ResourceId::Host]);
        assert!(state.settings.fpgas.is_empty());
        assert!(state.device_locks.is_empty());

        migrate(&mut state);
        assert_eq!(state.state_version, DEFAULT_STATE_VERSION);
        assert!(check_version(&state).is_ok());
        assert!(state.claims[0].covers(&ResourceId::Host));
    }

    fn at(id: u64, user: &str, resources: &[&str], timeout: DateTime<Local>) -> Claim {
        Claim {
            id,
            timeout,
            soft_timeout: None,
            exclusive: true,
            user: user.into(),
            comment: String::new(),
            resources: resources.iter().map(|r| ResourceId::parse(r)).collect(),
        }
    }

    /// Regression: `hog`, then `release u280`, then let the claim expire. The hogger is a
    /// snapshot taken at hog time, so once a resource is trimmed out of the live claim the
    /// two no longer compare equal. Matching on content left ssh bind-mounted forever.
    #[test]
    fn hogger_is_still_recognised_after_a_partial_release() {
        let expired = Local::now() - chrono::Duration::hours(1);
        let mut state = load_default();
        state.hogger = Some(at(7, "me", &["host", "u280", "v80"], expired));
        state.claims = vec![at(7, "me", &["host", "v80"], expired)];

        let mut needs_release = false;
        let dropped = maintenance(&mut state, &mut needs_release);

        assert_eq!(dropped.len(), 1);
        assert!(needs_release, "expiring a trimmed hogging claim must still unhog ssh");
    }

    /// Regression: main compares a raw load against a migrated one to decide whether to
    /// write. If both sides were migrated the upgrade compared equal to itself and was
    /// never persisted, so the file stayed at v2 and re-migrated on every single run.
    #[test]
    fn migration_registers_as_a_change_so_it_gets_written_back() {
        let baseline: DiskState = serde_json::from_str(V2_STATEFILE).unwrap();
        let mut upgraded: DiskState = serde_json::from_str(V2_STATEFILE).unwrap();
        migrate(&mut upgraded);
        ensure_claim_ids(&mut upgraded);

        assert_ne!(baseline, upgraded, "a migration must count as a change");
        assert_eq!(upgraded.state_version, DEFAULT_STATE_VERSION);
        assert!(upgraded.claims.iter().all(|c| c.id != 0), "every claim needs an id");
    }

    #[test]
    fn claim_ids_are_unique_and_never_reused() {
        let mut state = load_default();
        state.claims = vec![at(3, "a", &["u280"], Local::now())];
        assert_eq!(state.next_claim_id(), 4);
        // a dropped claim still counts while it is the hogger, so ids are not recycled
        state.hogger = Some(at(9, "a", &["host"], Local::now()));
        assert_eq!(state.next_claim_id(), 10);
    }

    #[test]
    fn claims_only_cover_what_they_name() {
        let claim = Claim {
            id: 1,
            timeout: Local::now(),
            soft_timeout: None,
            exclusive: true,
            user: "me".into(),
            comment: String::new(),
            resources: vec![ResourceId::Fpga("u280".into())],
        };
        assert!(claim.covers_alias("u280"));
        assert!(!claim.covers_alias("v80"));
        assert!(!claim.covers(&ResourceId::Host));
        assert_eq!(claim.resources_str(), "u280");
    }
}

#[cfg(test)]
mod hosthog_compat_tests {
    use super::*;

    /// rose's statefile, byte for byte: hosthog from before it renamed systemd "timers" to
    /// "units", which is the only difference between its v1 and v2 formats.
    const ROSE_V1: &str = r#"{"hogger":null,"overmounts":[],"claims":[],"settings":{"authorized_keys_file":["%h/.ssh/authorized_keys","/etc/ssh/authorized_keys.d/%u"]},"disabled_systemd_timers":[],"state_version":1}"#;

    #[test]
    fn hosthog_v1_statefile_is_read_and_upgraded() {
        let mut state: DiskState = serde_json::from_str(ROSE_V1).unwrap();
        migrate(&mut state);
        ensure_claim_ids(&mut state);
        assert_eq!(state.state_version, DEFAULT_STATE_VERSION);
        assert!(check_version(&state).is_ok());
        assert_eq!(state.settings.authorized_keys_file.len(), 2);
    }

    /// A v1 host that is hogged at upgrade time must still restart the units it stopped.
    #[test]
    fn v1_disabled_timers_carry_over_as_units() {
        let hogged = ROSE_V1.replace(
            r#""disabled_systemd_timers":[]"#,
            r#""disabled_systemd_timers":["fstrim.timer"]"#,
        );
        let state: DiskState = serde_json::from_str(&hogged).unwrap();
        assert_eq!(state.disabled_systemd_units, vec!["fstrim.timer"]);
    }

    /// fpgahog runs next to the cluster's own hosthog rather than replacing it, and hosthog
    /// cannot read fpgahog's newer format, so the two must never share a statefile.
    #[test]
    fn state_is_kept_apart_from_hosthogs() {
        assert_eq!(STATE_FILE.as_str(), "/var/lib/fpgahog/fpgahog.json");
    }
}

#[cfg(test)]
mod statefile_io_tests {
    use super::*;
    use std::time::Duration;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fpgahog-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Regression: every command loads, changes and rewrites the whole file, so two claims
    /// made at the same moment dropped one of them. A second writer must wait for the first.
    #[test]
    fn a_second_writer_waits_for_the_first() {
        let dir = scratch("lock");
        let path = dir.join("fpgahog.lock");
        let first = lock_path(&path, Duration::from_millis(0)).unwrap();
        assert!(lock_path(&path, Duration::from_millis(150)).is_err(), "the lock was handed out twice");
        drop(first);
        assert!(lock_path(&path, Duration::from_millis(150)).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The waiter is told which process is holding the lock, so a slow eviction is obvious.
    #[test]
    fn the_lock_names_the_process_holding_it() {
        let dir = scratch("lock-pid");
        let path = dir.join("fpgahog.lock");
        let first = lock_path(&path, Duration::from_millis(0)).unwrap();
        let err = lock_path(&path, Duration::from_millis(100)).unwrap_err();
        assert!(err.contains(&std::process::id().to_string()), "{}", err);
        drop(first);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Readers such as `status` and `check` take no lock, so they must never see half a file.
    #[test]
    fn a_statefile_is_replaced_in_one_step() {
        let dir = scratch("atomic");
        let path = dir.join("state.json");
        std::fs::write(&path, "old").unwrap();
        write_atomically(&path, "{\"new\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\":true}");
        assert!(!path.with_extension("json.tmp").exists(), "temporary file left behind");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod cable_state_tests {
    use super::*;

    /// Statefiles written before cables existed must still load, with nothing locked.
    #[test]
    fn a_statefile_without_cables_loads() {
        let mut state = load_default();
        state.settings.fpgas.push(FpgaSpec {
            alias: "u280".into(),
            bdf: "0000:c1:00.0".into(),
            pci_id: String::new(),
            devices: vec![],
            cables: vec![],
        });
        let mut json = serde_json::to_value(&state).unwrap();
        json.as_object_mut().unwrap().remove("cable_locks");
        json["settings"]["fpgas"][0].as_object_mut().unwrap().remove("cables");
        let back: DiskState = serde_json::from_value(json).unwrap();
        assert!(back.cable_locks.is_empty());
        assert!(back.settings.fpgas[0].cables.is_empty());
    }

    #[test]
    fn cable_locks_round_trip() {
        let mut state = load_default();
        state.cable_locks.push(CableLock {
            serial: "217702174005".into(),
            alias: "u280".into(),
            claimant: "anubhav".into(),
            claimant_uid: 2049,
            jtag_orig: Ownership { uid: 0, gid: 0, mode: 0o666 },
            console_orig: Some(Ownership { uid: 0, gid: 27, mode: 0o660 }),
            port_disable: "/sys/devices/usb3/3-0:1.0/usb3-port2/disable".into(),
        });
        let text = serde_json::to_string(&state).unwrap();
        let back: DiskState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.cable_locks, state.cable_locks);
    }
}
