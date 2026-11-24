//! Admission reads the running nvfwd device, not its now-stale startup cap.
//! Callers must establish owned-slot QEMU identity and bracket this read with Seen checks.

use crate::context::Context;
use crate::monitor::Command;
use guesthouse_daemon::slot::Slot;
use serde_json::json;

const RESERVATION: &str = "admission-reservation-bytes";
const ROOTS: [&str; 2] = ["/machine/peripheral", "/machine/peripheral-anon"];

enum Reservation {
    WholeCard,
    GiB(u32),
}

/// Some(0) reserves the whole GPU. None is only a verified new device's zero reservation:
/// its live cap is zero and rejects positive allocations. It is not an absent GPU consumer.
pub(crate) fn running_cap_gib(context: &Context, slot: Slot, argument: &[u8]) -> Option<u32> {
    match read_cap(context, slot, argument) {
        Some(Reservation::GiB(0)) => None,
        Some(Reservation::GiB(cap)) => Some(cap),
        Some(Reservation::WholeCard) | None => Some(0),
    }
}

fn list(context: &Context, slot: Slot, path: &str) -> Option<Vec<(String, String)>> {
    let reply = context.ask_monitor(Some(slot), &Command::new("qom-list", json!({"path": path})))?;
    if reply.get("error").is_some() {
        return None;
    }
    reply.get("return")?.as_array()?.iter().map(|item| {
        Some((item.get("name")?.as_str()?.to_owned(), item.get("type")?.as_str()?.to_owned()))
    }).collect()
}

fn component(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !matches!(name, "." | "..")
        && !name.contains('/') && !name.contains('\0')
}

fn read_cap(context: &Context, slot: Slot, argument: &[u8]) -> Option<Reservation> {
    let argument = std::str::from_utf8(argument).ok()?;
    let mut ids = argument.split(',').filter_map(|part| part.strip_prefix("id="));
    let expected = ids.next();
    if ids.next().is_some() || expected.is_some_and(|name| !component(name)) {
        return None;
    }
    let mut devices = Vec::new();
    for root in ROOTS {
        for (name, kind) in list(context, slot, root)? {
            if kind == "child<nvfwd>" {
                if !component(&name) {
                    return None;
                }
                devices.push((root, name));
            }
        }
    }
    // The current launch plan has exactly one nvfwd device. Ambiguity reserves the full card.
    let [(root, name)] = devices.as_slice() else { return None };
    if let Some(expected) = expected {
        if *root != ROOTS[0] || name != expected {
            return None;
        }
    }
    let path = format!("{root}/{name}");
    let properties = list(context, slot, &path)?;
    let matching: Vec<_> = properties.iter().filter(|(name, _)| name == RESERVATION).collect();
    if matching.is_empty() {
        // The complete property list positively proves this binary lacks online accounting.
        let mut caps = argument.split(',').filter_map(|part| part.strip_prefix("max-vram="));
        let cap = caps.next()?.strip_suffix('G')?.parse::<u32>().ok()?;
        return caps.next().is_none().then_some(
            if cap == 0 { Reservation::WholeCard } else { Reservation::GiB(cap) },
        );
    }
    if matching.len() != 1 || matching[0].1 != "uint64" {
        return None;
    }
    let reply = context.ask_monitor(Some(slot), &Command::new(
        "qom-get", json!({"path": path, "property": RESERVATION}),
    ))?;
    if reply.get("error").is_some() {
        return None;
    }
    let bytes = reply.get("return")?.as_u64()?;
    // Reservation is max(cap, used including pending) captured under one object lock by nvfwd.
    // In this new device, zero cap rejects positive allocation. A trusted zero reservation
    // therefore charges zero; unknown/legacy zero still conservatively reserves the whole card.
    u32::try_from(bytes.div_ceil(1 << 30)).ok().map(Reservation::GiB)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::monitor::Monitor;
    use serde_json::Value;
    use std::os::fd::BorrowedFd;
    use std::path::Path;

    /// Explicitly confirms one old anonymous nvfwd and its complete legacy property list.
    pub struct LegacyMonitor;
    impl Monitor for LegacyMonitor {
        fn ask(&self, _socket: &Path, command: &Command) -> Option<Value> {
            match (command.name.as_str(), command.arguments["path"].as_str()?) {
                ("qom-list", "/machine/peripheral") => Some(json!({"return": []})),
                ("qom-list", "/machine/peripheral-anon") => Some(json!({"return": [
                    {"name":"device[17]", "type":"child<nvfwd>"}]})),
                ("qom-list", "/machine/peripheral-anon/device[17]") => Some(json!({"return": [
                    {"name":"max-vram","type":"uint64"}]})),
                _ => None,
            }
        }
        fn ask_handing(&self, _socket: &Path, _file: BorrowedFd<'_>, _commands: &[Command]) -> Vec<Value> {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Monitor;
    use crate::testing::{Scratch, scratch_context};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::os::fd::BorrowedFd;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Answers {
        responses: Mutex<HashMap<(String, String), Option<Value>>>,
        calls: Mutex<Vec<(PathBuf, Command)>>,
    }
    impl Answers {
        fn set(&self, name: &str, path: &str, response: Option<Value>) {
            self.responses.lock().unwrap().insert((name.into(), path.into()), response);
        }
        fn device(named: bool) -> Self {
            let answer = Self::default();
            let child = if named { "nvfwd" } else { "device[23]" };
            let root = if named { ROOTS[0] } else { ROOTS[1] };
            for container in ROOTS {
                answer.set("qom-list", container, Some(json!({"return": []})));
            }
            answer.set("qom-list", root, Some(json!({"return": [
                {"name": "device[0]", "type": "child<virtio-blk-pci>"},
                {"name": child, "type": "child<nvfwd>"}]})));
            let path = format!("{root}/{child}");
            answer.set("qom-list", &path, Some(json!({"return": [
                {"name": RESERVATION, "type": "uint64"}]})));
            answer.set("qom-get", &path, Some(json!({"return": 3_u64 << 30})));
            answer
        }
    }
    impl Monitor for Answers {
        fn ask(&self, socket: &Path, command: &Command) -> Option<Value> {
            self.calls.lock().unwrap().push((socket.to_owned(), command.clone()));
            self.responses.lock().unwrap().get(&(
                command.name.clone(), command.arguments["path"].as_str()?.to_owned(),
            )).cloned().flatten()
        }
        fn ask_handing(&self, _socket: &Path, _file: BorrowedFd<'_>, _commands: &[Command]) -> Vec<Value> {
            Vec::new()
        }
    }

    #[test]
    fn live_reservation_replaces_argv_and_rounds_used_or_pending_bytes_up() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let monitor = Arc::new(Answers::device(true));
        context.monitor = monitor.clone();
        let slot = crate::slots::number(2);
        let argument = b"nvfwd,id=nvfwd,max-vram=1G";
        assert_eq!(running_cap_gib(&context, slot, argument), Some(3));
        // The atomic property already includes max(live cap, used+pending), even after cap shrinks.
        monitor.set("qom-get", "/machine/peripheral/nvfwd", Some(json!({"return": (4_u64 << 30) + 1})));
        assert_eq!(running_cap_gib(&context, slot, argument), Some(5));
        let calls = monitor.calls.lock().unwrap();
        assert!(calls.iter().all(|(path, _)| *path == context.run_folder(Some(slot)).join("qmp.sock")));
        assert_eq!(calls.iter().filter(|(_, call)| call.name == "qom-get").count(), 2);
        assert!(calls.iter().filter(|(_, call)| call.name == "qom-get")
            .all(|(_, call)| call.arguments["property"] == RESERVATION));
    }

    #[test]
    fn anonymous_device_is_discovered_by_type_not_device_zero() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let monitor = Arc::new(Answers::device(false));
        context.monitor = monitor.clone();
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,max-vram=1G"), Some(3));
        let calls = monitor.calls.lock().unwrap();
        assert!(calls.iter().any(|(_, call)| call.name == "qom-get"
            && call.arguments["path"] == "/machine/peripheral-anon/device[23]"));
        assert!(!calls.iter().any(|(_, call)| call.arguments["path"] == "/machine/peripheral-anon/device[0]"));
    }

    #[test]
    fn only_confirmed_legacy_device_uses_argv_and_unknown_is_full_card() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.monitor = Arc::new(fixtures::LegacyMonitor);
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,max-vram=2G"), Some(2));
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,max-vram=0G"), Some(0));
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,max-vram=1536M"), Some(0));
        // None models a timeout/disconnect, not evidence of a legacy binary.
        context.monitor = Arc::new(Answers::default());
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,max-vram=2G"), Some(0));
    }

    #[test]
    fn new_property_failures_never_fall_back_to_stale_argv() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let monitor = Arc::new(Answers::device(true));
        context.monitor = monitor.clone();
        for response in [
            None, Some(json!({"error": {"class":"GenericError"}})),
            Some(json!({"return":"2G"})), Some(json!({"return":-1})),
            Some(json!({"return":1.5})), Some(json!({"return":u64::MAX})),
        ] {
            monitor.set("qom-get", "/machine/peripheral/nvfwd", response);
            assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,id=nvfwd,max-vram=1G"), Some(0));
        }
        monitor.set("qom-list", "/machine/peripheral/nvfwd", Some(json!({"return": [
            {"name":RESERVATION,"type":"str"}]})));
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,id=nvfwd,max-vram=1G"), Some(0));
        monitor.set("qom-list", "/machine/peripheral/nvfwd", None);
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,id=nvfwd,max-vram=1G"), Some(0));
    }

    #[test]
    fn trusted_new_zero_means_no_positive_allocation_allowed_not_unknown() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let monitor = Arc::new(Answers::device(true));
        context.monitor = monitor.clone();
        monitor.set("qom-get", "/machine/peripheral/nvfwd", Some(json!({"return":0})));
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,id=nvfwd,max-vram=3G"), None);
        monitor.set("qom-get", "/machine/peripheral/nvfwd", None);
        assert_eq!(running_cap_gib(&context, crate::slots::first(), b"nvfwd,id=nvfwd,max-vram=3G"), Some(0));
    }

    #[test]
    fn ambiguous_wrong_id_or_invalid_qom_children_fail_closed() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        let monitor = Arc::new(Answers::device(true));
        context.monitor = monitor.clone();
        let slot = crate::slots::first();
        assert_eq!(running_cap_gib(&context, slot, b"nvfwd,id=other,max-vram=1G"), Some(0));
        for children in [
            json!([{"name":"nvfwd","type":"child<nvfwd>"},{"name":"extra","type":"child<nvfwd>"}]),
            json!([{"name":"../other","type":"child<nvfwd>"}]),
            json!([{"name":"nvfwd","type":"child<not-nvfwd>"}]),
            json!([{"name":"nvfwd"}]),
        ] {
            monitor.set("qom-list", ROOTS[0], Some(json!({"return":children})));
            assert_eq!(running_cap_gib(&context, slot, b"nvfwd,max-vram=1G"), Some(0));
        }
    }
}
