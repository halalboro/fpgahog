use chrono::{DateTime, Duration, Local};
use crate::ClaimCommand;
use crate::cable;
use crate::diskstate::{Claim, DiskState, ResourceId, HOST_RESOURCE};
use crate::fpga;
use crate::{parse_timeout, try_parse_timeout};
use crate::users;
use crate::util;
use std::io::ErrorKind;
use std::io::Write;
use std::process::{Command, Stdio};

fn next_minute(timeout: DateTime<Local>) -> DateTime<Local> {
    // timeout is in same minute. `at` cant handle that because it ignores seconds.
    // Hence we always have to select the next minute.
    return timeout + Duration::seconds(61);
}

fn schedule_maintenance(timeout: DateTime<Local>) {
    let timeout = next_minute(timeout);
    let binary = util::prog_for_later();
    if util::is_build_output(std::path::Path::new(&binary)) {
        println!(
            "WARN: the job below runs {}, a build output. If it is gone by then, the claim is not ended on time. Install with `just install`.",
            binary
        );
    }
    // Log to the journal instead of letting at mail the output of every re-check.
    let future_command = format!("{} maintenance 2>&1 | logger -t fpgahog", binary);
    let future = format!("{}", timeout.format("%H:%M %Y-%m-%d"));
    println!("Scheduling job {} at {}", future_command, future);
    match Command::new("at").arg(future).stdin(Stdio::piped()).spawn() {
        Ok(mut command) => {
            {
                let mut stdin = command.stdin.take().expect("Failed to open stdin");
                stdin.write_all(future_command.as_bytes()).expect("Failed to write to stdin");
            }
            let exit = command.wait().expect("Command didnt run");
            if !exit.success() {
                println!("Scheduling maintenance failed");
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {
            println!("Claim may not expire in time (program `at` is missing).");
        }
        Err(err) => {
            println!("Claim may not expire in time: at: {}", err);
        },
    }
}

pub struct Conflict {
    pub resource: ResourceId,
    pub user: String,
    pub until: DateTime<Local>,
    pub exclusive: bool,
}

/// Claims by other users that stand in the way. Two shared claims on one resource are fine;
/// anything involving an exclusive claim is not. Resources we do not ask for never collide,
/// which is the whole point of per-device claims.
pub fn find_conflicts(
    state: &DiskState,
    me: &str,
    requested: &[ResourceId],
    exclusive: bool,
) -> Vec<Conflict> {
    let mut conflicts = vec![];
    for resource in requested {
        for claim in state.foreign_claims_on(resource, me) {
            if !exclusive && !claim.exclusive {
                continue; // sharing is allowed
            }
            conflicts.push(Conflict {
                resource: resource.clone(),
                user: claim.user.clone(),
                until: claim.timeout,
                exclusive: claim.exclusive,
            });
        }
    }
    conflicts
}

fn report_conflicts(conflicts: &[Conflict]) {
    eprintln!("Claim refused. Already claimed by someone else:");
    for conflict in conflicts {
        eprintln!(
            "  {:<10} {} ({}) for another {}",
            conflict.resource.to_string(),
            conflict.user,
            if conflict.exclusive { "exclusive" } else { "shared" },
            util::format_timeout_abs(conflict.until),
        );
    }
}

/// How often a live exclusive board claim re-applies its locks.
pub const RECHECK_MINUTES: i64 = 5;

/// When to queue the next lock re-check, or None if nothing needs queueing now. A live
/// exclusive claim on a board with device nodes keeps one job pending at all times, because a
/// driver reload recreates the nodes world-writable and nothing else would notice.
pub fn next_recheck(state: &DiskState, now: DateTime<Local>) -> Option<DateTime<Local>> {
    if !guards_a_board(state, now) {
        return None;
    }
    match state.recheck_at {
        Some(pending) if pending > now => None,
        _ => Some(now + Duration::minutes(RECHECK_MINUTES)),
    }
}

/// Whether a live exclusive claim covers a board with device files or a cable to protect.
fn guards_a_board(state: &DiskState, now: DateTime<Local>) -> bool {
    state
        .claims
        .iter()
        .filter(|c| c.exclusive && c.timeout > now)
        .any(|c| {
            c.resources.iter().filter_map(|r| r.alias()).any(|alias| {
                state.settings.fpga(alias).is_some_and(|spec| !spec.devices.is_empty() || !spec.cables.is_empty())
            })
        })
}

/// Queue the next re-check when one is due, and forget the chain once no board needs it.
pub fn keep_locks_asserted(state: &mut DiskState) {
    let now = Local::now();
    if !guards_a_board(state, now) {
        state.recheck_at = None;
        return;
    }
    if let Some(at) = next_recheck(state, now) {
        schedule_maintenance(at);
        state.recheck_at = Some(at);
    }
}

/// What stands between the caller and a set of resources.
pub struct CheckOutcome {
    /// exclusive claims by other people
    pub blocking: Vec<Claim>,
    /// shared claims by other people
    pub sharing: Vec<Claim>,
}

pub fn check_access(state: &DiskState, me: &str, requested: &[ResourceId], now: DateTime<Local>) -> CheckOutcome {
    let mut outcome = CheckOutcome { blocking: vec![], sharing: vec![] };
    for claim in state.claims.iter().filter(|c| c.user != me && c.timeout > now) {
        if !requested.iter().any(|resource| claim.covers(resource)) {
            continue;
        }
        if claim.exclusive {
            outcome.blocking.push(claim.clone());
        } else {
            outcome.sharing.push(claim.clone());
        }
    }
    outcome
}

/// `check`: exit code 0 if the caller may use the resources now, 1 if someone else holds one
/// exclusively, 2 if the request itself is wrong. Read-only, so scripts need no root.
pub fn do_check(text: &str, state: &DiskState) -> i32 {
    let requested = ResourceId::parse_list(text);
    if requested.is_empty() {
        eprintln!("No resource given. Try `{} list`.", util::prog_name());
        return 2;
    }
    if let Err(err) = fpga::resolve(state, &requested) {
        eprintln!("{}", err);
        return 2;
    }
    // Without a login name every claim counts as someone else's, which errs on the safe side.
    let me = users::my_username().unwrap_or_default();
    let outcome = check_access(state, &me, &requested, Local::now());
    for claim in &outcome.sharing {
        println!(
            "note: {} is also in use by {} (shared, {} left){}",
            claim.resources_str(),
            claim.user,
            util::format_timeout_abs(claim.timeout),
            describe(claim)
        );
    }
    for claim in &outcome.blocking {
        eprintln!(
            "{} is claimed exclusively by {} for another {}{}",
            claim.resources_str(),
            claim.user,
            util::format_timeout_abs(claim.timeout),
            describe(claim)
        );
    }
    if outcome.blocking.is_empty() { 0 } else { 1 }
}

fn describe(claim: &Claim) -> String {
    if claim.comment.is_empty() {
        String::new()
    } else {
        format!(": {}", claim.comment)
    }
}

/// A claim's positional arguments, sorted into their roles.
#[derive(Debug, PartialEq)]
pub struct ClaimArgs {
    pub resources: String,
    pub timeout: String,
    pub comment: Vec<String>,
}

/// Sort `claim`'s positional arguments into their roles. Two forms are accepted:
///
///   fpgahog:  <resource[,resource...]> <timeout> [comment...]
///   hosthog:  <timeout> [comment...]            claims the host, as hosthog always did
///
/// Only the first word decides. If it names known resources, it is the fpgahog form; if it
/// reads as a timeout instead, it is the hosthog form. An FPGA alias that also reads as a
/// timeout is refused, because the two forms would disagree about what it means.
pub fn split_claim_args(
    args: &[String],
    known: &[String],
    is_timeout: impl Fn(&str) -> bool,
) -> Result<ClaimArgs, String> {
    let first = match args.first() {
        Some(first) => first,
        None => return Err(String::from("missing timeout")),
    };

    let named = ResourceId::parse_list(first);
    let names_known_resources = !named.is_empty()
        && named
            .iter()
            .all(|resource| known.iter().any(|k| *k == resource.to_string()));

    if names_known_resources {
        if let Some(alias) = named.iter().filter_map(|r| r.alias()).find(|alias| is_timeout(alias)) {
            return Err(format!(
                "FPGA alias `{}` also reads as a timeout, so `claim {} ...` is ambiguous. Rename it in settings.fpgas.",
                alias, alias
            ));
        }
        return match args.get(1) {
            Some(timeout) => Ok(ClaimArgs {
                resources: first.clone(),
                timeout: timeout.clone(),
                comment: args[2..].to_vec(),
            }),
            None => Err(format!("missing timeout after `{}`", first)),
        };
    }

    if is_timeout(first) {
        return Ok(ClaimArgs {
            resources: HOST_RESOURCE.to_string(),
            timeout: first.clone(),
            comment: args[1..].to_vec(),
        });
    }

    Err(format!(
        "unknown resource `{}`. Known resources: {}.",
        first,
        known.join(", ")
    ))
}

pub fn do_claim(cmd: &ClaimCommand, state: &mut DiskState) {
    let me = match users::my_username() {
        Some(me) => me,
        None => {
            eprintln!("Cannot determine your username, refusing to claim.");
            std::process::exit(1);
        }
    };

    let mut known = vec![HOST_RESOURCE.to_string()];
    known.extend(state.settings.aliases());
    let args = match split_claim_args(&cmd.args, &known, |text| try_parse_timeout(text).is_ok()) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{}", err);
            if err.starts_with("unknown resource") {
                eprintln!("Run `{} discover --write` to register this host's FPGAs.", util::prog_name());
            }
            std::process::exit(1);
        }
    };
    let requested = ResourceId::parse_list(&args.resources);

    // Fail before touching anything if an alias is not configured on this host.
    let specs = match fpga::resolve(state, &requested) {
        Ok(specs) => specs.into_iter().cloned().collect::<Vec<_>>(),
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    };

    // An FPGA is exclusive by default: a board handed to two people at once is rarely what
    // anybody meant. `host` keeps hosthog's opt-in behaviour.
    let exclusive = if cmd.exclusive {
        true
    } else if cmd.shared {
        false
    } else {
        !specs.is_empty()
    };
    if cmd.exclusive && cmd.shared {
        eprintln!("--exclusive and --shared contradict each other.");
        std::process::exit(1);
    }

    let conflicts = find_conflicts(state, &me, &requested, exclusive);
    if !conflicts.is_empty() {
        report_conflicts(&conflicts);
        std::process::exit(1);
    }

    if exclusive {
        if !check_enforceable(&specs, cmd.force) {
            std::process::exit(1);
        }
        if !check_free(&specs, &me, cmd.force) {
            std::process::exit(1);
        }
    }

    let timeout = parse_timeout(&args.timeout);
    let soft_timeout = match &cmd.soft_timeout {
        Some(soft_timeout) => Some(parse_timeout(soft_timeout)),
        None => None,
    };
    let claim = Claim {
        id: state.next_claim_id(),
        timeout,
        soft_timeout,
        exclusive,
        user: me.clone(),
        comment: args.comment.join(" "),
        resources: requested,
    };

    state.claims.push(claim.clone());

    println!(
        "Claimed {} until {} ({}){}",
        claim.resources_str(),
        claim.timeout.format("%Y-%m-%d %H:%M"),
        if exclusive { "exclusive" } else { "shared" },
        if claim.comment.is_empty() { String::new() } else { format!(": {}", claim.comment) },
    );

    // Hand over the device nodes (and pick up anything that drifted while we were away).
    fpga::sync_locks(state);
    keep_locks_asserted(state);
    // Cut other users off the claimed cables they still hold open (spec §4).
    if exclusive && cmd.force {
        let held = cable::foreign_cable_holders(&specs, util::get_uid(&me));
        cable::evict_held(state, &held);
        fpga::sync_locks(state);
    }
    schedule_maintenance(timeout);
}

/// Refuse an exclusive claim on a board we have no way to protect, unless forced. Recording
/// a claim that silently enforces nothing is worse than saying so.
fn check_enforceable(specs: &[crate::diskstate::FpgaSpec], force: bool) -> bool {
    let unenforceable: Vec<&crate::diskstate::FpgaSpec> =
        specs.iter().filter(|s| s.devices.is_empty() && s.cables.is_empty()).collect();
    if unenforceable.is_empty() {
        return true;
    }
    for spec in &unenforceable {
        eprintln!(
            "{} ({}) has no device nodes or cables configured, so an exclusive claim cannot be enforced.",
            spec.alias, spec.bdf
        );
    }
    if force {
        println!("--force given: recording an advisory claim that blocks nobody.");
        return true;
    }
    eprintln!(
        "Load the board's driver and re-run `{} discover --write`, add device nodes or cable\n\
         serials to settings.fpgas[] by hand, or pass --force to take an advisory claim.",
        util::prog_name()
    );
    false
}

/// Refuse to take a board out from under a running job, unless forced.
fn check_free(specs: &[crate::diskstate::FpgaSpec], me: &str, force: bool) -> bool {
    let my_uid = util::get_uid(me);
    let mut blocked = false;
    for spec in specs {
        let mut watched = spec.devices.clone();
        watched.extend(cable::board_paths(spec));
        let foreign: Vec<fpga::Holder> = fpga::holders(&watched)
            .into_iter()
            .filter(|h| Some(h.uid) != my_uid)
            .collect();
        if foreign.is_empty() {
            continue;
        }
        blocked = true;
        eprintln!("{} is in use by:", spec.alias);
        for holder in &foreign {
            eprintln!(
                "  pid {:<8} {} ({})",
                holder.pid, holder.cmdline, holder.user
            );
            eprintln!("  {:<12} holding {}", "", holder.device);
        }
    }
    if !blocked {
        return true;
    }
    if force {
        println!("--force given: claiming anyway. Cables they hold open are re-plugged; open device files keep working until closed.");
        return true;
    }
    eprintln!(
        "\nClaim refused. Re-run with --force to claim anyway\n\
         (their cables are re-plugged, which cuts them off; open device files keep working until closed)."
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diskstate::{Claim, Settings};
    use chrono::Duration as ChronoDuration;

    fn claim_of(user: &str, resources: &[&str], exclusive: bool) -> Claim {
        Claim {
            id: 1,
            timeout: Local::now() + ChronoDuration::hours(1),
            soft_timeout: None,
            exclusive,
            user: user.to_string(),
            comment: String::new(),
            resources: resources.iter().map(|r| ResourceId::parse(r)).collect(),
        }
    }

    fn state_with(claims: Vec<Claim>) -> DiskState {
        DiskState {
            hogger: None,
            overmounts: vec![],
            claims,
            settings: Settings { authorized_keys_file: vec![], fpgas: vec![] },
            disabled_systemd_units: vec![],
            device_locks: vec![],
            cable_locks: vec![],
            pending_replug: vec![],
            recheck_at: None,
            state_version: 3,
        }
    }

    /// The whole point of per-device claims: two boards, two people, no collision.
    #[test]
    fn different_fpgas_never_collide() {
        let state = state_with(vec![claim_of("colleague", &["v80"], true)]);
        let conflicts = find_conflicts(&state, "me", &[ResourceId::parse("u280")], true);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn exclusive_claims_collide_on_the_same_board() {
        let state = state_with(vec![claim_of("colleague", &["u280"], true)]);
        let conflicts = find_conflicts(&state, "me", &[ResourceId::parse("u280")], true);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].user, "colleague");
        assert_eq!(conflicts[0].resource, ResourceId::Fpga("u280".into()));
    }

    #[test]
    fn shared_claims_coexist_but_an_exclusive_one_does_not() {
        let state = state_with(vec![claim_of("colleague", &["u280"], false)]);
        assert!(find_conflicts(&state, "me", &[ResourceId::parse("u280")], false).is_empty());
        assert_eq!(find_conflicts(&state, "me", &[ResourceId::parse("u280")], true).len(), 1);

        let state = state_with(vec![claim_of("colleague", &["u280"], true)]);
        assert_eq!(find_conflicts(&state, "me", &[ResourceId::parse("u280")], false).len(), 1);
    }

    #[test]
    fn my_own_claims_do_not_block_me() {
        let state = state_with(vec![claim_of("me", &["u280"], true)]);
        assert!(find_conflicts(&state, "me", &[ResourceId::parse("u280")], true).is_empty());
    }

    /// A host claim is just another resource: it must not shadow the boards.
    #[test]
    fn host_and_fpga_claims_are_independent() {
        let state = state_with(vec![claim_of("colleague", &["host"], true)]);
        assert!(find_conflicts(&state, "me", &[ResourceId::parse("u280")], true).is_empty());
        assert_eq!(find_conflicts(&state, "me", &[ResourceId::Host], true).len(), 1);
    }
}

#[cfg(test)]
mod claim_form_tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn known() -> Vec<String> {
        words("host v80 u280")
    }

    /// Stand-in for real timeout parsing ("15min", "4h"), so these tests are about argument
    /// order alone. The real parser is checked against board names in main.rs.
    fn timeoutish(text: &str) -> bool {
        let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        !digits.is_empty() && digits.len() < text.len() && digits.chars().all(|c| c.is_ascii_digit())
    }

    /// hosthog's README example: no resource, so it claims the host.
    #[test]
    fn hosthog_form_claims_the_host() {
        let parsed = split_claim_args(&words("15min some benchmarks"), &known(), timeoutish).unwrap();
        assert_eq!(
            parsed,
            ClaimArgs { resources: "host".into(), timeout: "15min".into(), comment: words("some benchmarks") }
        );
    }

    #[test]
    fn fpgahog_form_names_the_resource_first() {
        let parsed = split_claim_args(&words("u280 4h perf tests"), &known(), timeoutish).unwrap();
        assert_eq!(
            parsed,
            ClaimArgs { resources: "u280".into(), timeout: "4h".into(), comment: words("perf tests") }
        );
        let several = split_claim_args(&words("host,v80 2h"), &known(), timeoutish).unwrap();
        assert_eq!(several.resources, "host,v80");
        assert!(several.comment.is_empty());
    }

    /// Only the first word decides the form: a hosthog comment that mentions a board is still
    /// a comment.
    #[test]
    fn a_board_name_inside_a_hosthog_comment_stays_a_comment() {
        let parsed = split_claim_args(&words("2h u280 tests"), &known(), timeoutish).unwrap();
        assert_eq!(parsed.resources, "host");
        assert_eq!(parsed.comment, words("u280 tests"));
    }

    #[test]
    fn a_resource_without_a_timeout_is_refused() {
        let err = split_claim_args(&words("u280"), &known(), timeoutish).unwrap_err();
        assert!(err.contains("timeout"), "{}", err);
    }

    #[test]
    fn an_unknown_word_that_is_not_a_timeout_is_refused() {
        let err = split_claim_args(&words("u2800 4h"), &known(), timeoutish).unwrap_err();
        assert!(err.contains("unknown resource `u2800`"), "{}", err);
    }

    /// An alias that reads as a timeout would make `claim 1h ...` mean different things in the
    /// two forms, so it is refused rather than guessed at.
    #[test]
    fn an_alias_that_reads_as_a_timeout_is_refused() {
        let err = split_claim_args(&words("1h 2h"), &words("host 1h"), timeoutish).unwrap_err();
        assert!(err.contains("`1h`"), "{}", err);
        assert!(err.to_lowercase().contains("rename"), "{}", err);
    }
}

#[cfg(test)]
mod recheck_and_check_tests {
    use super::*;
    use crate::diskstate::{load_default, FpgaSpec};
    use chrono::Duration as Span;

    fn state() -> DiskState {
        let mut state = load_default();
        state.settings.fpgas = vec![
            FpgaSpec { alias: "u280".into(), bdf: "0000:c1:00.0".into(), pci_id: String::new(), devices: vec!["/dev/u280_v0".into()], cables: vec![] },
            FpgaSpec { alias: "v80".into(), bdf: "0000:61:00.0".into(), pci_id: String::new(), devices: vec![], cables: vec![] },
        ];
        state
    }

    fn claim(user: &str, resources: &str, exclusive: bool, left: Span) -> Claim {
        Claim {
            id: 0,
            timeout: Local::now() + left,
            soft_timeout: None,
            exclusive,
            user: user.into(),
            comment: String::new(),
            resources: ResourceId::parse_list(resources),
        }
    }

    /// Nothing to guard: a host claim, a shared board claim, and a board without device nodes.
    #[test]
    fn no_recheck_without_an_enforceable_exclusive_board_claim() {
        let mut s = state();
        s.claims = vec![
            claim("me", "host", true, Span::hours(1)),
            claim("me", "u280", false, Span::hours(1)),
            claim("me", "v80", true, Span::hours(1)),
        ];
        assert_eq!(next_recheck(&s, Local::now()), None);
    }

    /// Regression: locks were re-applied only when someone ran a command or the claim expired,
    /// so a driver reload, which program_fpga.sh does on every run, left the board open.
    #[test]
    fn an_exclusive_board_claim_keeps_a_recheck_queued() {
        let mut s = state();
        s.claims = vec![claim("me", "u280", true, Span::hours(4))];
        let now = Local::now();
        assert_eq!(next_recheck(&s, now), Some(now + Span::minutes(RECHECK_MINUTES)));
    }

    /// Every command runs maintenance, so without this each one would queue yet another job.
    #[test]
    fn a_pending_recheck_is_not_queued_twice() {
        let mut s = state();
        s.claims = vec![claim("me", "u280", true, Span::hours(4))];
        let now = Local::now();
        s.recheck_at = Some(now + Span::minutes(3));
        assert_eq!(next_recheck(&s, now), None);
    }

    /// A recheck that already ran, or was missed because atd was down, gets replaced.
    #[test]
    fn a_recheck_in_the_past_is_replaced() {
        let mut s = state();
        s.claims = vec![claim("me", "u280", true, Span::hours(4))];
        let now = Local::now();
        s.recheck_at = Some(now - Span::minutes(1));
        assert_eq!(next_recheck(&s, now), Some(now + Span::minutes(RECHECK_MINUTES)));
    }

    #[test]
    fn an_expired_claim_stops_the_rechecks() {
        let mut s = state();
        s.claims = vec![claim("me", "u280", true, Span::minutes(-1))];
        assert_eq!(next_recheck(&s, Local::now()), None);
    }

    #[test]
    fn a_free_board_is_usable() {
        let outcome = check_access(&state(), "me", &ResourceId::parse_list("u280"), Local::now());
        assert!(outcome.blocking.is_empty() && outcome.sharing.is_empty());
    }

    #[test]
    fn someone_elses_exclusive_claim_blocks() {
        let mut s = state();
        s.claims = vec![claim("colleague", "u280", true, Span::hours(1))];
        let outcome = check_access(&s, "me", &ResourceId::parse_list("u280"), Local::now());
        assert_eq!(outcome.blocking.len(), 1);
        assert_eq!(outcome.blocking[0].user, "colleague");
    }

    #[test]
    fn my_own_exclusive_claim_does_not_block_me() {
        let mut s = state();
        s.claims = vec![claim("me", "u280", true, Span::hours(1))];
        assert!(check_access(&s, "me", &ResourceId::parse_list("u280"), Local::now()).blocking.is_empty());
    }

    #[test]
    fn a_shared_claim_is_reported_without_blocking() {
        let mut s = state();
        s.claims = vec![claim("colleague", "u280", false, Span::hours(1))];
        let outcome = check_access(&s, "me", &ResourceId::parse_list("u280"), Local::now());
        assert!(outcome.blocking.is_empty());
        assert_eq!(outcome.sharing.len(), 1);
    }

    /// Maintenance has not removed it yet, but it no longer holds anything.
    #[test]
    fn an_expired_claim_is_ignored() {
        let mut s = state();
        s.claims = vec![claim("colleague", "u280", true, Span::minutes(-5))];
        assert!(check_access(&s, "me", &ResourceId::parse_list("u280"), Local::now()).blocking.is_empty());
    }

    /// A hog's claim covers every board, so it blocks programming any of them.
    #[test]
    fn a_hog_blocks_every_board() {
        let mut s = state();
        s.claims = vec![claim("colleague", "host,v80,u280", true, Span::hours(1))];
        assert_eq!(check_access(&s, "me", &ResourceId::parse_list("v80"), Local::now()).blocking.len(), 1);
    }
}

#[cfg(test)]
mod cable_enforcement_tests {
    use super::*;
    use crate::diskstate::{load_default, FpgaSpec};

    fn board_with_only_a_cable() -> FpgaSpec {
        FpgaSpec {
            alias: "v80".into(),
            bdf: "0000:61:00.0".into(),
            pci_id: String::new(),
            devices: vec![],
            cables: vec!["XFL1EZVSAG4S".into()],
        }
    }

    /// A board whose driver is unloaded still has a cable to protect, so it needs re-checks.
    #[test]
    fn a_board_with_only_a_cable_keeps_a_recheck_queued() {
        let mut state = load_default();
        state.settings.fpgas = vec![board_with_only_a_cable()];
        state.claims = vec![Claim {
            id: 1,
            timeout: Local::now() + Duration::hours(1),
            soft_timeout: None,
            exclusive: true,
            user: "me".into(),
            comment: String::new(),
            resources: ResourceId::parse_list("v80"),
        }];
        assert!(next_recheck(&state, Local::now()).is_some());
    }

    #[test]
    fn a_board_with_only_a_cable_is_enforceable() {
        assert!(check_enforceable(&[board_with_only_a_cable()], false));
    }

    #[test]
    fn a_board_with_neither_is_still_refused() {
        let mut spec = board_with_only_a_cable();
        spec.cables.clear();
        assert!(!check_enforceable(&[spec], false));
    }
}
