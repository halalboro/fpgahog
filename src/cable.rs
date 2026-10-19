//! USB JTAG cables: finding a board's cable by serial, locking its JTAG node and serial
//! consoles, and cutting off handles opened before a claim.
//! Design: docs/superpowers/specs/2026-09-17-jtag-cable-locking-design.md

use crate::diskstate::{CableLock, DiskState, FpgaSpec, Ownership};
use crate::fpga;
use crate::util;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SYSFS: &str = "/sys";
pub const DEV: &str = "/dev";

/// USB vendors `discover` offers as FPGA cables: FTDI, Xilinx.
const CABLE_VENDORS: [&str; 2] = ["0403", "03fd"];

#[derive(Debug, Clone, PartialEq)]
pub struct Cable {
    pub serial: String,
    pub product: String,
    /// usbfs node JTAG tools open, /dev/bus/usb/BBB/DDD
    pub jtag: PathBuf,
    /// /dev/ttyUSB* of the cable's interfaces, sorted
    pub consoles: Vec<PathBuf>,
    /// writing 1 then 0 here disconnects and reconnects the cable; this is the hub's port
    /// object, which outlives the device itself
    pub port_disable: PathBuf,
}

/// Serials and user names go into a udev rule, so only a plain character set is accepted.
pub fn valid_serial(serial: &str) -> bool {
    !serial.is_empty()
        && serial.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn read_attr(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_string())
}

fn devices_dir(sysfs: &Path) -> PathBuf {
    sysfs.join("bus/usb/devices")
}

/// USB devices (not interfaces, whose names contain ':'), sorted.
fn usb_device_names(sysfs: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(devices_dir(sysfs))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.contains(':'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The cable at usb device `name`, e.g. "3-2", as it is right now.
fn read_cable(sysfs: &Path, dev: &Path, name: &str) -> Option<Cable> {
    let dir = devices_dir(sysfs).join(name);
    let serial = read_attr(&dir, "serial")?;
    let busnum: u32 = read_attr(&dir, "busnum")?.parse().ok()?;
    let devnum: u32 = read_attr(&dir, "devnum")?.parse().ok()?;
    let prefix = format!("{}:", name);
    let mut consoles = vec![];
    if let Ok(entries) = fs::read_dir(devices_dir(sysfs)) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            if let Ok(children) = fs::read_dir(entry.path()) {
                for child in children.flatten() {
                    let child = child.file_name().to_string_lossy().into_owned();
                    if child.starts_with("ttyUSB") {
                        consoles.push(dev.join(child));
                    }
                }
            }
        }
    }
    consoles.sort();
    Some(Cable {
        serial,
        product: read_attr(&dir, "product").unwrap_or_default(),
        jtag: dev.join(format!("bus/usb/{:03}/{:03}", busnum, devnum)),
        consoles,
        // Resolve the symlink to the hub's port object: the device directory itself disappears
        // the moment the port is disabled, taking a path through it with it.
        port_disable: fs::canonicalize(dir.join("port")).unwrap_or_else(|_| dir.join("port")).join("disable"),
    })
}

/// The cable with this serial right now. Device numbers and ttyUSB names change on every
/// re-enumeration, so nothing here is cached.
pub fn find(sysfs: &Path, dev: &Path, serial: &str) -> Result<Option<Cable>, String> {
    let matches: Vec<Cable> = usb_device_names(sysfs)
        .iter()
        .filter_map(|name| read_cable(sysfs, dev, name))
        .filter(|cable| cable.serial == serial)
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        n => Err(format!("{} USB devices share serial {}; not locking any of them", n, serial)),
    }
}

/// FTDI and Xilinx USB devices: the cables FPGA boards are programmed through.
pub fn discover(sysfs: &Path, dev: &Path) -> Vec<Cable> {
    usb_device_names(sysfs)
        .iter()
        .filter(|name| {
            read_attr(&devices_dir(sysfs).join(name.as_str()), "idVendor")
                .is_some_and(|vendor| CABLE_VENDORS.contains(&vendor.as_str()))
        })
        .filter_map(|name| read_cable(sysfs, dev, name))
        .collect()
}

pub const RULES_FILE: &str = "/run/udev/rules.d/99-fpgahog.rules";

/// udev rules handing each cable to its claimant whenever its device files are (re)created: on
/// replug, on power cycle, and after `evict`. Entries are (serial, user), both already checked
/// with `valid_serial`.
pub fn render_rules(entries: &[(String, String)]) -> String {
    let mut text = String::from("# Written by fpgahog: cables held by exclusive claims. Rewritten on every change.\n");
    for (serial, user) in entries {
        text.push_str(&format!(
            "SUBSYSTEM==\"usb\", ENV{{DEVTYPE}}==\"usb_device\", ATTR{{serial}}==\"{}\", OWNER=\"{}\", MODE=\"0600\"\n",
            serial, user
        ));
        text.push_str(&format!(
            "SUBSYSTEM==\"tty\", ATTRS{{serial}}==\"{}\", OWNER=\"{}\", MODE=\"0600\"\n",
            serial, user
        ));
    }
    text
}

/// Replace the rule file in one step, or remove it when no cable is held. udevd picks the change
/// up on the next event without a reload, and /run is cleared on reboot.
pub fn write_rules(path: &Path, entries: &[(String, String)]) -> Result<(), String> {
    if entries.is_empty() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("cannot remove {}: {}", path.display(), err)),
        };
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
    }
    // udev reads only *.rules, so the temporary name is ignored until the rename.
    let temporary = path.with_extension("rules.tmp");
    fs::write(&temporary, render_rules(entries))
        .map_err(|e| format!("cannot write {}: {}", temporary.display(), e))?;
    fs::rename(&temporary, path).map_err(|e| format!("cannot replace {}: {}", path.display(), e))
}

/// A cable some live exclusive claim should hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub serial: String,
    pub alias: String,
    pub user: String,
    pub uid: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// claimed but not locked yet: record a lock (ownership is read when applying)
    Record(Wanted),
    /// no longer claimed and plugged in: put the original ownership back, then drop the lock
    Restore(CableLock),
    /// no longer claimed and unplugged: drop the lock; udev's defaults apply when it returns
    Drop(CableLock),
    /// the board changed hands: keep the original ownership, change who holds it
    Repoint(Wanted),
}

/// What to do to bring the cable locks in line with the claims. Pure, so every case is testable.
pub fn plan(wanted: &[Wanted], locks: &[CableLock], present: impl Fn(&str) -> bool) -> Vec<Action> {
    let mut actions = vec![];
    for lock in locks {
        match wanted.iter().find(|w| w.serial == lock.serial) {
            Some(w) if w.uid != lock.claimant_uid || w.user != lock.claimant => {
                actions.push(Action::Repoint(w.clone()))
            }
            Some(_) => {}
            None if present(&lock.serial) => actions.push(Action::Restore(lock.clone())),
            None => actions.push(Action::Drop(lock.clone())),
        }
    }
    for w in wanted {
        if !locks.iter().any(|lock| lock.serial == w.serial) {
            actions.push(Action::Record(w.clone()));
        }
    }
    actions
}

fn ownership_of(path: &Path) -> Option<Ownership> {
    let meta = fs::metadata(path).ok()?;
    Some(Ownership { uid: meta.uid(), gid: meta.gid(), mode: meta.mode() & 0o7777 })
}

/// The ownership to restore on release. Read from the cable's files unless they cannot tell us:
/// the cable is absent, or udev already handed the files to the claimant. Then rose's udev
/// defaults are used (spec §1).
pub fn original_ownership(cable: Option<&Cable>, claimant_uid: u32, dialout_gid: u32) -> (Ownership, Option<Ownership>) {
    let default_jtag = Ownership { uid: 0, gid: 0, mode: 0o666 };
    let default_console = Ownership { uid: 0, gid: dialout_gid, mode: 0o660 };
    let cable = match cable {
        Some(cable) => cable,
        None => return (default_jtag, Some(default_console)),
    };
    let genuine = |found: Option<Ownership>| found.filter(|o| !(o.uid == claimant_uid && o.mode == 0o600));
    let jtag = genuine(ownership_of(&cable.jtag)).unwrap_or(default_jtag);
    let console = cable
        .consoles
        .first()
        .map(|path| genuine(ownership_of(path)).unwrap_or(default_console));
    (jtag, console)
}

/// Serials listed by more than one board, or twice by one board. Locking such a cable would
/// hand it to two claimants in turn on every pass, so it is refused instead.
pub fn duplicate_serials(specs: &[FpgaSpec]) -> Vec<String> {
    let mut seen: Vec<String> = vec![];
    let mut duplicates: Vec<String> = vec![];
    for serial in specs.iter().flat_map(|spec| spec.cables.iter()) {
        if seen.contains(serial) {
            if !duplicates.contains(serial) {
                duplicates.push(serial.clone());
            }
        } else {
            seen.push(serial.clone());
        }
    }
    duplicates
}

/// Cables the live exclusive claims should hold. Invalid serials, and user names that cannot go
/// into a udev rule, are skipped with a warning.
pub fn wanted(state: &DiskState) -> Vec<Wanted> {
    let mut out = vec![];
    let duplicates = duplicate_serials(&state.settings.fpgas);
    for (alias, user) in fpga::intended_owners(state) {
        let spec = match state.settings.fpga(&alias) {
            Some(spec) => spec,
            None => continue,
        };
        for serial in &spec.cables {
            if !valid_serial(serial) {
                println!("WARN: {} lists an invalid cable serial {:?}; skipping it", alias, serial);
                continue;
            }
            if duplicates.contains(serial) {
                println!("WARN: cable {} is listed by more than one board; locking it for none of them", serial);
                continue;
            }
            if !valid_serial(&user) {
                println!("WARN: user name {:?} cannot go into a udev rule; not locking cable {}", user, serial);
                continue;
            }
            match util::get_uid(&user) {
                Some(uid) => out.push(Wanted { serial: serial.clone(), alias: alias.clone(), user: user.clone(), uid }),
                None => println!("WARN: {} is claimed by unknown user {}; leaving cable {} alone", alias, user, serial),
            }
        }
    }
    out
}

fn files(cable: &Cable) -> Vec<PathBuf> {
    let mut files = vec![cable.jtag.clone()];
    files.extend(cable.consoles.iter().cloned());
    files
}

/// The cable's JTAG node and consoles, for `fpga::holders`.
pub fn paths(cable: &Cable) -> Vec<String> {
    files(cable).iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

fn set(path: &Path, ownership: Ownership) -> Result<(), String> {
    fpga::set_ownership(&path.to_string_lossy(), ownership.uid, ownership.gid, ownership.mode)
}

fn restore(cable: &Cable, lock: &CableLock) -> Result<(), String> {
    set(&cable.jtag, lock.jtag_orig)?;
    if let Some(original) = lock.console_orig {
        for console in &cable.consoles {
            set(console, original)?;
        }
    }
    Ok(())
}

/// Hand a file to the claimant, keeping its group, unless it already is theirs.
fn reassert(path: &Path, claimant_uid: u32) {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    if meta.uid() == claimant_uid && meta.mode() & 0o7777 == fpga::LOCKED_MODE {
        return;
    }
    match fpga::set_ownership(&path.to_string_lossy(), claimant_uid, meta.gid(), fpga::LOCKED_MODE) {
        Ok(()) => println!("locked {} for uid {}", path.display(), claimant_uid),
        Err(err) => println!("WARN: could not lock {}: {}", path.display(), err),
    }
}

/// Converge cable ownership onto the claims (spec §3). The rule is written first, so a cable
/// re-created between the steps below comes back locked anyway.
pub fn sync(state: &mut DiskState, sysfs: &Path, dev: &Path, rules: &Path) {
    reenable_disabled_ports(state);
    let wanted = wanted(state);
    let entries: Vec<(String, String)> = wanted.iter().map(|w| (w.serial.clone(), w.user.clone())).collect();
    if let Err(err) = write_rules(rules, &entries) {
        println!("WARN: {}", err);
    }

    let mut serials: Vec<String> = wanted.iter().map(|w| w.serial.clone()).collect();
    serials.extend(state.cable_locks.iter().map(|lock| lock.serial.clone()));
    serials.sort();
    serials.dedup();
    let found: Vec<(String, Option<Cable>)> = serials
        .into_iter()
        .map(|serial| {
            let cable = find(sysfs, dev, &serial).unwrap_or_else(|err| {
                println!("WARN: {}", err);
                None
            });
            (serial, cable)
        })
        .collect();
    let lookup = |serial: &str| found.iter().find(|(s, _)| s == serial).and_then(|(_, cable)| cable.clone());
    let dialout = util::get_gid("dialout").unwrap_or(0);

    for action in plan(&wanted, &state.cable_locks, |serial| lookup(serial).is_some()) {
        match action {
            Action::Record(w) => {
                let cable = lookup(&w.serial);
                let (jtag_orig, console_orig) = original_ownership(cable.as_ref(), w.uid, dialout);
                let port_disable = cable
                    .map(|cable| cable.port_disable.to_string_lossy().into_owned())
                    .unwrap_or_default();
                state.cable_locks.push(CableLock {
                    serial: w.serial,
                    alias: w.alias,
                    claimant: w.user,
                    claimant_uid: w.uid,
                    jtag_orig,
                    console_orig,
                    port_disable,
                });
            }
            Action::Repoint(w) => {
                if let Some(lock) = state.cable_locks.iter_mut().find(|lock| lock.serial == w.serial) {
                    println!("{} changed hands: cable {} now belongs to {}", w.alias, w.serial, w.user);
                    lock.alias = w.alias;
                    lock.claimant = w.user;
                    lock.claimant_uid = w.uid;
                }
            }
            Action::Drop(lock) => {
                println!("released cable {} ({}); it is unplugged, udev restores it on return", lock.serial, lock.alias);
                state.cable_locks.retain(|l| l.serial != lock.serial);
            }
            Action::Restore(lock) => {
                let cable = lookup(&lock.serial).expect("Restore is only planned for present cables");
                match restore(&cable, &lock) {
                    Ok(()) => {
                        println!("released cable {} ({})", lock.serial, lock.alias);
                        state.cable_locks.retain(|l| l.serial != lock.serial);
                    }
                    Err(err) => println!("WARN: could not restore cable {}: {}. Will retry.", lock.serial, err),
                }
            }
        }
    }

    for lock in &state.cable_locks {
        if let Some(cable) = lookup(&lock.serial) {
            for path in files(&cable) {
                reassert(&path, lock.claimant_uid);
            }
        }
    }
}

/// How long a re-plugged cable may take to come back.
pub const REPLUG_WAIT: Duration = Duration::from_secs(10);

/// Disconnect and reconnect the cable at its hub port. Unlike toggling `authorized`, this
/// invalidates every open handle, the JTAG one included (probe on rose, 2026-09-14). Run it only
/// after `sync` has written the udev rule, so the cable comes back already locked.
pub fn evict(cable: &Cable, sysfs: &Path, dev: &Path, wait: Duration) -> Result<(), String> {
    fs::write(&cable.port_disable, "1")
        .map_err(|e| format!("cannot disable the port of cable {}: {}", cable.serial, e))?;
    // Wait for the device to actually go, rather than sleeping a flat half second: the shorter
    // this window, the less chance a kill lands inside it.
    for _ in 0..80 {
        if matches!(find(sysfs, dev, &cable.serial), Ok(None)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    fs::write(&cable.port_disable, "0")
        .map_err(|e| format!("cannot re-enable the port of cable {}: {}", cable.serial, e))?;
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(Some(back)) = find(sysfs, dev, &cable.serial) {
            if back.jtag.exists() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("cable {} did not come back within {}s", cable.serial, wait.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The current JTAG nodes and consoles of a board's cables, for holder checks.
pub fn board_paths(spec: &FpgaSpec) -> Vec<String> {
    spec.cables
        .iter()
        .filter(|serial| valid_serial(serial))
        .filter_map(|serial| find(Path::new(SYSFS), Path::new(DEV), serial).ok().flatten())
        .flat_map(|cable| paths(&cable))
        .collect()
}

/// Cables of `specs` that somebody else holds open, with who holds them.
pub fn foreign_cable_holders(specs: &[FpgaSpec], my_uid: Option<u32>) -> Vec<(Cable, String, Vec<fpga::Holder>)> {
    let mut held = vec![];
    for spec in specs {
        for serial in spec.cables.iter().filter(|serial| valid_serial(serial)) {
            let cable = match find(Path::new(SYSFS), Path::new(DEV), serial) {
                Ok(Some(cable)) => cable,
                _ => continue,
            };
            let foreign: Vec<fpga::Holder> = fpga::holders(&paths(&cable))
                .into_iter()
                .filter(|holder| Some(holder.uid) != my_uid)
                .collect();
            if !foreign.is_empty() {
                held.push((cable, spec.alias.clone(), foreign));
            }
        }
    }
    held
}

/// Name who would lose a cable, without touching anything.
pub fn report_foreign_holders(held: &[(Cable, String, Vec<fpga::Holder>)]) {
    for (cable, alias, holders) in held {
        for holder in holders {
            println!("pid {} ({}, {}) holds {}'s cable {}", holder.pid, holder.cmdline, holder.user, alias, cable.serial);
        }
    }
}

/// Re-plug the held cables, cutting their handles off. The port is written down before it is
/// disabled, so an interrupted eviction is finished by the next run rather than stranding the
/// cable.
pub fn evict_held(state: &mut DiskState, held: &[(Cable, String, Vec<fpga::Holder>)]) {
    for (cable, alias, holders) in held {
        for holder in holders {
            println!("cutting pid {} ({}, {}) off {}'s cable", holder.pid, holder.cmdline, holder.user, alias);
        }
        let port = cable.port_disable.to_string_lossy().into_owned();
        state.pending_replug.push(port.clone());
        crate::diskstate::store(state);
        let outcome = evict(cable, Path::new(SYSFS), Path::new(DEV), REPLUG_WAIT);
        state.pending_replug.retain(|pending| *pending != port);
        crate::diskstate::store(state);
        match outcome {
            Ok(()) => println!("re-plugged cable {} ({})", cable.serial, alias),
            Err(err) => println!("WARN: {}. The claim stands, and the cable is locked when it returns.", err),
        }
    }
}

/// What `status` can say about one configured cable.
#[derive(Debug, Clone, PartialEq)]
pub enum CableView {
    Invalid,
    /// gone because a port we disabled was never re-enabled, not because it was unplugged
    PortDisabled,
    Absent,
    Present { locked: bool },
}

/// Re-enable hub ports an eviction left disabled: the ones it marked before disabling, and the
/// ones belonging to cables we hold that are missing. Without this a killed eviction strands the
/// cable, since nothing else ever writes to a port.
pub fn reenable_disabled_ports(state: &mut DiskState) {
    let mut ports: Vec<String> = state.pending_replug.clone();
    for lock in &state.cable_locks {
        if !lock.port_disable.is_empty() && !ports.contains(&lock.port_disable) {
            ports.push(lock.port_disable.clone());
        }
    }
    for port in ports {
        let path = Path::new(&port);
        if fs::read_to_string(path).map(|v| v.trim() == "1").unwrap_or(false) {
            match fs::write(path, "0") {
                Ok(()) => println!("re-enabled the usb port at {} (an eviction did not finish)", port),
                Err(err) => println!("WARN: cannot re-enable the usb port at {}: {}", port, err),
            }
        }
    }
    state.pending_replug.clear();
}

/// A cable as it is now: `locked` means a lock is recorded and every file really is the
/// claimant's with mode 0600.
pub fn view(state: &DiskState, serial: &str, sysfs: &Path, dev: &Path) -> CableView {
    if !valid_serial(serial) {
        return CableView::Invalid;
    }
    let cable = match find(sysfs, dev, serial) {
        Ok(Some(cable)) => cable,
        _ => {
            let disabled = state
                .cable_locks
                .iter()
                .find(|lock| lock.serial == serial)
                .is_some_and(|lock| {
                    !lock.port_disable.is_empty()
                        && fs::read_to_string(&lock.port_disable).map(|v| v.trim() == "1").unwrap_or(false)
                });
            return if disabled { CableView::PortDisabled } else { CableView::Absent };
        }
    };
    let locked = state.cable_locks.iter().find(|lock| lock.serial == serial).is_some_and(|lock| {
        files(&cable).iter().all(|path| {
            fs::metadata(path).is_ok_and(|m| m.uid() == lock.claimant_uid && m.mode() & 0o7777 == fpga::LOCKED_MODE)
        })
    });
    CableView::Present { locked }
}

/// One phrase for a board's cables in `status`, or None if it has none.
pub fn describe(claimed: bool, views: &[CableView]) -> Option<String> {
    if views.is_empty() {
        return None;
    }
    let phrase = if views.contains(&CableView::Invalid) {
        "cable serial invalid"
    } else if views.contains(&CableView::PortDisabled) {
        "cable port disabled"
    } else if views.contains(&CableView::Absent) {
        "cable absent"
    } else if !claimed {
        "cable unlocked"
    } else if views.iter().all(|v| *v == CableView::Present { locked: true }) {
        "cable locked"
    } else {
        "cable NOT locked"
    };
    Some(phrase.to_string())
}

/// Pair boards without cables with the one cable whose product name contains the board's alias,
/// e.g. `A-V80` for `v80`. A board matching two cables, or a cable matching two boards, is left
/// for a human. Returns (alias, serial).
pub fn assign(specs: &[FpgaSpec], cables: &[Cable]) -> Vec<(String, String)> {
    let matches = |alias: &str, cable: &Cable| cable.product.to_lowercase().contains(&alias.to_lowercase());
    let mut pairs = vec![];
    let taken: Vec<&String> = specs.iter().flat_map(|spec| spec.cables.iter()).collect();
    for spec in specs.iter().filter(|spec| spec.cables.is_empty()) {
        let candidates: Vec<&Cable> = cables.iter().filter(|c| matches(&spec.alias, c)).collect();
        if candidates.len() != 1 || !valid_serial(&candidates[0].serial) {
            continue;
        }
        if taken.contains(&&candidates[0].serial) {
            continue;
        }
        if specs.iter().filter(|other| matches(&other.alias, candidates[0])).count() != 1 {
            continue;
        }
        pairs.push((spec.alias.clone(), candidates[0].serial.clone()));
    }
    pairs
}

#[cfg(test)]
pub(crate) mod fake {
    use std::fs;
    use std::path::PathBuf;

    /// A throwaway sysfs and /dev under the temp dir, removed on drop.
    pub struct Tree {
        pub root: PathBuf,
        pub sysfs: PathBuf,
        pub dev: PathBuf,
    }

    impl Tree {
        pub fn new(name: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("fpgahog-cable-{}-{}", name, std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let sysfs = root.join("sys");
            let dev = root.join("dev");
            fs::create_dir_all(sysfs.join("bus/usb/devices")).unwrap();
            fs::create_dir_all(dev.join("bus/usb/003")).unwrap();
            Tree { root, sysfs, dev }
        }

        /// A cable at usb `name` on bus 3, with one interface per console, and device files.
        pub fn cable(&self, name: &str, vendor: &str, serial: &str, product: &str, devnum: u32, consoles: &[u32]) {
            let devices = self.sysfs.join("bus/usb/devices");
            let dir = devices.join(name);
            let port = devices.join(format!("3-0:1.0/usb3-port{}", name.rsplit('-').next().unwrap_or("1")));
            fs::create_dir_all(&port).unwrap();
            fs::write(port.join("disable"), "0\n").unwrap();
            fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(&port, dir.join("port")).unwrap();
            for (attr, value) in [("idVendor", vendor), ("serial", serial), ("product", product), ("busnum", "3")] {
                fs::write(dir.join(attr), format!("{}\n", value)).unwrap();
            }
            fs::write(dir.join("devnum"), format!("{}\n", devnum)).unwrap();
            fs::write(self.dev.join(format!("bus/usb/003/{:03}", devnum)), "").unwrap();
            for (index, number) in consoles.iter().enumerate() {
                let tty = format!("ttyUSB{}", number);
                fs::create_dir_all(devices.join(format!("{}:1.{}", name, index)).join(&tty)).unwrap();
                fs::write(self.dev.join(&tty), "").unwrap();
            }
        }

        /// Take a cable away, as an unplug does.
        pub fn remove_cable(&self, name: &str) {
            let devices = self.sysfs.join("bus/usb/devices");
            for entry in fs::read_dir(&devices).unwrap().flatten() {
                let entry_name = entry.file_name().to_string_lossy().into_owned();
                if entry_name == name || entry_name.starts_with(&format!("{}:", name)) {
                    fs::remove_dir_all(entry.path()).unwrap();
                }
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod find_tests {
    use super::fake::Tree;
    use super::*;

    #[test]
    fn a_cable_is_found_with_its_jtag_node_consoles_and_port() {
        let tree = Tree::new("find");
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", 6, &[0, 4, 5, 6]);
        let cable = find(&tree.sysfs, &tree.dev, "217702174005").unwrap().unwrap();
        assert_eq!(cable.product, "A-U280-P32G");
        assert_eq!(cable.jtag, tree.dev.join("bus/usb/003/006"));
        let consoles: Vec<PathBuf> = ["ttyUSB0", "ttyUSB4", "ttyUSB5", "ttyUSB6"].iter().map(|t| tree.dev.join(t)).collect();
        assert_eq!(cable.consoles, consoles);
        assert!(cable.port_disable.ends_with("usb3-port2/disable"), "{}", cable.port_disable.display());
        assert!(cable.port_disable.exists());
    }

    /// A cable whose interface 0 hw_server detached has three consoles, not four.
    #[test]
    fn three_consoles_are_as_good_as_four() {
        let tree = Tree::new("three");
        tree.cable("3-1", "0403", "XFL1EZVSAG4S", "A-V80", 2, &[1, 2, 3]);
        assert_eq!(find(&tree.sysfs, &tree.dev, "XFL1EZVSAG4S").unwrap().unwrap().consoles.len(), 3);
    }

    #[test]
    fn an_unplugged_cable_is_none() {
        let tree = Tree::new("absent");
        assert_eq!(find(&tree.sysfs, &tree.dev, "217702174005").unwrap(), None);
    }

    #[test]
    fn two_devices_with_one_serial_are_an_error() {
        let tree = Tree::new("dup");
        tree.cable("3-1", "0403", "SAME", "A-V80", 2, &[1]);
        tree.cable("3-2", "0403", "SAME", "A-U280-P32G", 3, &[5]);
        assert!(find(&tree.sysfs, &tree.dev, "SAME").unwrap_err().contains("share serial"));
    }

    #[test]
    fn discover_offers_only_ftdi_and_xilinx_devices() {
        let tree = Tree::new("discover");
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", 3, &[5]);
        tree.cable("1-1", "046d", "MOUSE1", "USB Mouse", 4, &[]);
        let serials: Vec<String> = discover(&tree.sysfs, &tree.dev).into_iter().map(|c| c.serial).collect();
        assert_eq!(serials, vec!["217702174005"]);
    }

    #[test]
    fn only_plain_serials_are_accepted() {
        for good in ["XFL1EZVSAG4S", "217702174005", "a.b_c-d"] {
            assert!(valid_serial(good), "{}", good);
        }
        for bad in ["", "a\"b", "a b", "a,b", "x\ny"] {
            assert!(!valid_serial(bad), "{:?}", bad);
        }
    }
}

#[cfg(test)]
mod rule_tests {
    use super::fake::Tree;
    use super::*;

    fn entry(serial: &str, user: &str) -> (String, String) {
        (serial.to_string(), user.to_string())
    }

    /// Exactly the two lines the spec gives per cable: one for the JTAG node, one for consoles.
    #[test]
    fn each_cable_gets_a_usb_line_and_a_tty_line() {
        let text = render_rules(&[entry("217702174005", "anubhav")]);
        assert!(text.contains(
            "SUBSYSTEM==\"usb\", ENV{DEVTYPE}==\"usb_device\", ATTR{serial}==\"217702174005\", OWNER=\"anubhav\", MODE=\"0600\"\n"
        ));
        assert!(text.contains(
            "SUBSYSTEM==\"tty\", ATTRS{serial}==\"217702174005\", OWNER=\"anubhav\", MODE=\"0600\"\n"
        ));
        assert_eq!(text.lines().filter(|l| l.starts_with("SUBSYSTEM")).count(), 2);
    }

    #[test]
    fn the_rule_file_is_created_with_its_directory() {
        let tree = Tree::new("rules-write");
        let path = tree.root.join("run/udev/rules.d/99-fpgahog.rules");
        write_rules(&path, &[entry("217702174005", "anubhav")]).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("217702174005"));
        assert!(!path.with_extension("rules.tmp").exists(), "temporary file left behind");
    }

    /// No cable held means no rule at all, so a released cable gets udev's defaults back.
    #[test]
    fn an_empty_rule_set_removes_the_file() {
        let tree = Tree::new("rules-empty");
        let path = tree.root.join("99-fpgahog.rules");
        write_rules(&path, &[entry("217702174005", "anubhav")]).unwrap();
        write_rules(&path, &[]).unwrap();
        assert!(!path.exists());
        write_rules(&path, &[]).unwrap(); // removing a file that is already gone is fine
    }
}

#[cfg(test)]
mod plan_tests {
    use super::fake::Tree;
    use super::*;
    use crate::diskstate::{CableLock, Ownership};
    use std::os::unix::fs::PermissionsExt;

    fn wanted(serial: &str, user: &str, uid: u32) -> Wanted {
        Wanted { serial: serial.into(), alias: "u280".into(), user: user.into(), uid }
    }

    fn lock(serial: &str, user: &str, uid: u32) -> CableLock {
        CableLock {
            serial: serial.into(),
            alias: "u280".into(),
            claimant: user.into(),
            claimant_uid: uid,
            jtag_orig: Ownership { uid: 0, gid: 0, mode: 0o666 },
            console_orig: None,
            port_disable: String::new(),
        }
    }

    #[test]
    fn a_newly_claimed_cable_is_recorded() {
        let actions = plan(&[wanted("S1", "anubhav", 2049)], &[], |_| true);
        assert_eq!(actions, vec![Action::Record(wanted("S1", "anubhav", 2049))]);
    }

    #[test]
    fn a_held_cable_needs_nothing() {
        assert!(plan(&[wanted("S1", "anubhav", 2049)], &[lock("S1", "anubhav", 2049)], |_| true).is_empty());
    }

    #[test]
    fn a_released_cable_that_is_plugged_in_is_restored() {
        let actions = plan(&[], &[lock("S1", "anubhav", 2049)], |_| true);
        assert_eq!(actions, vec![Action::Restore(lock("S1", "anubhav", 2049))]);
    }

    /// Nothing to put back: udev's own defaults apply when it is plugged in again.
    #[test]
    fn a_released_cable_that_is_unplugged_is_dropped() {
        let actions = plan(&[], &[lock("S1", "anubhav", 2049)], |_| false);
        assert_eq!(actions, vec![Action::Drop(lock("S1", "anubhav", 2049))]);
    }

    #[test]
    fn a_board_that_changed_hands_is_repointed() {
        let actions = plan(&[wanted("S1", "theo", 1234)], &[lock("S1", "anubhav", 2049)], |_| true);
        assert_eq!(actions, vec![Action::Repoint(wanted("S1", "theo", 1234))]);
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn original_ownership_is_read_from_the_cables_files() {
        let tree = Tree::new("orig-read");
        tree.cable("3-2", "0403", "S1", "A-U280-P32G", 3, &[5]);
        let cable = find(&tree.sysfs, &tree.dev, "S1").unwrap().unwrap();
        set_mode(&cable.jtag, 0o666);
        set_mode(&cable.consoles[0], 0o660);
        let me = unsafe { libc::getuid() };
        let (jtag, console) = original_ownership(Some(&cable), me + 1, 27);
        assert_eq!(jtag.mode, 0o666);
        assert_eq!(jtag.uid, me);
        assert_eq!(console.unwrap().mode, 0o660);
    }

    /// Regression guard for the trap in spec §1: files udev already handed to the claimant must
    /// not be remembered as the original, or release would "restore" the lock.
    #[test]
    fn files_already_locked_for_the_claimant_fall_back_to_defaults() {
        let tree = Tree::new("orig-ours");
        tree.cable("3-2", "0403", "S1", "A-U280-P32G", 3, &[5]);
        let cable = find(&tree.sysfs, &tree.dev, "S1").unwrap().unwrap();
        set_mode(&cable.jtag, 0o600);
        set_mode(&cable.consoles[0], 0o600);
        let me = unsafe { libc::getuid() };
        let (jtag, console) = original_ownership(Some(&cable), me, 27);
        assert_eq!(jtag, Ownership { uid: 0, gid: 0, mode: 0o666 });
        assert_eq!(console, Some(Ownership { uid: 0, gid: 27, mode: 0o660 }));
    }

    #[test]
    fn an_absent_cable_gets_the_defaults() {
        let (jtag, console) = original_ownership(None, 2049, 27);
        assert_eq!(jtag, Ownership { uid: 0, gid: 0, mode: 0o666 });
        assert_eq!(console, Some(Ownership { uid: 0, gid: 27, mode: 0o660 }));
    }
}

#[cfg(test)]
mod sync_tests {
    use super::fake::Tree;
    use super::*;
    use crate::diskstate::{load_default, Claim, DiskState, FpgaSpec, ResourceId};
    use std::os::unix::fs::PermissionsExt;

    fn me() -> String {
        crate::util::get_username(unsafe { libc::getuid() })
    }

    fn state_with_claim(exclusive: bool) -> DiskState {
        let mut state = load_default();
        state.settings.fpgas.push(FpgaSpec {
            alias: "u280".into(),
            bdf: "0000:c1:00.0".into(),
            pci_id: String::new(),
            devices: vec![],
            cables: vec!["217702174005".into()],
        });
        state.claims.push(Claim {
            id: 1,
            timeout: chrono::Local::now() + chrono::Duration::hours(1),
            soft_timeout: None,
            exclusive,
            user: me(),
            comment: String::new(),
            resources: vec![ResourceId::Fpga("u280".into())],
        });
        state
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o7777
    }

    /// Plug the U280 cable in with udev's usual modes: JTAG 0666, consoles 0660.
    fn plug(tree: &Tree, devnum: u32, consoles: &[u32]) -> Cable {
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", devnum, consoles);
        let cable = find(&tree.sysfs, &tree.dev, "217702174005").unwrap().unwrap();
        fs::set_permissions(&cable.jtag, fs::Permissions::from_mode(0o666)).unwrap();
        for console in &cable.consoles {
            fs::set_permissions(console, fs::Permissions::from_mode(0o660)).unwrap();
        }
        cable
    }

    #[test]
    fn an_exclusive_claim_locks_the_cable_and_writes_the_rule() {
        let tree = Tree::new("sync-lock");
        let cable = plug(&tree, 6, &[0, 4]);
        let rules = tree.root.join("rules.d/99-fpgahog.rules");
        let mut state = state_with_claim(true);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        assert_eq!(mode(&cable.jtag), 0o600);
        assert!(cable.consoles.iter().all(|c| mode(c) == 0o600));
        let text = fs::read_to_string(&rules).unwrap();
        assert!(text.contains("217702174005") && text.contains(&format!("OWNER=\"{}\"", me())));
        assert_eq!(state.cable_locks.len(), 1);
        assert_eq!(state.cable_locks[0].jtag_orig.mode, 0o666);
        assert_eq!(state.cable_locks[0].console_orig.unwrap().mode, 0o660);
    }

    #[test]
    fn a_shared_claim_locks_nothing() {
        let tree = Tree::new("sync-shared");
        let cable = plug(&tree, 6, &[0]);
        let rules = tree.root.join("rules.d/99-fpgahog.rules");
        let mut state = state_with_claim(false);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        assert_eq!(mode(&cable.jtag), 0o666);
        assert!(!rules.exists());
        assert!(state.cable_locks.is_empty());
    }

    #[test]
    fn releasing_restores_the_files_and_removes_the_rule() {
        let tree = Tree::new("sync-release");
        let cable = plug(&tree, 6, &[0, 4]);
        let rules = tree.root.join("rules.d/99-fpgahog.rules");
        let mut state = state_with_claim(true);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        state.claims.clear();
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        assert_eq!(mode(&cable.jtag), 0o666);
        assert!(cable.consoles.iter().all(|c| mode(c) == 0o660));
        assert!(!rules.exists());
        assert!(state.cable_locks.is_empty());
    }

    /// Re-enumeration gives the cable new device files; the recorded originals must survive.
    #[test]
    fn a_replugged_cable_is_locked_again() {
        let tree = Tree::new("sync-replug");
        plug(&tree, 6, &[0, 4]);
        let rules = tree.root.join("rules.d/99-fpgahog.rules");
        let mut state = state_with_claim(true);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        tree.remove_cable("3-2");
        let replugged = plug(&tree, 7, &[1, 2, 3]);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        assert_eq!(mode(&replugged.jtag), 0o600);
        assert!(replugged.consoles.iter().all(|c| mode(c) == 0o600));
        assert_eq!(state.cable_locks.len(), 1);
        assert_eq!(state.cable_locks[0].jtag_orig.mode, 0o666);
    }

    #[test]
    fn an_unplugged_released_cable_is_dropped() {
        let tree = Tree::new("sync-drop");
        plug(&tree, 6, &[0]);
        let rules = tree.root.join("rules.d/99-fpgahog.rules");
        let mut state = state_with_claim(true);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        tree.remove_cable("3-2");
        state.claims.clear();
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        assert!(state.cable_locks.is_empty());
        assert!(!rules.exists());
    }

    #[test]
    fn view_reports_whether_the_files_are_really_locked() {
        let tree = Tree::new("sync-view");
        let cable = plug(&tree, 6, &[0]);
        let rules = tree.root.join("rules.d/99-fpgahog.rules");
        let mut state = state_with_claim(true);
        sync(&mut state, &tree.sysfs, &tree.dev, &rules);
        assert_eq!(view(&state, "217702174005", &tree.sysfs, &tree.dev), CableView::Present { locked: true });
        fs::set_permissions(&cable.jtag, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(view(&state, "217702174005", &tree.sysfs, &tree.dev), CableView::Present { locked: false });
        assert_eq!(view(&state, "NOPE", &tree.sysfs, &tree.dev), CableView::Absent);
        assert_eq!(view(&state, "bad serial", &tree.sysfs, &tree.dev), CableView::Invalid);
    }
}

#[cfg(test)]
mod evict_tests {
    use super::fake::Tree;
    use super::*;
    use std::time::Duration;

    #[test]
    fn eviction_disconnects_and_reconnects_the_port_then_waits_for_the_cable() {
        let tree = Tree::new("evict-ok");
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", 6, &[0]);
        let cable = find(&tree.sysfs, &tree.dev, "217702174005").unwrap().unwrap();
        evict(&cable, &tree.sysfs, &tree.dev, Duration::from_secs(2)).unwrap();
        assert_eq!(fs::read_to_string(&cable.port_disable).unwrap(), "0");
    }

    #[test]
    fn a_cable_that_does_not_come_back_is_an_error() {
        let tree = Tree::new("evict-gone");
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", 6, &[0]);
        let cable = find(&tree.sysfs, &tree.dev, "217702174005").unwrap().unwrap();
        fs::remove_file(&cable.jtag).unwrap();
        let err = evict(&cable, &tree.sysfs, &tree.dev, Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("did not come back"), "{}", err);
    }

    /// Regression: the port control was stored as <device>/port/disable, which vanishes with the
    /// device the moment the port is disabled, so re-enabling it failed and the cable stayed dead.
    #[test]
    fn the_port_control_survives_the_device_disappearing() {
        let tree = Tree::new("evict-portpath");
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", 6, &[0]);
        let cable = find(&tree.sysfs, &tree.dev, "217702174005").unwrap().unwrap();
        tree.remove_cable("3-2");
        assert!(cable.port_disable.exists(), "{} went away with the device", cable.port_disable.display());
        fs::write(&cable.port_disable, "0").unwrap();
    }

    #[test]
    fn a_missing_port_control_is_an_error() {
        let tree = Tree::new("evict-noport");
        tree.cable("3-2", "0403", "217702174005", "A-U280-P32G", 6, &[0]);
        let cable = find(&tree.sysfs, &tree.dev, "217702174005").unwrap().unwrap();
        fs::remove_dir_all(cable.port_disable.parent().unwrap()).unwrap();
        let err = evict(&cable, &tree.sysfs, &tree.dev, Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("cannot disable"), "{}", err);
    }
}

#[cfg(test)]
mod describe_tests {
    use super::*;

    #[test]
    fn a_board_without_cables_says_nothing() {
        assert_eq!(describe(true, &[]), None);
    }

    #[test]
    fn each_state_has_its_phrase() {
        assert_eq!(describe(true, &[CableView::Present { locked: true }]).as_deref(), Some("cable locked"));
        assert_eq!(describe(true, &[CableView::Present { locked: false }]).as_deref(), Some("cable NOT locked"));
        assert_eq!(describe(false, &[CableView::Present { locked: false }]).as_deref(), Some("cable unlocked"));
        assert_eq!(describe(true, &[CableView::Absent]).as_deref(), Some("cable absent"));
    }

    #[test]
    fn an_invalid_serial_is_reported_first() {
        let views = [CableView::Present { locked: true }, CableView::Invalid];
        assert_eq!(describe(true, &views).as_deref(), Some("cable serial invalid"));
    }
}

#[cfg(test)]
mod assign_tests {
    use super::*;

    fn board(alias: &str, cables: &[&str]) -> FpgaSpec {
        FpgaSpec {
            alias: alias.into(),
            bdf: String::new(),
            pci_id: String::new(),
            devices: vec![],
            cables: cables.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn cable(serial: &str, product: &str) -> Cable {
        Cable { serial: serial.into(), product: product.into(), jtag: PathBuf::new(), consoles: vec![], port_disable: PathBuf::new() }
    }

    fn rose() -> Vec<Cable> {
        vec![cable("XFL1EZVSAG4S", "A-V80"), cable("217702174005", "A-U280-P32G")]
    }

    #[test]
    fn boards_get_the_cable_named_after_them() {
        let pairs = assign(&[board("v80", &[]), board("u280", &[])], &rose());
        assert_eq!(pairs, vec![
            ("v80".to_string(), "XFL1EZVSAG4S".to_string()),
            ("u280".to_string(), "217702174005".to_string()),
        ]);
    }

    #[test]
    fn placeholder_names_match_nothing() {
        assert!(assign(&[board("fpga0", &[])], &rose()).is_empty());
    }

    #[test]
    fn a_board_matching_two_cables_is_left_for_a_human() {
        assert!(assign(&[board("a", &[])], &rose()).is_empty());
    }

    #[test]
    fn a_cable_matching_two_boards_is_left_for_a_human() {
        assert!(assign(&[board("u280", &[]), board("280", &[])], &rose()).is_empty());
    }

    #[test]
    fn a_board_with_cables_is_never_touched() {
        assert!(assign(&[board("u280", &["SET-BY-HAND"])], &rose()).is_empty());
    }
}

#[cfg(test)]
mod repair_tests {
    use super::fake::Tree;
    use super::*;
    use crate::diskstate::{load_default, CableLock, Ownership};

    fn port_file(tree: &Tree, state: &str) -> PathBuf {
        let port = tree.root.join("usb3-port2/disable");
        fs::create_dir_all(port.parent().unwrap()).unwrap();
        fs::write(&port, state).unwrap();
        port
    }

    fn lock_with_port(serial: &str, port: &Path) -> CableLock {
        CableLock {
            serial: serial.into(),
            alias: "u280".into(),
            claimant: "me".into(),
            claimant_uid: 0,
            jtag_orig: Ownership { uid: 0, gid: 0, mode: 0o666 },
            console_orig: None,
            port_disable: port.to_string_lossy().into_owned(),
        }
    }

    /// Regression: an eviction killed between the two writes left the port disabled for good,
    /// and no later run put it back.
    #[test]
    fn a_pending_replug_is_finished_on_the_next_pass() {
        let tree = Tree::new("repair-pending");
        let port = port_file(&tree, "1");
        let mut state = load_default();
        state.pending_replug = vec![port.to_string_lossy().into_owned()];
        reenable_disabled_ports(&mut state);
        assert_eq!(fs::read_to_string(&port).unwrap().trim(), "0");
        assert!(state.pending_replug.is_empty());
    }

    /// Even with the marker lost: a cable we hold that is gone while its port reads disabled is
    /// one we disabled ourselves.
    #[test]
    fn a_locked_cable_whose_port_is_disabled_is_re_enabled() {
        let tree = Tree::new("repair-lock");
        let port = port_file(&tree, "1");
        let mut state = load_default();
        state.cable_locks.push(lock_with_port("217702174005", &port));
        reenable_disabled_ports(&mut state);
        assert_eq!(fs::read_to_string(&port).unwrap().trim(), "0");
    }

    #[test]
    fn an_enabled_port_is_left_alone() {
        let tree = Tree::new("repair-noop");
        let port = port_file(&tree, "0");
        let mut state = load_default();
        state.pending_replug = vec![port.to_string_lossy().into_owned()];
        reenable_disabled_ports(&mut state);
        assert_eq!(fs::read_to_string(&port).unwrap().trim(), "0");
        assert!(state.pending_replug.is_empty());
    }

    #[test]
    fn status_says_the_port_is_disabled_rather_than_absent() {
        let tree = Tree::new("repair-view");
        let port = port_file(&tree, "1");
        let mut state = load_default();
        state.cable_locks.push(lock_with_port("217702174005", &port));
        assert_eq!(view(&state, "217702174005", &tree.sysfs, &tree.dev), CableView::PortDisabled);
        assert_eq!(describe(true, &[CableView::PortDisabled]).as_deref(), Some("cable port disabled"));
    }

    /// Two evictions in one command must still fit inside the wait other commands give the lock.
    #[test]
    fn eviction_fits_inside_the_statefile_lock_wait() {
        let worst = 2 * (REPLUG_WAIT.as_secs() + 1);
        assert!(
            worst < crate::diskstate::LOCK_WAIT.as_secs(),
            "worst case {}s vs lock wait {}s",
            worst,
            crate::diskstate::LOCK_WAIT.as_secs()
        );
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use crate::diskstate::{load_default, Claim, ResourceId};

    fn board(alias: &str, cables: &[&str]) -> FpgaSpec {
        FpgaSpec {
            alias: alias.into(),
            bdf: String::new(),
            pci_id: String::new(),
            devices: vec![],
            cables: cables.iter().map(|c| c.to_string()).collect(),
        }
    }

    #[test]
    fn a_serial_on_two_boards_is_rejected() {
        assert_eq!(duplicate_serials(&[board("v80", &["S1"]), board("u280", &["S1"])]), vec!["S1".to_string()]);
    }

    #[test]
    fn a_serial_listed_twice_on_one_board_is_rejected() {
        assert_eq!(duplicate_serials(&[board("u280", &["S1", "S1"])]), vec!["S1".to_string()]);
    }

    #[test]
    fn distinct_serials_are_fine() {
        assert!(duplicate_serials(&[board("v80", &["S1"]), board("u280", &["S2"])]).is_empty());
    }

    /// A cable two boards claim is locked for neither, and never reaches the udev rule.
    #[test]
    fn a_duplicate_is_never_locked() {
        let mut state = load_default();
        state.settings.fpgas = vec![board("v80", &["S1"]), board("u280", &["S1"])];
        state.claims = vec![Claim {
            id: 1,
            timeout: chrono::Local::now() + chrono::Duration::hours(1),
            soft_timeout: None,
            exclusive: true,
            user: crate::util::get_username(unsafe { libc::getuid() }),
            comment: String::new(),
            resources: ResourceId::parse_list("v80"),
        }];
        assert!(wanted(&state).is_empty());
    }

    #[test]
    fn assign_skips_a_serial_another_board_already_uses() {
        let specs = [board("v80", &["217702174005"]), board("u280", &[])];
        let cables = vec![Cable {
            serial: "217702174005".into(),
            product: "A-U280-P32G".into(),
            jtag: PathBuf::new(),
            consoles: vec![],
            port_disable: PathBuf::new(),
        }];
        assert!(assign(&specs, &cables).is_empty());
    }
}
