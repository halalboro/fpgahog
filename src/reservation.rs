//! Atomic, exact-board deployment reservations. main holds the state lock throughout.
use crate::{
    cable, claims,
    diskstate::{self, Claim, DiskState, FpgaSpec, ResourceId},
    fpga, users,
};
use chrono::{DateTime, Local};
use clap::Subcommand;

#[derive(Subcommand, Clone)]
pub enum Action {
    /// Acquire or reuse your exclusive claim on exactly one configured PCI BDF.
    Acquire {
        #[arg(long)]
        bdf: String,
        /// Caller-generated unique token, also used for failure rollback.
        #[arg(long)]
        token: String,
        #[arg(long, default_value = "never")]
        expiry: String,
    },
    /// Release only the claim created with this token, never a reused claim.
    Release {
        #[arg(long)]
        token: String,
    },
    /// Reassert locks and verify an existing exclusive claim after driver reload.
    Verify {
        #[arg(long)]
        bdf: String,
    },
}

fn marker(token: &str) -> Result<String, String> {
    if token.len() != 36 || !token.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err("reservation token must be a UUID (36 hex/hyphen characters)".into());
    }
    Ok(format!("deploy-hw reservation {}", token))
}

fn normalize_bdf(text: &str) -> Result<String, String> {
    let text = text.to_ascii_lowercase();
    let full = if text.len() == 7 {
        format!("0000:{}", text)
    } else {
        text
    };
    let b = full.as_bytes();
    if b.len() != 12
        || b[4] != b':'
        || b[7] != b':'
        || b[10] != b'.'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| [4, 7, 10].contains(&i) || c.is_ascii_hexdigit())
        || b[11] > b'7'
    {
        return Err("invalid PCI BDF".into());
    }
    Ok(full)
}

fn board(state: &DiskState, bdf: &str) -> Result<FpgaSpec, String> {
    let normalized = normalize_bdf(bdf)?;
    let matches: Vec<_> = state
        .settings
        .fpgas
        .iter()
        .filter(|s| normalize_bdf(&s.bdf).ok().as_ref() == Some(&normalized))
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "BDF {} must match exactly one configured FPGA (found {})",
            normalized,
            matches.len()
        ));
    }
    let spec = matches[0];
    if state
        .settings
        .fpgas
        .iter()
        .filter(|s| s.alias == spec.alias)
        .count()
        != 1
    {
        return Err("ambiguous FPGA alias in inventory".into());
    }
    Ok(spec.clone())
}

fn own_claim(state: &DiskState, me: &str, spec: &FpgaSpec) -> Option<u64> {
    state
        .claims
        .iter()
        .find(|c| {
            c.user == me && c.exclusive && c.active_at(Local::now()) && c.covers_alias(&spec.alias)
        })
        .map(|c| c.id)
}

fn conflict(state: &DiskState, me: &str, spec: &FpgaSpec) -> Result<(), String> {
    // Include active shared claims: an exclusive deployment must not interrupt them.
    let blockers: Vec<_> = state
        .claims
        .iter()
        .filter(|c| {
            c.user != me
                && c.active_at(Local::now())
                && (c.covers_alias(&spec.alias) || c.covers(&ResourceId::Host))
        })
        .map(|c| format!("{} ({})", c.user, c.remaining()))
        .collect();
    if blockers.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} is reserved by {}; refusing deployment",
            spec.alias,
            blockers.join(", ")
        ))
    }
}

fn expiry(text: &str) -> Result<Option<DateTime<Local>>, String> {
    if text == "never" {
        return Ok(None);
    }
    let until = crate::try_parse_timeout(text)?;
    if until <= Local::now() {
        return Err("expiry must be in the future".into());
    }
    Ok(Some(until))
}

fn verify_enforcement(state: &DiskState, spec: &FpgaSpec) -> Result<(), String> {
    let locks = fpga::locks_for(state, &spec.alias);
    let devices_ok = spec.devices.iter().all(|path| {
        locks
            .iter()
            .any(|l| l.path == *path && fpga::lock_is_intact(l) == Some(true))
    });
    let cables_ok = spec.cables.iter().all(|serial| {
        matches!(
            cable::view(
                state,
                serial,
                std::path::Path::new(cable::SYSFS),
                std::path::Path::new(cable::DEV)
            ),
            cable::CableView::Present { locked: true }
        )
    });
    if (spec.devices.is_empty() && spec.cables.is_empty()) || !devices_ok || !cables_ok {
        return Err(format!("reservation for {} is not fully enforced; check device/cable inventory and permissions", spec.alias));
    }
    Ok(())
}

fn remove_created(state: &mut DiskState, me: &str, mark: &str) {
    state
        .claims
        .retain(|c| !(c.user == me && c.comment == mark));
}

pub fn run(action: Action, state: &mut DiskState) -> Result<String, String> {
    let me = users::my_username().ok_or("Cannot identify reservation owner")?;
    match action {
        Action::Release { token } => {
            let mark = marker(&token)?;
            let aliases: Vec<String> = state
                .claims
                .iter()
                .filter(|c| c.user == me && c.comment == mark)
                .flat_map(|c| {
                    c.resources
                        .iter()
                        .filter_map(|r| r.alias().map(str::to_string))
                })
                .collect();
            remove_created(state, &me, &mark);
            // Record intent before touching devices: a later maintenance pass can retry.
            diskstate::store(state);
            fpga::sync_locks(state);
            claims::keep_locks_asserted(state);
            if state.device_locks.iter().any(|l| {
                aliases.contains(&l.alias)
                    && !state
                        .claims
                        .iter()
                        .any(|c| c.exclusive && c.covers_alias(&l.alias))
            }) || state.cable_locks.iter().any(|l| {
                aliases.contains(&l.alias)
                    && !state
                        .claims
                        .iter()
                        .any(|c| c.exclusive && c.covers_alias(&l.alias))
            }) {
                return Err("claim removed but some device locks could not be restored; run sudo fpgahog maintenance".into());
            }
            Ok("reservation rollback complete (pre-existing claims preserved)".into())
        }
        Action::Verify { bdf } => {
            let spec = board(state, &bdf)?;
            conflict(state, &me, &spec)?;
            if own_claim(state, &me, &spec).is_none() {
                return Err("no active exclusive reservation owned by caller".into());
            }
            fpga::sync_locks(state);
            verify_enforcement(state, &spec)?;
            claims::keep_locks_asserted(state);
            Ok(format!("reservation enforced for {}", spec.alias))
        }
        Action::Acquire {
            bdf,
            token,
            expiry: text,
        } => {
            let mark = marker(&token)?;
            let timeout = expiry(&text)?;
            let spec = board(state, &bdf)?;
            conflict(state, &me, &spec)?;
            if let Some(id) = own_claim(state, &me, &spec) {
                fpga::sync_locks(state);
                verify_enforcement(state, &spec)?;
                return Ok(format!("Reusing reservation {} for {} (expiry unchanged). Release: sudo fpgahog release '{}'", id, spec.alias, spec.alias.replace('\'', "'\\''")));
            }
            if state.claims.iter().any(|c| c.comment == mark) {
                return Err("reservation token already used".into());
            }
            if !claims::check_enforceable(std::slice::from_ref(&spec), false)
                || !claims::check_free(std::slice::from_ref(&spec), &me, false)
            {
                return Err("FPGA cannot be exclusively reserved".into());
            }
            let id = state.next_claim_id();
            state.claims.push(Claim {
                id,
                timeout,
                soft_timeout: None,
                exclusive: true,
                user: me.clone(),
                comment: mark.clone(),
                resources: vec![ResourceId::Fpga(spec.alias.clone())],
                inhibit: true,
            });
            diskstate::store(state);
            fpga::sync_locks(state);
            if let Err(err) = verify_enforcement(state, &spec) {
                remove_created(state, &me, &mark);
                diskstate::store(state);
                fpga::sync_locks(state);
                return Err(err);
            }
            claims::keep_locks_asserted(state);
            if let Some(until) = timeout {
                claims::schedule_maintenance(until);
            }
            Ok(format!(
                "Reserved {} (claim {}). Release: sudo fpgahog reservation release --token {}",
                spec.alias, id, token
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> DiskState {
        let mut s = diskstate::load_default();
        s.settings.fpgas = vec![FpgaSpec {
            alias: "v80".into(),
            bdf: "0000:81:00.0".into(),
            pci_id: String::new(),
            devices: vec![],
            cables: vec![],
        }];
        s
    }
    fn claim(user: &str, comment: &str) -> Claim {
        Claim {
            id: 1,
            timeout: None,
            soft_timeout: None,
            exclusive: true,
            user: user.into(),
            comment: comment.into(),
            resources: vec![ResourceId::Fpga("v80".into())],
            inhibit: true,
        }
    }
    #[test]
    fn indefinite_and_timed_expiry() {
        assert_eq!(expiry("never").unwrap(), None);
        assert!(expiry("1h").unwrap().unwrap() > Local::now());
        assert!(expiry("2000-01-01").is_err());
        assert!(expiry("garbage").is_err());
        let c = claim("me", "test");
        assert!(c.active_at(Local::now() + chrono::Duration::days(36500)));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"timeout\":null"));
        assert_eq!(serde_json::from_str::<Claim>(&json).unwrap(), c);
    }
    #[test]
    fn maintenance_keeps_indefinite_claim_and_rechecks_enforcement() {
        let mut s = fixture();
        s.settings.fpgas[0]
            .devices
            .push("/not-a-real-device".into());
        s.claims.push(claim("me", "permanent"));
        let mut expired = claim("me", "expired");
        expired.id = 2;
        expired.timeout = Some(Local::now() - chrono::Duration::hours(1));
        s.claims.push(expired);
        let mut released = false;
        diskstate::maintenance(&mut s, &mut released);
        assert_eq!(s.claims.len(), 1);
        assert_eq!(s.claims[0].timeout, None);
        assert!(claims::next_recheck(&s, Local::now()).is_some());
    }
    #[test]
    fn timed_state_still_deserializes() {
        let mut c = claim("me", "timed");
        c.timeout = Some(Local::now() + chrono::Duration::hours(1));
        assert_eq!(
            serde_json::from_str::<Claim>(&serde_json::to_string(&c).unwrap()).unwrap(),
            c
        );
    }
    #[test]
    fn exact_board_and_ambiguity() {
        let mut s = fixture();
        assert_eq!(board(&s, "81:00.0").unwrap().alias, "v80");
        assert!(board(&s, "82:00.0").is_err());
        assert!(board(&s, "not-a-bdf").is_err());
        s.settings.fpgas.push(s.settings.fpgas[0].clone());
        assert!(board(&s, "81:00.0").is_err());
    }
    #[test]
    fn conflict_identifies_owner_and_expired_claim_does_not_block() {
        let mut s = fixture();
        s.claims.push(claim("alice", "existing"));
        let spec = board(&s, "81:00.0").unwrap();
        assert!(conflict(&s, "bob", &spec).unwrap_err().contains("alice"));
        assert!(conflict(&s, "alice", &spec).is_ok());
        s.claims[0].timeout = Some(Local::now() - chrono::Duration::seconds(1));
        assert!(conflict(&s, "bob", &spec).is_ok());
    }
    #[test]
    fn shared_and_host_claims_block_deployment() {
        let mut s = fixture();
        let spec = board(&s, "81:00.0").unwrap();
        let mut other = claim("alice", "shared");
        other.exclusive = false;
        s.claims.push(other);
        assert!(conflict(&s, "bob", &spec).is_err());
        s.claims[0].resources = vec![ResourceId::Host];
        assert!(conflict(&s, "bob", &spec).is_err());
    }
    #[test]
    fn own_exclusive_is_reused_without_expiry_change() {
        let mut s = fixture();
        let mut old = claim("me", "existing");
        old.timeout = Some(Local::now() + chrono::Duration::hours(2));
        s.claims.push(old.clone());
        assert_eq!(own_claim(&s, "me", &s.settings.fpgas[0]), Some(old.id));
        assert_eq!(s.claims[0], old);
        s.claims[0].exclusive = false;
        assert_eq!(own_claim(&s, "me", &s.settings.fpgas[0]), None);
    }
    #[test]
    fn rollback_preserves_preexisting_and_foreign_claims() {
        let mut s = fixture();
        s.claims = vec![
            claim("me", "old"),
            claim("me", "new"),
            claim("other", "new"),
        ];
        remove_created(&mut s, "me", "new");
        assert_eq!(s.claims.len(), 2);
        assert!(s.claims.iter().any(|c| c.comment == "old"));
        assert!(s.claims.iter().any(|c| c.user == "other"));
        remove_created(&mut s, "me", "new");
        assert_eq!(s.claims.len(), 2);
    }
}
