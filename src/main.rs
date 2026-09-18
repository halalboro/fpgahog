use clap::{Args, Parser, Subcommand};
use std::process::Command;
use chrono::prelude::*;

mod hog;
mod diskstate;
mod fpga;
mod cable;
mod users;
mod claims;
mod util;
mod systemd_units;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Args)]
struct StatusCommand {
    #[arg(short, long)]
    /// More detailed status
    verbose: bool,
}

// this does not actually set the default value for the cli
impl Default for StatusCommand {
    fn default() -> Self {
        Self { verbose: false}
    }
}

#[derive(Args)]
pub struct ClaimCommand {
    /// `[RESOURCE] TIMEOUT [COMMENT]...`. RESOURCE is `host` or an FPGA alias from `list`,
    /// comma separated for several (`host,u280`); leave it out to claim the host, exactly like
    /// hosthog's `claim TIMEOUT [COMMENT]...`. TIMEOUT is a duration (`15min`, `4h`) or a date,
    /// after which the claim is removed.
    #[arg(required = true, num_args = 1.., value_name = "[RESOURCE] TIMEOUT [COMMENT]")]
    pub args: Vec<String>,
    /// Optional timeout: will not remove the claim, but will be shown in the status
    #[arg(short, long)]
    pub soft_timeout: Option<String>,
    /// Claim exclusive access. Other new claims will not be allowed. Default for FPGAs.
    #[arg(short, long)]
    pub exclusive: bool,
    /// Announce your use without locking anybody out. Default for `host`.
    #[arg(long)]
    pub shared: bool,
    /// Claim even when the board is in use or cannot be enforced.
    #[arg(short, long)]
    pub force: bool,
}


#[derive(clap::ValueEnum, Clone)]
enum Resource {
    SystemdTimers,
    // SshKeys, // cant be done via this API because it relies on cli params regarding which users
    // to lock out
}


#[derive(Subcommand)]
enum Commands {
    /// show current claims
    Status {
        #[command(flatten)]
        status: StatusCommand,
    },
    /// Claim a resource. Fails if already claimed exclusively.
    Claim {
        #[command(flatten)]
        claim: ClaimCommand,
    },
    /// prematurely release a claim (removes all of your hogs and exclusive claims)
    Release {
        /// Resources to give back. Default: your hogs and exclusive claims, as in hosthog.
        resources: Vec<String>,
    },
    /// List the FPGAs this host offers and who holds them
    List {},
    /// Look for FPGAs on the PCI bus and optionally register them
    Discover {
        /// Add newly found boards to the settings in the statefile
        #[arg(long)]
        write: bool,
    },
    /// Exit 0 if you may use these resources now, 1 if someone else holds them exclusively
    ///
    /// Meant for scripts, e.g. `fpgahog check u280 || exit 1` before programming a board. Needs
    /// no root. Shared claims by other people are mentioned but do not block.
    Check {
        /// `host` or FPGA aliases, comma separated
        resources: String,
    },
    /// Hog the entire host (others will hate you)
    Hog {
        /// Block ssh login for all users except the ones specified here (default: your user and
        /// root). Specify -u multiple times to add more users.
        #[arg(short, long)]
        users: Vec<String>,
        /// Also cut other users off the cables they hold open, by re-plugging them
        #[arg(short, long)]
        force: bool,
    },
    /// post a message to all logged in users
    ///
    /// The message will arrive at:
    /// - login shells (wall)
    /// - all tmux sessions (tmux display-popup)
    Post {
        /// message to post
        message: Vec<String>
    },
    /// List all logged in users
    ///
    /// Checks:
    /// - login shells/ssh sessions (users)
    /// - tmux sessions of all users
    /// - xrdp sessions
    /// - vscode?
    Users {
    },

    #[command(hide(true))]
    /// disable/hog a system resource  (this disables e.g. ssh keys)
    Disable {
        resource: Resource,
    },
    #[command(hide(true))]
    /// enable/release a system resource  (this enables e.g. ssh keys)
    Enable {
        resource: Resource,
    },
    #[command(hide(true))]
    // Internal command used to trigger updating the list of claims and hogs
    Maintenance {}
}

/// Commands that change the host or its state. They are refused up front without root instead
/// of failing at the very end after half their work: a claim used to queue its `at` job first.
fn needs_root(command: &Commands) -> bool {
    match command {
        Commands::Claim { .. }
        | Commands::Release { .. }
        | Commands::Hog { .. }
        | Commands::Disable { .. }
        | Commands::Enable { .. }
        | Commands::Maintenance { .. } => true,
        Commands::Discover { write } => *write,
        Commands::Status { .. }
        | Commands::List { .. }
        | Commands::Post { .. }
        | Commands::Users { .. }
        | Commands::Check { .. } => false,
    }
}

fn show_status_verbose(_cmd: StatusCommand, state: &diskstate::DiskState) {
    println!("{}", serde_yaml::to_string(&state).unwrap());
}

/// How well a board is actually protected, in one phrase. The column exists to separate
/// "claimed" from "enforced": an exclusive claim whose nodes are missing or not ours protects
/// nothing, and must say so instead of reading like a free board ("0/2 locked").
fn enforcement_summary(state: &diskstate::DiskState, spec: &diskstate::FpgaSpec) -> String {
    let claimed = state.claims.iter().any(|c| c.exclusive && c.covers_alias(&spec.alias));
    let views: Vec<cable::CableView> = spec
        .cables
        .iter()
        .map(|serial| cable::view(state, serial, std::path::Path::new(cable::SYSFS), std::path::Path::new(cable::DEV)))
        .collect();
    let cables = cable::describe(claimed, &views);
    let devices = if spec.devices.is_empty() { None } else { Some(device_summary(state, spec)) };
    match (devices, cables) {
        (None, None) => String::from("advisory (no device nodes)"),
        (Some(devices), None) => devices,
        (None, Some(cables)) => cables,
        (Some(devices), Some(cables)) => format!("{}, {}", devices, cables),
    }
}

/// The device-file part of `enforcement_summary`, for a board that has device files.
fn device_summary(state: &diskstate::DiskState, spec: &diskstate::FpgaSpec) -> String {
    let total = spec.devices.len();
    let missing = spec
        .devices
        .iter()
        .filter(|d| !std::path::Path::new(d.as_str()).exists())
        .count();
    let claimed_exclusively = state
        .claims
        .iter()
        .any(|c| c.exclusive && c.covers_alias(&spec.alias));

    if !claimed_exclusively {
        return if missing > 0 {
            format!("unlocked, {} of {} nodes missing", missing, total)
        } else {
            String::from("unlocked")
        };
    }

    let locks = fpga::locks_for(state, &spec.alias);
    let intact = spec
        .devices
        .iter()
        .filter(|path| {
            locks
                .iter()
                .any(|lock| &lock.path == *path && fpga::lock_is_intact(lock) == Some(true))
        })
        .count();
    if intact == total {
        return format!("{}/{} locked", intact, total);
    }
    let mut problem = format!("ENFORCEMENT FAILED: {}/{} locked", intact, total);
    if missing > 0 {
        problem.push_str(&format!(", {} nodes missing", missing));
    }
    problem
}

fn claim_summary(state: &diskstate::DiskState, alias: &str) -> String {
    match state.claims.iter().find(|c| c.covers_alias(alias)) {
        Some(claim) => format!(
            "{} ({}), {} left",
            claim.user,
            if claim.exclusive { "exclusive" } else { "shared" },
            util::format_timeout_abs(claim.timeout)
        ),
        None => String::from("free"),
    }
}

fn show_fpgas(state: &diskstate::DiskState) {
    if state.settings.fpgas.is_empty() {
        let discovered = fpga::discover();
        if !discovered.is_empty() {
            println!(
                "No FPGAs registered yet, but {} FPGA-like PCI device(s) are present.",
                discovered.len()
            );
            println!("Run `sudo {} discover --write` to register them.\n", util::prog_name());
        }
        return;
    }

    let discovered = fpga::discover();
    let rows: Vec<Vec<String>> = state
        .settings
        .fpgas
        .iter()
        .map(|spec| {
            let driver = discovered
                .iter()
                .find(|d| d.bdf == spec.bdf)
                .and_then(|d| d.driver.clone())
                .unwrap_or_else(|| String::from("-"));
            vec![
                spec.alias.clone(),
                spec.bdf.clone(),
                driver,
                claim_summary(state, &spec.alias),
                enforcement_summary(state, spec),
            ]
        })
        .collect();

    println!("FPGAs:");
    print_table(&["Resource", "PCI address", "Driver", "Claim", "Enforcement"], &rows);
    println!("");
}

/// Print rows as left-aligned columns, each as wide as its widest cell. Fixed widths broke as
/// soon as a real driver name (`coyote_driver_ultrascale_plus`) turned up.
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(cell.chars().count());
        }
    }
    let render = |cells: &[String]| -> String {
        let last = cells.len() - 1;
        cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                if column == last {
                    cell.clone()
                } else {
                    format!("{:<width$}", cell, width = widths[column])
                }
            })
            .collect::<Vec<String>>()
            .join("  ")
    };
    let header: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", render(&header));
    for row in rows {
        println!("{}", render(row));
    }
}

fn show_status(_cmd: StatusCommand, state: &diskstate::DiskState) {
    if state.overmounts.len() > 0 {
        println!("");
        if let Some(claim) = &state.hogger {
            println!("{}", hog::ssh_hogged_message(claim));
        }
        let active_overmounts = state.overmounts.iter().filter(|file| hog::is_overmounted(file)).collect::<Vec<&String>>().len();
        println!("{} keys were disabled to hog, {} keys are still disabled", state.overmounts.len(), active_overmounts);
        println!("");
    }

    show_fpgas(state);

    println!("Active claims:");
    let now: DateTime<Local> = Local::now();
    let rows: Vec<Vec<String>> = state
        .claims
        .iter()
        .map(|claim| {
            // show the soft timeout instead of the hard one when there is one
            let remaining = match claim.soft_timeout {
                Some(soft_timeout) => format!("{} (soft)", util::format_timeout(soft_timeout - now)),
                None => util::format_timeout(claim.timeout - now),
            };
            let comment = match claim.exclusive {
                true => format!("(exclusive) {}", claim.comment),
                false => claim.comment.clone(),
            };
            vec![remaining, claim.user.clone(), claim.resources_str(), comment]
        })
        .collect();
    print_table(&["Remaining", "User", "Resources", "Comment"], &rows);
}

fn do_list(state: &diskstate::DiskState) {
    let discovered = fpga::discover();

    if state.settings.fpgas.is_empty() {
        println!("No FPGAs registered on this host.");
    }
    for spec in &state.settings.fpgas {
        let found = discovered.iter().find(|d| d.bdf == spec.bdf);
        println!("{}", spec.alias);
        println!("  pci address  {}", spec.bdf);
        println!("  pci id       {}", spec.pci_id);
        match found {
            Some(found) => {
                println!("  driver       {}", found.driver.clone().unwrap_or_else(|| String::from("(none bound)")));
                println!("  numa node    {}", found.numa_node.clone().unwrap_or_else(|| String::from("?")));
                if found.pci_id != spec.pci_id && !spec.pci_id.is_empty() {
                    println!("  WARNING      the card at this address now reports {}", found.pci_id);
                }
            }
            None => println!("  driver       MISSING: no PCI device at this address"),
        }
        if spec.devices.is_empty() {
            println!("  devices      (none configured: claims cannot be enforced)");
        } else {
            for device in &spec.devices {
                let gone = !std::path::Path::new(device.as_str()).exists();
                println!("  device       {}{}", device, if gone { "  (MISSING)" } else { "" });
            }
        }
        let duplicates = cable::duplicate_serials(&state.settings.fpgas);
        for serial in &spec.cables {
            if duplicates.contains(serial) {
                println!("  cable        {}  (DUPLICATE SERIAL: listed by more than one board)", serial);
                continue;
            }
            if !cable::valid_serial(serial) {
                println!("  cable        {:?}  (INVALID SERIAL)", serial);
                continue;
            }
            match cable::find(std::path::Path::new(cable::SYSFS), std::path::Path::new(cable::DEV), serial) {
                Ok(Some(found)) => {
                    let consoles: Vec<String> = found.consoles.iter().map(|c| c.display().to_string()).collect();
                    println!("  cable        {} {}", found.serial, found.product);
                    println!("    jtag       {}", found.jtag.display());
                    println!("    consoles   {}", if consoles.is_empty() { String::from("-") } else { consoles.join(" ") });
                }
                Ok(None) => println!("  cable        {}  (ABSENT)", serial),
                Err(err) => println!("  cable        {}  ({})", serial, err),
            }
        }
        println!("  claim        {}", claim_summary(state, &spec.alias));
        println!("  enforcement  {}", enforcement_summary(state, spec));
        println!("");
    }

    let unregistered: Vec<&fpga::DiscoveredFpga> = discovered
        .iter()
        .filter(|d| !state.settings.fpgas.iter().any(|s| s.bdf == d.bdf))
        .collect();
    if !unregistered.is_empty() {
        println!("Unregistered FPGA-like PCI devices:");
        for found in unregistered {
            println!("  {}  {}  driver={}", found.bdf, found.pci_id, found.driver.clone().unwrap_or_else(|| String::from("-")));
        }
        println!("Run `sudo {} discover --write` to register them.", util::prog_name());
    }
}

fn do_discover(write: bool, state: &mut diskstate::DiskState) {
    let discovered = fpga::discover();
    if discovered.is_empty() {
        println!("No FPGA-like PCI devices found.");
        return;
    }

    println!("{:<15} {:<12} {:<16} {}", "PCI address", "PCI id", "Driver", "NUMA");
    for found in &discovered {
        println!(
            "{:<15} {:<12} {:<16} {}",
            found.bdf,
            found.pci_id,
            found.driver.clone().unwrap_or_else(|| String::from("-")),
            found.numa_node.clone().unwrap_or_else(|| String::from("?")),
        );
    }
    println!("");

    let cables = cable::discover(std::path::Path::new(cable::SYSFS), std::path::Path::new(cable::DEV));
    if !cables.is_empty() {
        println!("{:<15} {:<16} {}", "Cable serial", "Product", "JTAG node");
        for found in &cables {
            println!("{:<15} {:<16} {}", found.serial, found.product, found.jtag.display());
        }
        println!("");
    }

    if !write {
        println!("Nothing written. Re-run with --write to register these boards.");
        return;
    }

    let (added, updated) = fpga::merge_into_settings(state, &discovered);
    println!("{} board(s) added, {} updated.", added, updated);
    for (alias, serial) in cable::assign(&state.settings.fpgas, &cables) {
        if let Some(spec) = state.settings.fpgas.iter_mut().find(|spec| spec.alias == alias) {
            spec.cables.push(serial.clone());
            println!("{}: cable {} assigned", alias, serial);
        }
    }
    let assigned: Vec<String> = state.settings.fpgas.iter().flat_map(|spec| spec.cables.clone()).collect();
    for found in cables.iter().filter(|c| !assigned.contains(&c.serial)) {
        println!(
            "NOTE: cable {} ({}) is not assigned to a board; add its serial to settings.fpgas[].cables",
            found.serial, found.product
        );
    }
    if added > 0 {
        println!(
            "Registered under placeholder names (fpga0, fpga1, ...). Edit settings.fpgas in\n\
             the statefile to give them real names, and check that each `devices` list holds\n\
             the right character devices before relying on enforcement."
        );
    }
    for spec in &state.settings.fpgas {
        if spec.devices.is_empty() {
            println!(
                "NOTE: {} ({}) has no device nodes, so exclusive claims on it cannot be enforced.",
                spec.alias, spec.bdf
            );
        }
    }
}

/// Run a command to completion and pass on its output. Exits with an error if it fails.
fn run(command: &[String]) {
    let (bin, args) = match command.split_first() {
        Some(split) => split,
        None => {
            eprintln!("nothing to run");
            std::process::exit(1);
        }
    };
    let out = match Command::new(bin).args(args).output() {
        Ok(out) => out,
        Err(err) => {
            eprintln!("cannot run {}: {}", bin, err);
            std::process::exit(1);
        }
    };
    // hosthog printed these as raw byte arrays, "[]" after every post
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        eprintln!("{} failed ({})", bin, out.status);
        std::process::exit(1);
    }
}

fn do_post(mut message: Vec<String>) {
    println!("post message:");
    message.as_slice().into_iter().for_each(|i| print!("{} ", i));
    println!("");
    message.insert(0, String::from("wall"));
    run(&message);
}

/// A timeout as a point in time: a duration from now (`15min`, `4h`) or an absolute date.
fn try_parse_timeout(timeout: &str) -> Result<DateTime<Local>, String> {
    let now: DateTime<Local> = Local::now();
    match duration_str::parse(timeout) {
        Ok(parsed) => match chrono::Duration::from_std(parsed) {
            Ok(duration) => Ok(now + duration),
            Err(err) => Err(format!("timeout `{}` is too long: {}", timeout, err)),
        },
        Err(duration_err) => match dateparser::parse(timeout) {
            Ok(parsed) => Ok(DateTime::from(parsed)),
            Err(date_err) => Err(format!(
                "`{}` is neither a duration ({}) nor a date ({})",
                timeout, duration_err, date_err
            )),
        },
    }
}

fn parse_timeout(timeout: &str) -> DateTime<Local> {
    match try_parse_timeout(timeout) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    }
}

fn do_maintenance(state: &mut diskstate::DiskState) {
    let mut needs_release = false;
    let _dropped = diskstate::maintenance(state, &mut needs_release);
    if needs_release {
        hog::release_host(state);
    }
    if state.hogger.is_none() && state.overmounts.len() != 0 {
        println!("WARN: host is not hogged, yet there still seem to be unexpected overmounts. Attempting to remove.");
        hog::release_ssh(state);
    }
    // Converge device ownership last, once the claim list is final.
    fpga::sync_locks(state);
    claims::keep_locks_asserted(state);
}

fn main() {
    let cli = Cli::parse();

    if let Some(command) = &cli.command {
        if needs_root(command) && !users::is_root() {
            let name = std::env::args().skip(1).find(|arg| !arg.starts_with('-')).unwrap_or_default();
            eprintln!(
                "`{} {}` changes the host, so it must run as root: sudo {} {} ...",
                util::prog_name(), name, util::prog_name(), name
            );
            std::process::exit(1);
        }
    }

    // Commands run one at a time as root; see diskstate::lock. Readers take no lock, because
    // the statefile is only ever replaced whole.
    let _lock = if users::is_root() {
        match diskstate::lock() {
            Ok(lock) => Some(lock),
            Err(err) => {
                eprintln!("{}", err);
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // Baseline for the dirty check below. It must NOT be migrated, otherwise a statefile
    // upgrade compares equal to itself and is never written back.
    let _original_state = diskstate::load_raw();
    let mut state = diskstate::load();
    if let Err(e) = diskstate::check_version(&state) {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    match cli.command {
        Some(Commands::Status { status }) if !status.verbose => {
            show_status(status, &state);
        }
        Some(Commands::Status { status }) if status.verbose => {
            show_status_verbose(status, &state);
        }
        Some(Commands::Claim { claim }) => {
            do_maintenance(&mut state);
            claims::do_claim(&claim, &mut state);
        }
        Some(Commands::Release { resources }) => {
            do_maintenance(&mut state);
            hog::do_release(resources, &mut state);
        }
        Some(Commands::List { }) => {
            do_list(&state);
        }
        Some(Commands::Discover { write }) => {
            do_discover(write, &mut state);
        }
        Some(Commands::Check { resources }) => {
            std::process::exit(claims::do_check(&resources, &state));
        }
        Some(Commands::Hog{ users, force }) => {
            do_maintenance(&mut state);
            hog::do_hog(users, force, &mut state)
        },
        Some(Commands::Post{ message }) => {
            do_post(message)
        },
        Some(Commands::Users { }) => {
            users::do_list_users();
        },
        Some(Commands::Disable{ resource: Resource::SystemdTimers {}}) => {
            if let Err(err) = systemd_units::disable_resource(&mut state) {
                eprintln!("{}", err);
            }
        },
        Some(Commands::Enable{ resource: Resource::SystemdTimers {}}) => {
            if let Err(err) = systemd_units::enable_resource(&mut state) {
                eprintln!("{}", err);
            }
        },
        Some(Commands::Maintenance { }) => {
            do_maintenance(&mut state);
        },
        None => {
            show_status(StatusCommand::default(), &state);
            println!(
                "See more options with: {} help",
                util::prog_name()
            );
        }
        _ => unimplemented!()
    };

    if _original_state != state {
        // println!("state changed, storing");
        diskstate::store(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// hosthog's documented claim syntax must keep parsing, under either binary name.
    #[test]
    fn hosthog_claim_syntax_parses() {
        assert!(Cli::try_parse_from(["hosthog", "claim", "--exclusive", "15min", "some", "benchmarks"]).is_ok());
        assert!(Cli::try_parse_from(["hosthog", "claim", "15min"]).is_ok());
        assert!(Cli::try_parse_from(["fpgahog", "claim", "u280", "4h", "perf"]).is_ok());
    }

    /// The hosthog form hinges on telling a timeout from a board name, and dateparser is
    /// lenient, so this checks the real parser rather than a stand-in.
    #[test]
    fn real_timeouts_are_not_mistaken_for_board_names() {
        for timeout in ["15min", "4h", "30s", "2026-12-24 10:00"] {
            assert!(try_parse_timeout(timeout).is_ok(), "{} should read as a timeout", timeout);
        }
        for name in ["host", "u280", "v80", "fpga0", "fpga1"] {
            assert!(try_parse_timeout(name).is_err(), "{} should not read as a timeout", name);
        }
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    fn command(args: &[&str]) -> Commands {
        Cli::try_parse_from(args.iter().copied()).unwrap().command.unwrap()
    }

    /// Regression: a non-root `claim` got as far as queueing its `at` job before failing on
    /// the root check at the very end.
    #[test]
    fn commands_that_change_the_host_need_root() {
        for args in [
            &["fpgahog", "claim", "u280", "4h"][..],
            &["fpgahog", "release"][..],
            &["fpgahog", "hog"][..],
            &["fpgahog", "maintenance"][..],
            &["fpgahog", "discover", "--write"][..],
        ] {
            assert!(needs_root(&command(args)), "{:?} should need root", args);
        }
    }

    /// Cutting other people off a cable happens only when asked for, as with `claim`.
    #[test]
    fn hog_takes_force() {
        assert!(Cli::try_parse_from(["fpgahog", "hog", "--force"]).is_ok());
        assert!(Cli::try_parse_from(["fpgahog", "hog"]).is_ok());
    }

    #[test]
    fn looking_does_not_need_root() {
        for args in [
            &["fpgahog", "status"][..],
            &["fpgahog", "list"][..],
            &["fpgahog", "discover"][..],
            &["fpgahog", "users"][..],
            &["fpgahog", "check", "u280"][..],
        ] {
            assert!(!needs_root(&command(args)), "{:?} should not need root", args);
        }
    }
}
