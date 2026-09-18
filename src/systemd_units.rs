use crate::diskstate;
use zbus_systemd::{zbus, zvariant::OwnedObjectPath};

const DISABLE_TIMERS: bool = true;
const DISABLE_UNITS: &[&str] = &["xrdp.service"];

type ExResult<T> = Result<T, Box<dyn std::error::Error + 'static>>;

/// Layout defined by https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.systemd1.html
#[derive(Debug)]
#[allow(dead_code)]
struct Unit {
    name: String,
    description: String,
    loaded_state: String,
    active_state: String,
    sub_state: String,
    followup_unit: String,
    unit_path: OwnedObjectPath,
    job_id: u32,
    job_type: String,
    job_path: OwnedObjectPath,
}

async fn list_units<'a>(
    manager: &zbus_systemd::systemd1::ManagerProxy<'a>,
    states: Vec<String>,
    match_globs: Vec<String>,
) -> ExResult<Vec<Unit>> {
    let units = manager.list_units_by_patterns(states, match_globs).await?;
    // convert unit tuple to struct
    let units = units.into_iter().map(
        |(
            name,
            description,
            loaded_state,
            active_state,
            sub_state,
            followup_unit,
            unit_path,
            job_id,
            job_type,
            job_path,
        )| {
            Unit {
                name,
                description,
                loaded_state,
                active_state,
                sub_state,
                followup_unit,
                unit_path,
                job_id,
                job_type,
                job_path,
            }
        },
    );
    Ok(units.collect())
}

/// Stop the units a hog pauses. Failures are returned, never fatal: by the time this runs the
/// host is already hogged, and dying here would leave that half-recorded.
pub fn disable_resource(state: &mut diskstate::DiskState) -> Result<(), String> {
    println!("systemd_units: disable systemd services");
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("cannot start async runtime: {}", e))?;
    runtime.block_on(disable_units(state)).map_err(|e| e.to_string())
}

/// Start the units a hog paused. Units that fail stay recorded, so a later release retries them.
pub fn enable_resource(state: &mut diskstate::DiskState) -> Result<(), String> {
    println!("systemd_units: enable systemd services");
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("cannot start async runtime: {}", e))?;
    runtime.block_on(enable_units(state)).map_err(|e| e.to_string())
}

async fn disable_units(state: &mut diskstate::DiskState) -> ExResult<()> {
    let conn = zbus::Connection::system()
        .await
        .map_err(|e| format!("cannot reach systemd over the system bus: {}", e))?;
    let manager = zbus_systemd::systemd1::ManagerProxy::new(&conn).await?;

    let states = vec!["active".to_string()];
    let mut units = vec![];
    if DISABLE_TIMERS {
        units.append(&mut list_units(&manager, states.clone(), vec!["*.timer".to_string()]).await?);
    }
    for unit in DISABLE_UNITS {
        units.append(&mut list_units(&manager, states.clone(), vec![unit.to_string()]).await?);
    }

    let mut failed = vec![];
    for unit in units {
        if let Err(err) = disable_unit(state, &manager, &unit).await {
            println!("WARN: {}", err);
            failed.push(unit.name.clone());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("could not stop {}", failed.join(", ")).into())
    }
}

async fn disable_unit<'a>(
    state: &mut diskstate::DiskState,
    manager: &zbus_systemd::systemd1::ManagerProxy<'a>,
    unit: &Unit,
) -> Result<(), String> {
    println!("disabling {}", unit.name);
    match manager.stop_unit(unit.name.clone(), "fail".to_string()).await {
        Err(zbus::Error::MethodError(name, _option, _message))
            if name == "org.freedesktop.DBus.Error.AccessDenied" =>
        {
            Err(format!("insufficient permissions to stop {}; run as root", unit.name))
        }
        Err(e) => Err(format!("cannot stop {}: {}", unit.name, e)),
        Ok(_) => {
            if !state.disabled_systemd_units.contains(&unit.name) {
                state.disabled_systemd_units.push(unit.name.clone());
            }
            Ok(())
        }
    }
}

async fn enable_units(state: &mut diskstate::DiskState) -> ExResult<()> {
    let conn = zbus::Connection::system()
        .await
        .map_err(|e| format!("cannot reach systemd over the system bus: {}", e))?;
    let manager = zbus_systemd::systemd1::ManagerProxy::new(&conn).await?;

    let mut failed = vec![];
    for unit_name in state.disabled_systemd_units.clone() {
        println!("enabling {}", unit_name);
        match manager.start_unit(unit_name.clone(), "fail".to_string()).await {
            Err(zbus::Error::MethodError(name, _option, _message))
                if name == "org.freedesktop.DBus.Error.AccessDenied" =>
            {
                println!("WARN: insufficient permissions to start {}; run as root", unit_name);
                failed.push(unit_name);
            }
            Err(e) => {
                println!("WARN: cannot start {}: {}", unit_name, e);
                failed.push(unit_name);
            }
            Ok(_) => state.disabled_systemd_units.retain(|t| *t != unit_name),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("could not start {}", failed.join(", ")).into())
    }
}
