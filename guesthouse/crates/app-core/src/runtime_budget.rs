//! Runtime cap changes for this install's owned Windows/Linux slots only.
//! Lock order: run_task's target-slot lock, then the shared admission lock. Never another slot lock.
use crate::{context::Context, failure::{Failure, Result}, machine::Machine};
use crate::daemon_control::{self, Budget};
use crate::process_stat::Seen;
use crate::processes::{self, Qemu};
use crate::monitor::{Command, MAIN_SOCKET};
use guesthouse_daemon::slot::Slot;
use rustix::fs::{flock, FlockOperation};
use serde_json::{json, Value};
use std::{fs::{self, File}, num::NonZeroU64, path::PathBuf};

pub(crate) const LOCK_FILE: &str = "gpu-admission.lock";
const GIB: u64 = 1 << 30;
const CAP: &str = "vram-budget-bytes";
const USED: &str = "vram-used-bytes";
const RESERVED: &str = "admission-reservation-bytes";

enum Endpoint {
    Windows { daemon: Seen, socket: PathBuf, slot: Slot },
    Linux { qemu: Seen, socket: PathBuf, path: String },
}

fn qmp(context: &Context, socket: &std::path::Path, pid: u32, name: &str, arguments: Value) -> Option<Value> {
    let answer = context.monitor.ask_owned(socket, pid, &Command::new(name, arguments))?;
    if answer.get("error").is_some() { return None; }
    answer.get("return").cloned()
}

fn properties(context: &Context, socket: &std::path::Path, pid: u32, path: &str) -> Option<Vec<(String, String)>> {
    qmp(context, socket, pid, "qom-list", json!({"path":path}))?.as_array()?.iter()
        .map(|entry| Some((entry.get("name")?.as_str()?.into(), entry.get("type")?.as_str()?.into()))).collect()
}

fn component(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !matches!(name, "." | "..") && !name.contains(['/', '\0'])
}

fn endpoint(context: &Context, slot: Slot, qemu: &Qemu) -> Option<Endpoint> {
    let devices: Vec<_> = qemu.words.windows(2).filter(|p| p[0] == b"-device").collect();
    let linux: Vec<_> = devices.iter().filter(|p| p[1].starts_with(b"nvfwd,")).collect();
    let windows = devices.iter().filter(|p| p[1].starts_with(b"gpupv-vgpu,")).count();
    match (linux.as_slice(), windows) {
        ([], 1) => {
            let daemons: Vec<_> = crate::machine::sorted_names(&context.processes).into_iter().filter_map(|pid| {
                let seen = Seen::now(context, pid.parse().ok()?)?;
                (crate::daemon::is_own_daemon(context, &pid, slot) && seen.is_alive(context)).then_some(seen)
            }).collect();
            let [daemon] = daemons.as_slice() else { return None; };
            Some(Endpoint::Windows { daemon: *daemon, slot,
                socket: context.run_folder(Some(slot)).join(guesthouse_daemon::control::SOCKET_NAME) })
        }
        ([argument], 0) => {
            let argument = std::str::from_utf8(&argument[1]).ok()?;
            let mut ids = argument.split(',').filter_map(|part| part.strip_prefix("id="));
            let expected = ids.next();
            if ids.next().is_some() || expected.is_some_and(|name| !component(name)) { return None; }
            let socket = context.run_folder(Some(slot)).join(MAIN_SOCKET);
            let mut matches = Vec::new();
            for root in ["/machine/peripheral", "/machine/peripheral-anon"] {
                for (name, kind) in properties(context, &socket, qemu.seen.pid, root)? {
                    if kind == "child<nvfwd>" {
                        if !component(&name) { return None; }
                        matches.push((root, name));
                    }
                }
            }
            let [(root, name)] = matches.as_slice() else { return None; };
            if expected.is_some_and(|expected| *root != "/machine/peripheral" || name != expected) { return None; }
            let path = format!("{root}/{name}");
            let fields = properties(context, &socket, qemu.seen.pid, &path)?;
            for required in [CAP, USED, RESERVED] {
                let matching: Vec<_> = fields.iter().filter(|(name, _)| name == required).collect();
                if matching.len() != 1 || matching[0].1 != "uint64" { return None; }
            }
            Some(Endpoint::Linux { qemu: qemu.seen, socket, path })
        }
        _ => None,
    }
}

impl Endpoint {
    fn budget(&self, context: &Context) -> Option<Budget> {
        match self {
            Self::Windows { daemon, socket, slot } => {
                let owns = || daemon.is_alive(context) && crate::daemon::is_own_daemon(context, &daemon.pid.to_string(), *slot);
                if !owns() { return None; }
                let state = daemon_control::budget(socket, daemon.pid)?;
                owns().then_some(state)
            }
            Self::Linux { qemu, socket, path } => {
                if !qemu.is_alive(context) { return None; }
                let get = |property: &str| qmp(context, socket, qemu.pid, "qom-get",
                    json!({"path":path,"property":property}))?.as_u64();
                let cap = get(CAP)?;
                let charged = get(USED)?;
                let reservation = get(RESERVED)?;
                // A concurrent direct cap writer is outside the manager lock. Do not accept an
                // inconsistent snapshot; the atomic reservation must cover the sampled charge.
                if cap != get(CAP)? || reservation < cap.max(charged) { return None; }
                qemu.is_alive(context).then_some(Budget { cap: Some(cap), charged, reservation: Some(reservation) })
            }
        }
    }

    /// Exactly one write attempt; a lost reply does not imply no mutation.
    fn set(&self, context: &Context, bytes: NonZeroU64) -> bool {
        match self {
            Self::Windows { daemon, socket, slot } =>
                daemon.is_alive(context) && crate::daemon::is_own_daemon(context, &daemon.pid.to_string(), *slot)
                    && daemon_control::set_budget(socket, daemon.pid, bytes).is_some(),
            Self::Linux { qemu, socket, path } => qemu.is_alive(context) &&
                qmp(context, socket, qemu.pid, "qom-set",
                    json!({"path":path,"property":CAP,"value":bytes.get()})).is_some(),
        }
    }
}

fn consumer(qemu: &Qemu) -> bool {
    qemu.words.windows(2).any(|p| p[0] == b"-device" &&
        (p[1].starts_with(b"nvfwd,") || p[1].starts_with(b"gpupv-vgpu,")))
}

fn same_target(context: &Context, slot: Slot, before: &Qemu, machine: &Machine) -> bool {
    before.seen.is_alive(context) && processes::budget_vms(context).get(&slot.number())
        .is_some_and(|now| now.seen == before.seen && now.runs(machine))
}

/// Existing explicit uncapped state can always be made finite. A lower/equal cap cannot authorize
/// additional allocation, even if retained objects or other guests already put the card over budget.
fn decreasing(current: Budget, requested: NonZeroU64) -> bool {
    current.cap.is_none_or(|cap| requested.get() <= cap)
}

fn admits(current: Budget, requested: NonZeroU64, others: &[Option<u64>], maximum_gib: u32) -> bool {
    if decreasing(current, requested) { return true; }
    let Some(reservation) = current.reservation else { return false; };
    let mut used = u128::from(requested.get().max(current.charged).max(reservation).div_ceil(GIB));
    for other in others {
        let Some(bytes) = other else { return false; };
        used += u128::from(bytes.div_ceil(GIB));
    }
    maximum_gib != 0 && used <= u128::from(maximum_gib)
}

pub(crate) fn apply(context: &Context, machine: &Machine, gib: u32) -> Result<String> {
    let bytes = NonZeroU64::new(u64::from(gib) * GIB)
        .ok_or_else(|| Failure::new("The running video budget must be a positive number of GiB."))?;
    fs::create_dir_all(&context.paths.runtime)?;
    let lock = File::create(context.paths.runtime.join(LOCK_FILE))?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| Failure::new("GPU admission is busy; try the budget change after the current start or change finishes."))?;
    let slot = context.slot;
    let vms = processes::budget_vms(context);
    let target = vms.get(&slot.number()).filter(|qemu| qemu.runs(machine))
        .ok_or_else(|| Failure::new("This machine is not the owned running VM in the selected slot."))?;
    let control = endpoint(context, slot, target)
        .ok_or_else(|| Failure::new("This running GPU has no verified online budget control."))?;
    let current = control.budget(context)
        .ok_or_else(|| Failure::new("The running budget is unknown; no change was sent."))?;
    if !same_target(context, slot, target, machine) {
        return Err(Failure::new("The running VM changed before budget admission; no change was sent."));
    }
    let mut others = Vec::new();
    if !decreasing(current, bytes) {
        for (number, qemu) in &vms {
            if *number == slot.number() || !consumer(qemu) { continue; }
            let reservation = endpoint(context, crate::slots::number(*number), qemu)
                .and_then(|control| control.budget(context)).and_then(|state| state.reservation)
                .filter(|_| qemu.seen.is_alive(context));
            others.push(reservation);
        }
        if !admits(current, bytes, &others, crate::settings::max_vram_gib(context)) {
            return Err(Failure::new("The increased budget does not fit the verified reservations of the owned running GPUs."));
        }
    }
    if !same_target(context, slot, target, machine) {
        return Err(Failure::new("The running VM changed before the budget write; no change was sent."));
    }
    let acknowledged = control.set(context, bytes);
    let after = control.budget(context);
    if !same_target(context, slot, target, machine) {
        return Err(Failure::new("The VM changed after the budget write; it may have applied. No retry or rollback was sent."));
    }
    let Some(after) = after.filter(|state| state.cap == Some(bytes.get())) else {
        return Err(Failure::new("The budget write could not be confirmed; it may have applied. No retry or rollback was sent. Read the live state before increasing another budget."));
    };
    let reservation = after.reservation.ok_or_else(|| Failure::new("The updated reservation is unknown."))?;
    Ok(format!("Running budget verified: {} GiB; charged {} bytes; reservation {} bytes. {} Saved startup settings are unchanged.",
        gib, after.charged, reservation,
        if acknowledged { "Write acknowledged." } else { "Write reply was lost; independent readback confirms the effective cap." }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{monitor::Monitor, testing::{Scratch, scratch_context}};
    use std::{collections::HashMap, os::fd::BorrowedFd, path::Path, sync::{Arc, Mutex}};

    #[derive(Default)]
    struct State {
        budgets: HashMap<u32, (u64, u64)>,
        writes: Vec<(u32, u64)>,
        lost_reply: bool,
        unreadable_after_write: bool,
        unknown: Option<u32>,
    }
    #[derive(Default)]
    struct Mock(Mutex<State>);
    impl Monitor for Mock {
        fn ask(&self, _: &Path, _: &Command) -> Option<Value> { panic!("online changes need owned QMP"); }
        fn ask_handing(&self, _: &Path, _: BorrowedFd<'_>, _: &[Command]) -> Vec<Value> { Vec::new() }
        fn ask_owned(&self, _: &Path, pid: u32, command: &Command) -> Option<Value> {
            let mut state = self.0.lock().unwrap();
            if state.unknown == Some(pid) || (state.unreadable_after_write && !state.writes.is_empty()) { return None; }
            let (cap, used) = *state.budgets.get(&pid)?;
            match command.name.as_str() {
                "qom-list" => match command.arguments["path"].as_str()? {
                    "/machine/peripheral" => Some(json!({"return":[{"name":"nvfwd","type":"child<nvfwd>"}]})),
                    "/machine/peripheral-anon" => Some(json!({"return":[]})),
                    "/machine/peripheral/nvfwd" => Some(json!({"return":[
                        {"name":CAP,"type":"uint64"},{"name":USED,"type":"uint64"},{"name":RESERVED,"type":"uint64"}]})),
                    _ => None,
                },
                "qom-get" => Some(json!({"return":match command.arguments["property"].as_str()? {
                    CAP => cap, USED => used, RESERVED => cap.max(used), _ => return None,
                }})),
                "qom-set" => {
                    assert_eq!(command.arguments["property"], CAP);
                    let bytes = command.arguments["value"].as_u64()?;
                    state.budgets.get_mut(&pid)?.0 = bytes;
                    state.writes.push((pid, bytes));
                    (!state.lost_reply).then(|| json!({"return":{}}))
                }
                _ => None,
            }
        }
    }

    fn vm(context: &Context, slot: Slot, pid: u32, machine: &Machine) {
        let folder = context.run_folder(Some(slot));
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join(crate::vm_args::PID_FILE);
        fs::write(&file, pid.to_string()).unwrap();
        crate::processes::stand_in::fake_process(context, pid,
            &["/different/build/qemu-system-x86_64", "-pidfile", file.to_str().unwrap(),
              "-device", "nvfwd,id=nvfwd,max-vram=1G", "-drive", &format!("file={},format=qcow2", machine.disk)], &[], "");
    }

    fn setup(scratch: &Scratch) -> (Context, Machine, Arc<Mock>) {
        let mut context = scratch_context(scratch);
        crate::settings::write_json(&context.paths.setup_file(), &json!({"card_memory_mib":8192})).unwrap();
        let machine = Machine { disk: scratch.path("test.qcow2").display().to_string(),
            vram_gib: 4, ..Machine::new("budget-test", "budget test", "", "") };
        fs::create_dir_all(machine.folder(&context)).unwrap();
        machine.save(&context).unwrap();
        vm(&context, context.slot, 40, &machine);
        let monitor = Arc::new(Mock::default());
        monitor.0.lock().unwrap().budgets.insert(40, (GIB, GIB));
        context.monitor = monitor.clone();
        (context, machine, monitor)
    }

    #[test]
    fn budget_admission_rounds_up_and_retains_charge_after_reduction() {
        let request = |gib| NonZeroU64::new(gib * GIB).unwrap();
        let old = Budget { cap: Some(2*GIB), charged: 5*GIB, reservation: Some(5*GIB) };
        assert!(admits(old, request(1), &[None], 0), "safe reduction despite unknown or overbudget neighbors");
        assert!(!admits(old, request(3), &[Some(2*GIB)], 6));
        assert!(admits(old, request(3), &[Some(GIB)], 6));
        assert!(!admits(old, request(3), &[Some(GIB+1)], 6), "ceil each reservation");
        assert!(!admits(old, request(3), &[None], 8));
        assert!(admits(Budget { cap: None, charged: GIB, reservation: None }, request(1), &[None], 0));
    }

    #[test]
    fn budget_action_uses_owned_slot_and_preserves_saved_settings() {
        let scratch = Scratch::new();
        let (context, machine, monitor) = setup(&scratch);
        let host = crate::vm_host::stand_in::RecordingHost::default();
        crate::actions::run_task(&context, &host, "apply_video_budget", &machine.id, &["2".into()]).unwrap();
        assert_eq!(monitor.0.lock().unwrap().writes, [(40, 2*GIB)]);
        assert_eq!(crate::machine::load(&context, &machine.id).unwrap().vram_gib, 4);
        assert!(host.said().is_empty());
    }

    #[test]
    fn budget_increase_checks_other_live_slots_and_unknown_refuses_before_write() {
        for unknown in [false, true] {
            let scratch = Scratch::new();
            let (context, machine, monitor) = setup(&scratch);
            let other = Machine { disk: scratch.path("other.qcow2").display().to_string(), ..machine.clone() };
            vm(&context, crate::slots::number(1), 41, &other);
            let mut state = monitor.0.lock().unwrap();
            state.budgets.insert(41, (4*GIB, 2*GIB));
            state.unknown = unknown.then_some(41);
            drop(state);
            assert!(apply(&context, &machine, 3).is_err());
            assert!(monitor.0.lock().unwrap().writes.is_empty());
            // A verified non-increasing request never lends out the other VM's unknown usage.
            assert!(apply(&context, &machine, 1).is_ok());
        }
    }

    #[test]
    fn budget_reduction_below_usage_keeps_reservation_and_lost_reply_is_read_back() {
        let scratch = Scratch::new();
        let (context, machine, monitor) = setup(&scratch);
        monitor.0.lock().unwrap().budgets.insert(40, (4*GIB, 5*GIB));
        monitor.0.lock().unwrap().lost_reply = true;
        let report = apply(&context, &machine, 2).unwrap();
        assert!(report.contains("independent readback confirms"));
        assert!(report.contains(&format!("reservation {} bytes", 5*GIB)));
        assert_eq!(monitor.0.lock().unwrap().writes, [(40, 2*GIB)]);
    }

    #[test]
    fn budget_unconfirmed_write_is_not_retried_or_rolled_back() {
        let scratch = Scratch::new();
        let (context, machine, monitor) = setup(&scratch);
        monitor.0.lock().unwrap().lost_reply = true;
        monitor.0.lock().unwrap().unreadable_after_write = true;
        let error = apply(&context, &machine, 2).unwrap_err();
        assert!(error.0.contains("may have applied"));
        let state = monitor.0.lock().unwrap();
        assert_eq!(state.writes, [(40, 2*GIB)]);
        assert_eq!(state.budgets[&40].0, 2*GIB);
    }

    #[test]
    fn budget_wrong_machine_or_shared_start_lock_never_sends_a_write() {
        let scratch = Scratch::new();
        let (context, machine, monitor) = setup(&scratch);
        let wrong = Machine { disk: "/another/disk".into(), ..machine.clone() };
        assert!(apply(&context, &wrong, 2).is_err());
        fs::create_dir_all(&context.paths.runtime).unwrap();
        let start_lock = File::create(context.paths.runtime.join(LOCK_FILE)).unwrap();
        flock(&start_lock, FlockOperation::LockExclusive).unwrap();
        assert!(apply(&context, &machine, 2).unwrap_err().0.contains("admission is busy"));
        assert!(monitor.0.lock().unwrap().writes.is_empty());
    }
    #[test]
    fn budget_windows_command_lost_reply_is_verified_with_same_owned_daemon() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicU64, Ordering};
        let scratch = Scratch::new();
        let (context, machine, _) = setup(&scratch);
        let folder = context.run_folder(None);
        let pidfile = folder.join(crate::vm_args::PID_FILE);
        crate::processes::stand_in::fake_process(&context, 40,
            &["/different/qemu-system-x86_64", "-pidfile", pidfile.to_str().unwrap(),
              "-device", "gpupv-vgpu,id=gpu", "-drive", &format!("file={},format=qcow2", machine.disk)], &[], "");
        let daemon_pid = std::process::id();
        crate::processes::stand_in::fake_process(&context, daemon_pid,
            &["/bin/guesthouse-daemon", context.paths.runtime.to_str().unwrap()],
            &[&format!("GUESTHOUSE_SLOT={}", context.slot.number())], "");
        let listener = UnixListener::bind(folder.join(guesthouse_daemon::control::SOCKET_NAME)).unwrap();
        let cap = Arc::new(AtomicU64::new(GIB));
        let observed = cap.clone();
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
                if index == 1 {
                    assert_eq!(line, format!("budget {}\n", 2*GIB));
                    observed.store(2*GIB, Ordering::SeqCst);
                    continue; // Applied, but its reply is lost.
                }
                assert_eq!(line, "status\n");
                let n = observed.load(Ordering::SeqCst);
                write!(stream, "OK version=1 budget_scope=rm-explicit-not-nvidia-smi budget_limit_bytes={n} charged_bytes={GIB} admission_reservation_bytes={n}\n").unwrap();
            }
        });
        let report = apply(&context, &machine, 2).unwrap();
        server.join().unwrap();
        assert_eq!(cap.load(Ordering::SeqCst), 2*GIB);
        assert!(report.contains("independent readback confirms"));
    }

}
