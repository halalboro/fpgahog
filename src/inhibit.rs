//! Hold off reboots and suspends while anyone has a live claim.
//!
//! A logind inhibitor is a file descriptor: it lasts exactly as long as the process holding
//! it, and fpgahog is a one-shot command. So `fpgahog inhibit-hold` takes the lock and parks,
//! systemd keeps that process alive in a transient unit, and the ordinary commands only
//! reconcile that unit against the claims on disk -- the same shape as the device and cable
//! locks, and self-healing through the same five-minute re-check.
//!
//! What this does and does not stop is systemd's choice, not ours. `systemctl` skips the
//! inhibitor check when it is not talking to a terminal, and skips it for any *scheduled*
//! shutdown (`shutdown -r +1440`), which is how unattended updates reboot a host. So a block
//! inhibitor stops a colleague typing `sudo reboot` and leaves the auto-updater alone. It is
//! a sign on the door, not a lock: `systemctl reboot -i` goes through.

use crate::diskstate::DiskState;
use crate::util;
use chrono::{DateTime, Local};
use zbus_systemd::zbus;
use zbus_systemd::zvariant::{OwnedValue, Value};

/// The transient unit that parks the process holding the lock.
pub const UNIT: &str = "fpgahog-inhibit.service";
/// Identifies our lock in `systemd-inhibit --list`, and tells us which one is ours.
const WHO: &str = "fpgahog";
const WHAT: &str = "shutdown:sleep";
const MODE: &str = "block";
/// How long to wait for the parked process to actually take the lock before complaining.
const TAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long to wait for a stopped holder to stop being a loaded unit.
const UNIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

type ExResult<T> = Result<T, Box<dyn std::error::Error + 'static>>;

/// Why reboots and suspends should be held off right now, or None when nothing is claimed.
///
/// The text is also the comparison key: it must name the same claims the same way on every
/// run, or every re-check would see a difference and restart the holder. That is why it names
/// an absolute expiry rather than a countdown, and why that expiry is rendered in UTC: `TZ`
/// survives `sudo` and `at`, so two people in different zones would otherwise disagree about
/// the same unchanged claim and tear the holder down in turn.
pub fn reason(state: &DiskState, now: DateTime<Local>) -> Option<String> {
    let holders: Vec<String> = state
        .claims
        .iter()
        .filter(|claim| claim.inhibit && claim.active_at(now))
        .map(|claim| {
            format!(
                "{} claimed by {} until {}",
                claim.resources_str(),
                claim.user,
                match claim.timeout {
                    Some(timeout) => timeout
                        .with_timezone(&chrono::Utc)
                        .format("%H:%M UTC on %Y-%m-%d")
                        .to_string(),
                    None => String::from("it is released"),
                }
            )
        })
        .collect();
    if holders.is_empty() {
        return None;
    }
    Some(format!(
        "{}. Ask them first; reboot anyway with `systemctl reboot -i`.",
        holders.join("; ")
    ))
}

/// Bring the inhibitor in line with the claims. Never fatal: a host without logind, or a
/// systemd that refuses us, should still get its device locks.
pub fn sync(state: &DiskState) {
    if let Err(err) = reconcile(state) {
        println!("WARN: cannot hold off reboots and suspends: {}", err);
    }
}

fn reconcile(state: &DiskState) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("cannot start an async runtime: {}", e))?;
    runtime.block_on(reconcile_async(state)).map_err(|e| e.to_string())
}

async fn reconcile_async(state: &DiskState) -> ExResult<()> {
    let wanted = reason(state, Local::now());
    let conn = zbus::Connection::system().await?;
    if held_reason(&conn).await? == wanted {
        return Ok(());
    }
    // The old lock names claims that have changed, so it has to go first.
    clear_holder(&conn).await?;
    let Some(why) = wanted else {
        return Ok(());
    };
    start_holder(&conn, &why).await?;
    // Starting the unit only means systemd queued the job; the process takes the lock a
    // moment later, and a binary that cannot run at all fails here rather than above.
    let deadline = std::time::Instant::now() + TAKE_WAIT;
    loop {
        if held_reason(&conn).await?.as_deref() == Some(why.as_str()) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{} did not take the lock in {:?}; see `systemctl status {}`",
                UNIT, TAKE_WAIT, UNIT
            )
            .into());
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The `why` of our own inhibitor, as logind currently sees it. `who` and `why` are free text
/// that any local user may register, so the lock only counts as ours when everything else
/// about it matches too -- otherwise a `delay` inhibitor with a copied `why`, which blocks
/// nothing and needs no privilege, would convince us the host was protected.
async fn held_reason(conn: &zbus::Connection) -> ExResult<Option<String>> {
    let manager = zbus_systemd::login1::ManagerProxy::new(conn).await?;
    Ok(manager
        .list_inhibitors()
        .await?
        .into_iter()
        .find(|(what, who, _why, mode, uid, _pid)| {
            who == WHO && what == WHAT && mode == MODE && *uid == 0
        })
        .map(|(_what, _who, why, _mode, _uid, _pid)| why))
}

async fn start_holder(conn: &zbus::Connection, why: &str) -> ExResult<()> {
    let manager = zbus_systemd::systemd1::ManagerProxy::new(conn).await?;
    let binary = util::prog_for_later();
    let exec = Value::new(vec![(
        binary.clone(),
        vec![binary, String::from("inhibit-hold"), why.to_string()],
        false,
    )]);
    let properties = vec![
        // `systemctl status` then says who is using the host, not just that someone is.
        (String::from("Description"), text(&format!("fpgahog: {}", why))?),
        (String::from("Type"), text("exec")?),
        (String::from("ExecStart"), OwnedValue::try_from(exec)?),
        // The holder is a long-lived root process whose only job is to keep a descriptor
        // open, and these hosts run builds that exhaust memory. Come straight back if the
        // OOM killer or anything else takes it, rather than waiting for the five-minute
        // re-check; systemd does not restart a unit we stopped on purpose, so teardown is
        // unaffected.
        (String::from("Restart"), text("always")?),
        (
            String::from("RestartUSec"),
            OwnedValue::try_from(Value::U64(1_000_000))?,
        ),
        (
            String::from("OOMScoreAdjust"),
            OwnedValue::try_from(Value::I32(-900))?,
        ),
        // Leave nothing behind to clean up by hand if the holder ever dies for good.
        (String::from("CollectMode"), text("inactive-or-failed")?),
    ];
    manager
        .start_transient_unit(UNIT.to_string(), String::from("replace"), properties, vec![])
        .await?;
    Ok(())
}

/// Stop the holder and wait until the unit name is free again. `StopUnit` only queues a job,
/// and `StartTransientUnit` refuses a name that is still loaded -- including one sitting in
/// `failed`, which only `ResetFailedUnit` clears. Starting without this wait fails with
/// `UnitExists`, which would leave the host with no inhibitor at all until the next re-check.
async fn clear_holder(conn: &zbus::Connection) -> ExResult<()> {
    let manager = zbus_systemd::systemd1::ManagerProxy::new(conn).await?;
    ignore_missing(manager.stop_unit(UNIT.to_string(), String::from("replace")).await)?;
    let deadline = std::time::Instant::now() + UNIT_WAIT;
    loop {
        ignore_missing(manager.reset_failed_unit(UNIT.to_string()).await)?;
        match manager.get_unit(UNIT.to_string()).await {
            // Gone from systemd's view: the name is free again.
            Err(zbus::Error::MethodError(name, _option, _message)) if is_missing(name.as_str()) => {
                return Ok(())
            }
            Err(err) => return Err(err.into()),
            Ok(_) if std::time::Instant::now() >= deadline => {
                return Err(
                    format!("{} was still loaded {:?} after stopping it", UNIT, UNIT_WAIT).into(),
                )
            }
            Ok(_) => tokio::time::sleep(POLL).await,
        }
    }
}

fn is_missing(error: &str) -> bool {
    error.ends_with(".NoSuchUnit")
}

/// Nothing to act on is the state we were asking for anyway.
fn ignore_missing<T>(result: zbus::Result<T>) -> zbus::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(zbus::Error::MethodError(name, _option, _message)) if is_missing(name.as_str()) => {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn text(value: &str) -> ExResult<OwnedValue> {
    Ok(OwnedValue::try_from(Value::new(value.to_string()))?)
}

/// Take the lock and park. Runs inside the transient unit; systemd stops it when the claims
/// are gone, and the lock goes with the process.
pub fn hold(why: &str) -> Result<(), String> {
    // One thread: this process sits on a descriptor for hours, so a worker pool per core is
    // just resident memory.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start an async runtime: {}", e))?;
    runtime.block_on(hold_async(why)).map_err(|e| e.to_string())
}

async fn hold_async(why: &str) -> ExResult<()> {
    let conn = zbus::Connection::system().await?;
    let manager = zbus_systemd::login1::ManagerProxy::new(&conn).await?;
    // The descriptor *is* the lock. Holding the binding is the whole job of this process.
    let _lock = manager
        .inhibit(
            WHAT.to_string(),
            WHO.to_string(),
            why.to_string(),
            MODE.to_string(),
        )
        .await?;
    println!("holding {} ({}) while: {}", WHAT, MODE, why);
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diskstate::{Claim, ResourceId};
    use chrono::Duration;

    fn claim(user: &str, resource: &str, left: Option<Duration>) -> Claim {
        Claim {
            id: 1,
            timeout: left.map(|left| Local::now() + left),
            soft_timeout: None,
            exclusive: true,
            user: user.into(),
            comment: String::new(),
            resources: vec![ResourceId::parse(resource)],
            inhibit: true,
        }
    }

    fn state_with(claims: Vec<Claim>) -> DiskState {
        let mut state = crate::diskstate::load_default();
        state.claims = claims;
        state
    }

    #[test]
    fn an_idle_host_holds_nothing() {
        assert_eq!(reason(&state_with(vec![]), Local::now()), None);
    }

    #[test]
    fn a_live_claim_names_its_board_owner_and_expiry() {
        let state = state_with(vec![claim("anna", "v80", Some(Duration::hours(2)))]);
        let why = reason(&state, Local::now()).expect("a live claim holds the lock");
        assert!(why.contains("v80"), "{}", why);
        assert!(why.contains("anna"), "{}", why);
        assert!(why.contains("systemctl reboot -i"), "{}", why);
    }

    #[test]
    fn an_expired_claim_lets_go() {
        let state = state_with(vec![claim("anna", "v80", Some(-Duration::minutes(1)))]);
        assert_eq!(reason(&state, Local::now()), None);
    }

    #[test]
    fn a_claim_that_never_expires_says_so() {
        let state = state_with(vec![claim("anna", "v80", None)]);
        let why = reason(&state, Local::now()).unwrap();
        assert!(why.contains("until it is released"), "{}", why);
    }

    #[test]
    fn opting_out_holds_nothing() {
        let mut state = state_with(vec![claim("anna", "v80", Some(Duration::hours(2)))]);
        state.claims[0].inhibit = false;
        assert_eq!(reason(&state, Local::now()), None);
    }

    /// The text is compared against what logind reports, so two reads a minute apart have to
    /// produce the same string. A countdown here would restart the holder every re-check.
    #[test]
    fn the_same_claims_always_produce_the_same_text() {
        let state = state_with(vec![claim("anna", "v80", Some(Duration::hours(2)))]);
        let now = Local::now();
        assert_eq!(reason(&state, now), reason(&state, now + Duration::minutes(7)));
    }

    #[test]
    fn every_live_claim_is_named() {
        let mut second = claim("bob", "host", Some(Duration::hours(1)));
        second.id = 2;
        let state = state_with(vec![claim("anna", "v80", Some(Duration::hours(2))), second]);
        let why = reason(&state, Local::now()).unwrap();
        assert!(why.contains("anna") && why.contains("bob"), "{}", why);
    }
}
