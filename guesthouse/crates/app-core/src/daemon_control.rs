//! Bounded host controls and accounting from the already-identified daemon's local control socket.

use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_REPLY: usize = 4096;
const READ_DEADLINE: Duration = Duration::from_millis(500);

pub(crate) fn reservation(socket: &Path, daemon_pid: u32) -> Option<u64> {
    reservation_with_timeout(socket, daemon_pid, READ_DEADLINE)
}

fn remaining(end: Instant) -> Option<Duration> {
    let left = end.checked_duration_since(Instant::now())?;
    (!left.is_zero()).then_some(left)
}

fn reservation_with_timeout(socket: &Path, daemon_pid: u32, timeout: Duration) -> Option<u64> {
    parse(&exchange(socket, daemon_pid, b"status\n", timeout)?)
}

fn exchange(socket: &Path, daemon_pid: u32, mut request: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let end = Instant::now() + timeout;
    // As with the app's input socket, nonblocking connect cannot wait for a backlog slot.
    // Busy/in-progress connection is conservatively unknown rather than retried indefinitely.
    let fd = net::socket_with(
        AddressFamily::UNIX, SocketType::STREAM, SocketFlags::NONBLOCK | SocketFlags::CLOEXEC, None,
    ).ok()?;
    net::connect(&fd, &SocketAddrUnix::new(socket).ok()?).ok()?;
    if net::sockopt::socket_peercred(&fd).ok()?.pid.as_raw_pid() as u32 != daemon_pid {
        return None;
    }
    let mut stream = UnixStream::from(fd);
    stream.set_nonblocking(false).ok()?;
    while !request.is_empty() {
        stream.set_write_timeout(Some(remaining(end)?)).ok()?;
        let sent = stream.write(request).ok()?;
        if sent == 0 {
            return None;
        }
        request = &request[sent..];
    }
    let mut response = Vec::new();
    loop {
        // Do not read a fifth KiB merely to detect excess. A full-size response is rejected.
        let room = MAX_REPLY.checked_sub(response.len())?;
        if room == 0 {
            return None;
        }
        let mut bytes = [0; 512];
        let length = room.min(bytes.len());
        stream.set_read_timeout(Some(remaining(end)?)).ok()?;
        let count = stream.read(&mut bytes[..length]).ok()?;
        if count == 0 {
            break;
        }
        response.extend_from_slice(&bytes[..count]);
    }
    Some(response)
}

/// Current verified object cap/charge; None cap means explicitly uncapped, not missing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Budget {
    pub cap: Option<u64>,
    pub charged: u64,
    pub reservation: Option<u64>,
}

pub(crate) fn budget(socket: &Path, daemon_pid: u32) -> Option<Budget> {
    parse_budget(&exchange(socket, daemon_pid, b"status\n", READ_DEADLINE)?)
}

/// One attempt only. A lost reply does not prove the command was not applied.
pub(crate) fn set_budget(socket: &Path, daemon_pid: u32, bytes: std::num::NonZeroU64) -> Option<Budget> {
    let request = format!("budget {bytes}\n");
    parse_budget(&exchange(socket, daemon_pid, request.as_bytes(), READ_DEADLINE)?)
}

fn parse_budget(response: &[u8]) -> Option<Budget> {
    if response.len() >= MAX_REPLY || !response.is_ascii() || response.last() != Some(&b'\n') {
        return None;
    }
    let line = std::str::from_utf8(&response[..response.len() - 1]).ok()?;
    if line.contains(['\r', '\n', '\0']) { return None; }
    let mut words = line.split(' ');
    if words.next()? != "OK" { return None; }
    let mut fields = std::collections::HashMap::new();
    for word in words {
        let (key, value) = word.split_once('=')?;
        if key.is_empty() || value.is_empty() || fields.insert(key, value).is_some() { return None; }
    }
    if fields.get("version") != Some(&"1") || fields.get("budget_scope") != Some(&"rm-explicit-not-nvidia-smi") { return None; }
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten()
    };
    let optional = |text: &str| -> Option<Option<u64>> {
        if text == "unknown" { Some(None) } else { number(text).map(Some) }
    };
    let cap = optional(fields.get("budget_limit_bytes")?)?;
    if cap == Some(0) { return None; }
    let charged = number(fields.get("charged_bytes")?)?;
    let reservation = optional(fields.get("admission_reservation_bytes")?)?;
    if reservation != cap.map(|cap| cap.max(charged)) { return None; }
    Some(Budget { cap, charged, reservation })
}

/// One complete OK status line, with each required field exactly once. Extra named fields allow
/// protocol extension; unknown/partial/error responses cannot establish a safe reservation.
fn parse(response: &[u8]) -> Option<u64> {
    if response.len() >= MAX_REPLY || !response.is_ascii() || response.last() != Some(&b'\n') {
        return None;
    }
    let line = std::str::from_utf8(&response[..response.len() - 1]).ok()?;
    if line.contains(['\r', '\n', '\0']) {
        return None;
    }
    let mut words = line.split(' ');
    if words.next()? != "OK" {
        return None;
    }
    let mut version = None;
    let mut scope = None;
    let mut bytes = None;
    for word in words {
        let (key, value) = word.split_once('=')?;
        if key.is_empty() || value.is_empty() {
            return None;
        }
        let field = match key {
            "version" => &mut version,
            "budget_scope" => &mut scope,
            "admission_reservation_bytes" => &mut bytes,
            _ => continue,
        };
        if field.replace(value).is_some() {
            return None;
        }
    }
    if version? != "1" || scope? != "rm-explicit-not-nvidia-smi" {
        return None;
    }
    let digits = bytes?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Scratch;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    fn status(value: &str) -> String {
        format!("OK version=1 scope=requested-not-hardware budget_scope=rm-explicit-not-nvidia-smi admission_reservation_bytes={value}\n")
    }

    #[test]
    fn status_parser_requires_unique_valid_protocol_scope_and_unsigned_reservation() {
        assert_eq!(parse(status("4294967297").as_bytes()), Some(4294967297));
        for text in [
            status("unknown"), status("-1"), status("+1"), status("1.0"), status("18446744073709551616"),
            status("2").replace("version=1", "version=2"),
            status("2").replace("version=1", "version=1 version=1"),
            status("2").replace("budget_scope=rm-explicit-not-nvidia-smi", "budget_scope=unknown"),
            status("2").replace("admission_reservation_bytes=2", "admission_reservation_bytes=2 admission_reservation_bytes=3"),
            status("2").replace("budget_scope=rm-explicit-not-nvidia-smi", "budget_scope=rm-explicit-not-nvidia-smi budget_scope=rm-explicit-not-nvidia-smi"),
            status("2").replace("OK ", "PARTIAL "),
            status("2").replace("OK ", "ERROR "),
            status("2").replace("version=1 ", ""),
            status("2").replace("admission_reservation_bytes=2", "charged_bytes=2"),
            status("2") + "extra=x\n",
        ] {
            assert_eq!(parse(text.as_bytes()), None, "{text}");
        }
        assert_eq!(parse(&vec![b'x'; MAX_REPLY]), None);
    }

    #[test]
    fn socket_reply_is_accepted_only_from_expected_peer_pid() {
        let scratch = Scratch::new();
        for expected in [std::process::id(), std::process::id().wrapping_add(100000)] {
            let path = scratch.path(&format!("{expected}.sock"));
            let listener = UnixListener::bind(&path).unwrap();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut request = [0; 7];
                if stream.read_exact(&mut request).is_ok() {
                    assert_eq!(&request, b"status\n");
                    let _ = stream.write_all(status("3221225472").as_bytes());
                }
            });
            let actual = reservation(&path, expected);
            assert_eq!(actual, (expected == std::process::id()).then_some(3221225472));
            thread.join().unwrap();
        }
    }

    #[test]
    fn stalled_or_oversized_socket_reply_is_unknown() {
        let scratch = Scratch::new();
        for stall in [true, false] {
            let path = scratch.path(if stall { "stalled.sock" } else { "large.sock" });
            let listener = UnixListener::bind(&path).unwrap();
            let (release, wait) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 7];
                stream.read_exact(&mut request).unwrap();
                if stall {
                    let _ = wait.recv_timeout(Duration::from_secs(2));
                } else {
                    let _ = stream.write_all(&vec![b'x'; MAX_REPLY + 100]);
                }
            });
            let start = Instant::now();
            assert_eq!(reservation_with_timeout(&path, std::process::id(), Duration::from_millis(30)), None);
            assert!(start.elapsed() < Duration::from_secs(1));
            let _ = release.send(());
            thread.join().unwrap();
        }
    }
    #[test]
    fn budget_snapshot_requires_consistent_cap_charge_and_reservation() {
        let response = |cap, charged, reserved| format!("OK version=1 budget_scope=rm-explicit-not-nvidia-smi budget_limit_bytes={cap} charged_bytes={charged} admission_reservation_bytes={reserved}\n");
        assert_eq!(parse_budget(response("10", "20", "20").as_bytes()),
            Some(Budget { cap: Some(10), charged: 20, reservation: Some(20) }));
        assert_eq!(parse_budget(response("unknown", "20", "unknown").as_bytes()),
            Some(Budget { cap: None, charged: 20, reservation: None }));
        for text in [
            response("10", "20", "10"), response("0", "0", "0"),
            response("unknown", "20", "0"), response("10", "-1", "10"),
            response("10", "1", "10").replace("charged_bytes=1", "charged_bytes=1 charged_bytes=1"),
        ] { assert_eq!(parse_budget(text.as_bytes()), None, "{text}"); }
    }

}
