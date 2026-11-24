//! Owner-only, one-request host control socket for this daemon's backend.
//!
//! Protocol v1: "status\n", "timeslice <nonzero-us>\n", "budget <nonzero-bytes>\n",
//! or "interleave <0|1|2>\n" (LOW, MEDIUM, HIGH). No privilege changes are made.
//! Replies are bounded text with named fields; unknown future fields may be ignored.
//! RM acceptance is not hardware readback. No client supplies RM or guest handles.

use guesthouse_backend::{Backend, GuestMemory};
use guesthouse_core::Session;
use std::fs::{self, Permissions};
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroU64;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

pub const SOCKET_NAME: &str = "control.sock";
const MAX_BYTES: usize = 4096;
const CLIENT_LIFETIME: Duration = Duration::from_secs(1);

struct Client {
    stream: UnixStream,
    expires: Instant,
    request: Vec<u8>,
    reply: Option<Vec<u8>>,
    sent: usize,
}

pub(crate) struct ControlPort {
    listener: UnixListener,
    owner: libc::uid_t,
    client: Option<Client>,
}

impl ControlPort {
    pub(crate) fn bind(folder: &Path) -> io::Result<Self> {
        let path = folder.join(SOCKET_NAME);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => return Err(io::Error::new(ErrorKind::AlreadyExists, "control path is not a socket")),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        // SAFETY: geteuid takes no arguments and has no failure case.
        let owner = unsafe { libc::geteuid() };
        Ok(Self { listener, owner, client: None })
    }

    /// One pending client; do not accept more until it completes or expires.
    pub(crate) fn wait_on(&self) -> (i32, i32, bool) {
        match &self.client {
            Some(client) => (-1, client.stream.as_raw_fd(), client.reply.is_some()),
            None => (self.listener.as_raw_fd(), -1, false),
        }
    }

    pub(crate) fn timeout(&self, now: Instant, other: Option<Duration>) -> Option<Duration> {
        let Some(client) = &self.client else { return other };
        let left = client.expires.saturating_duration_since(now);
        Some(other.map_or(left, |timeout| timeout.min(left)))
    }

    pub(crate) fn accept(&mut self, now: Instant) {
        if self.client.is_some() {
            return;
        }
        let Ok((stream, _)) = self.listener.accept() else { return };
        // Peer credentials close the bind-to-chmod race and deny other users even if mode changes.
        if !same_owner(&stream, self.owner) || stream.set_nonblocking(true).is_err() {
            return;
        }
        self.client = Some(Client {
            stream, expires: now + CLIENT_LIFETIME,
            request: Vec::new(), reply: None, sent: 0,
        });
    }

    /// At most one bounded read and one bounded write; never wait on a client.
    /// Kernel driver control calls retain the driver's own blocking/error semantics.
    pub(crate) fn service<B: Backend, M: GuestMemory>(&mut self, session: &mut Session<B, M>, now: Instant) {
        let Some(client) = &mut self.client else { return };
        if now >= client.expires {
            self.client = None;
            return;
        }
        if client.reply.is_none() {
            let mut bytes = [0; MAX_BYTES];
            let room = MAX_BYTES - client.request.len();
            match client.stream.read(&mut bytes[..room]) {
                Ok(0) => {
                    self.client = None;
                    return;
                }
                Ok(count) => client.request.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.client = None;
                    return;
                }
            }
            if client.request.contains(&b'\n') {
                client.reply = Some(answer(session, &client.request));
            } else if client.request.len() == MAX_BYTES {
                client.reply = Some(b"ERROR version=1 reason=request-too-long\n".to_vec());
            }
        }
        if let Some(reply) = &client.reply {
            match client.stream.write(&reply[client.sent..]) {
                Ok(0) => self.client = None,
                Ok(count) => {
                    client.sent += count;
                    if client.sent == reply.len() {
                        self.client = None;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.client = None,
            }
        }
    }
}

fn same_owner(stream: &UnixStream, owner: libc::uid_t) -> bool {
    let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials and length are valid output buffers of the declared sizes.
    let result = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(), &mut length)
    };
    result == 0 && length as usize == std::mem::size_of::<libc::ucred>() && credentials.uid == owner
}

fn safe_reason(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(160)
        .map(|character| if character.is_ascii_alphanumeric() || "-_.:".contains(character) {
            character
        } else { '_' }).collect()
}

fn budget_fields(state: guesthouse_backend::video_budget::VideoBudgetState) -> String {
    let number = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".into(), |n| n.to_string());
    format!("budget_scope=rm-explicit-not-nvidia-smi budget_limit_bytes={} charged_bytes={} live_object_bytes={} retained_export_bytes={} failed_rollback_bytes={} admission_reservation_bytes={}",
        number(state.limit.map(NonZeroU64::get)), state.charged_bytes, state.live_object_bytes,
        state.retained_export_bytes, state.failed_rollback_bytes,
        number(state.admission_reservation_bytes()))
}

fn answer<B: Backend, M: GuestMemory>(session: &mut Session<B, M>, request: &[u8]) -> Vec<u8> {
    let invalid = || b"ERROR version=1 reason=invalid-request\n".to_vec();
    if request.len() > MAX_BYTES || !request.is_ascii() || request.last() != Some(&b'\n') {
        return invalid();
    }
    let line = &request[..request.len() - 1];
    if line == b"status" {
        // The daemon serves this and guest allocations on the same thread: both components are
        // one coherent snapshot. A missing/uncapped budget must never look like zero reservation.
        let scheduling = session.backend().graphics_timeslice_state();
        let budget = session.backend().video_memory_budget_state();
        let interleave = session.backend().graphics_interleave_state();
        let available = [scheduling.is_ok(), budget.is_ok(), interleave.is_ok()];
        let result = if available.iter().all(|ok| *ok) { "OK" }
            else if available.iter().all(|ok| !*ok) { "ERROR" } else { "PARTIAL" };
        let interleave = match interleave {
            Ok(state) => format!("requested_interleave_level={} interleave_scope=requested-not-hardware",
                state.requested_level.map_or_else(|| "unset".into(), |level| (level as u32).to_string())),
            Err(error) => format!("interleave_error={}", safe_reason(error)),
        };
        let timeslice = match scheduling {
            Ok(state) => format!("requested_timeslice_us={} graphics_queues={} scope=requested-not-hardware",
                state.requested_micros.map_or(0, NonZeroU64::get), state.graphics_queues),
            Err(error) => format!("timeslice_error={}", safe_reason(error)),
        };
        let budget = match budget {
            Ok(state) => budget_fields(state),
            Err(error) => format!("budget_error={} admission_reservation_bytes=unknown", safe_reason(error)),
        };
        return format!("{result} version=1 {timeslice} {budget} {interleave}\n").into_bytes();
    }
    if let Some(digits) = line.strip_prefix(b"interleave ") {
        // Exactly one of the documented levels: zero is LOW, not "leave the default".
        let level = match digits {
            b"0" => guesthouse_backend::scheduling::GraphicsInterleaveLevel::Low,
            b"1" => guesthouse_backend::scheduling::GraphicsInterleaveLevel::Medium,
            b"2" => guesthouse_backend::scheduling::GraphicsInterleaveLevel::High,
            _ => return invalid(),
        };
        return match session.backend_mut().request_graphics_interleave(level) {
            Ok(report) => format_interleave_report(report),
            Err(error) => format!("ERROR version=1 reason={}\n", safe_reason(error)).into_bytes(),
        };
    }
    let (budget_request, digits) = match line.strip_prefix(b"budget ") {
        Some(digits) => (true, digits),
        None => match line.strip_prefix(b"timeslice ") {
            Some(digits) => (false, digits),
            None => return invalid(),
        },
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return invalid();
    }
    let Some(value) = std::str::from_utf8(digits).ok()
        .and_then(|text| text.parse().ok()).and_then(NonZeroU64::new) else { return invalid() };
    if budget_request {
        return match session.request_video_memory_budget(value) {
            Ok(state) => format!("OK version=1 {} guest_budget_update=future-queries\n",
                budget_fields(state)).into_bytes(),
            Err(error) => format!("ERROR version=1 reason={}\n", safe_reason(error)).into_bytes(),
        };
    }
    let report = match session.backend_mut().request_graphics_timeslice(value) {
        Ok(report) => report,
        Err(error) => return format!("ERROR version=1 reason={}\n", safe_reason(error)).into_bytes(),
    };
    format_report(report)
}

fn format_report(report: guesthouse_backend::scheduling::GraphicsTimesliceUpdate) -> Vec<u8> {
    format_queue_report("requested_timeslice_us", report.requested_micros.get(),
        report.queues, false)
}

fn format_interleave_report(report: guesthouse_backend::scheduling::GraphicsInterleaveUpdate) -> Vec<u8> {
    format_queue_report("requested_interleave_level", report.requested_level as u64,
        report.queues, true)
}

fn format_queue_report(
    field: &str, value: u64,
    queues: Vec<(u32, Result<(), guesthouse_backend::BackendError>)>,
    interleave: bool,
) -> Vec<u8> {
    let failed = queues.iter().filter(|(_, result)| result.is_err()).count();
    let result = if failed == 0 { "OK" }
        else if interleave && failed == queues.len() { "ERROR" } else { "PARTIAL" };
    let scope = if interleave && queues.is_empty() { "future-request-only" } else { "rm-acceptance" };
    let mut reply = format!("{result} version=1 {field}={value} attempted={} accepted={} failed={} scope={scope}\n",
        queues.len(), queues.len() - failed, failed);
    let mut omitted = 0;
    for (queue, outcome) in queues {
        if let Err(error) = outcome {
            let line = format!("queue={queue} error={}\n", safe_reason(error));
            if reply.len() + line.len() + 64 <= MAX_BYTES {
                reply.push_str(&line);
            } else {
                omitted += 1;
            }
        }
    }
    if omitted > 0 {
        reply.push_str(&format!("omitted_errors={omitted}\n"));
    }
    reply.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use guesthouse_backend::BackendError;
    use guesthouse_backend::scheduling::GraphicsTimesliceUpdate;
    use guesthouse_backend_nvidia::fake_guest::fake_backend;
    use guesthouse_backend_nvidia::rm::fake::FakeRm;
    use guesthouse_backend_standin::StandIn;
    use std::path::PathBuf;

    fn test_session<B: Backend>(backend: B) -> Session<B, guesthouse_backend_standin::SparseMemory> {
        let limits = guesthouse_core::MemoryLimits::new(backend.facts(), Some(1 << 20), 8 << 30);
        Session::new(backend, guesthouse_backend_standin::SparseMemory::new((1 << 40) + (1 << 30)),
            guesthouse_core::Config::new(limits))
    }

    fn guest_budget<B: Backend, M: GuestMemory>(session: &mut Session<B, M>) -> u64 {
        use guesthouse_messages::{Command, wire::{set_u32, u64_at}};
        let mut message = vec![0; 0x20];
        set_u32(&mut message, 16, Command::QueryVideoMemory.to_wire());
        let mut reply = Vec::new();
        assert_eq!(session.handle_message(&message, None, &mut reply), guesthouse_core::Answer::Now);
        u64_at(&reply, 0).unwrap()
    }

    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            let output = std::process::Command::new("mktemp")
                .args(["-d", "/tmp/gh-control.XXXXXX"]).output().unwrap();
            assert!(output.status.success());
            Self(PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()))
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn control_rejects_malformed_zero_overflow_and_multiple_commands() {
        let mut backend = test_session(fake_backend(FakeRm::new()));
        for request in [
            b"timeslice 0\n".as_slice(), b"timeslice -1\n", b"timeslice 18446744073709551616\n",
            b"timeslice 1\nstatus\n", b"status\nstatus\n", b"status", b"timeslice \n",
            b"timeslice 1 \n", b"timeslice \xff\n",
            b"budget 0\n", b"budget -1\n", b"budget 18446744073709551616\n",
            b"budget 1\nstatus\n", b"budget 1 \n",
        ] {
            assert!(answer(&mut backend, request).starts_with(b"ERROR"), "{request:?}");
        }
        assert_eq!(backend.backend().graphics_timeslice_state().unwrap().requested_micros, None);
        assert!(answer(&mut backend, &vec![b'x'; MAX_BYTES + 1]).starts_with(b"ERROR"));
    }

    #[test]
    fn control_status_reports_requested_value_and_unsupported_backends_refuse() {
        let mut backend = test_session(fake_backend(FakeRm::new()));
        let initial = String::from_utf8(answer(&mut backend, b"status\n")).unwrap();
        assert!(initial.contains("requested_timeslice_us=0"));
        let changed = String::from_utf8(answer(&mut backend, b"timeslice 4000\n")).unwrap();
        assert!(changed.starts_with("OK "));
        assert!(changed.contains("attempted=0"));
        let status = String::from_utf8(answer(&mut backend, b"status\n")).unwrap();
        assert!(status.contains("requested_timeslice_us=4000"));
        assert!(status.contains("scope=requested-not-hardware"));
        let mut other = test_session(StandIn::new());
        assert!(answer(&mut other, b"status\n").starts_with(b"ERROR"));
        assert!(answer(&mut other, b"timeslice 4000\n").starts_with(b"ERROR"));
    }

    #[test]
    fn control_partial_result_is_explicit_and_reply_is_bounded() {
        let report = GraphicsTimesliceUpdate {
            requested_micros: NonZeroU64::new(4000).unwrap(),
            queues: (0..400).map(|id| (id, if id == 0 { Ok(()) } else {
                Err(BackendError::new("driver rejected\nspoof".repeat(30)))
            })).collect(),
        };
        let response = format_report(report);
        assert!(response.len() <= MAX_BYTES);
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("PARTIAL "));
        assert!(text.contains("accepted=1 failed=399"));
        assert!(text.contains("omitted_errors="));
        assert!(!text.contains("\nspoof"));
    }

    #[test]
    fn control_half_request_is_nonblocking_and_expires_without_action() {
        let folder = Folder::new();
        let mut port = ControlPort::bind(&folder.0).unwrap();
        let mut peer = UnixStream::connect(folder.0.join(SOCKET_NAME)).unwrap();
        let now = Instant::now();
        port.accept(now);
        peer.write_all(b"timeslice 4").unwrap();
        let mut backend = test_session(fake_backend(FakeRm::new()));
        port.service(&mut backend, now);
        assert!(port.client.is_some());
        assert_eq!(backend.backend().graphics_timeslice_state().unwrap().requested_micros, None);
        assert!(port.timeout(now, None).is_some());
        port.service(&mut backend, now + CLIENT_LIFETIME);
        assert!(port.client.is_none());
        assert_eq!(backend.backend().graphics_timeslice_state().unwrap().requested_micros, None);
    }

    #[test]
    fn control_fragmented_request_executes_once_and_closes_after_reply() {
        let folder = Folder::new();
        let mut port = ControlPort::bind(&folder.0).unwrap();
        let mut peer = UnixStream::connect(folder.0.join(SOCKET_NAME)).unwrap();
        let now = Instant::now();
        port.accept(now);
        peer.write_all(b"timeslice ").unwrap();
        let mut backend = test_session(fake_backend(FakeRm::new()));
        port.service(&mut backend, now);
        peer.write_all(b"2000\n").unwrap();
        port.service(&mut backend, now);
        let mut response = String::new();
        peer.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("OK "));
        assert!(port.client.is_none());
        assert_eq!(backend.backend().graphics_timeslice_state().unwrap().requested_micros.unwrap().get(), 2000);
    }

    #[test]
    fn control_oversized_unterminated_request_has_bounded_error() {
        let folder = Folder::new();
        let mut port = ControlPort::bind(&folder.0).unwrap();
        let mut peer = UnixStream::connect(folder.0.join(SOCKET_NAME)).unwrap();
        let now = Instant::now();
        port.accept(now);
        peer.write_all(&vec![b'x'; MAX_BYTES]).unwrap();
        port.service(&mut test_session(StandIn::new()), now);
        let mut response = String::new();
        peer.read_to_string(&mut response).unwrap();
        assert!(response.contains("request-too-long"));
    }

    #[test]
    fn control_socket_is_owner_only_and_peer_identity_is_checked() {
        let folder = Folder::new();
        let mut port = ControlPort::bind(&folder.0).unwrap();
        assert_eq!(fs::metadata(folder.0.join(SOCKET_NAME)).unwrap().permissions().mode() & 0o777, 0o600);
        let (one, _two) = UnixStream::pair().unwrap();
        assert!(same_owner(&one, port.owner));
        assert!(!same_owner(&one, port.owner.wrapping_add(1)));
        port.owner = port.owner.wrapping_add(1);
        let _peer = UnixStream::connect(folder.0.join(SOCKET_NAME)).unwrap();
        port.accept(Instant::now());
        assert!(port.client.is_none());
    }

    #[test]
    fn control_existing_non_socket_is_preserved() {
        let folder = Folder::new();
        fs::write(folder.0.join(SOCKET_NAME), b"preserve").unwrap();
        assert!(ControlPort::bind(&folder.0).is_err());
        assert_eq!(fs::read(folder.0.join(SOCKET_NAME)).unwrap(), b"preserve");
    }

    #[test]
    fn control_budget_updates_enforcement_future_guest_queries_and_reservation() {
        use guesthouse_backend::MemoryKind;
        let mut session = test_session(fake_backend(FakeRm::new()));
        let initial = String::from_utf8(answer(&mut session, b"status\n")).unwrap();
        assert!(initial.contains("budget_limit_bytes=unknown"));
        assert!(initial.contains("admission_reservation_bytes=unknown"));
        assert_eq!(guest_budget(&mut session), 1 << 20);

        assert!(answer(&mut session, b"budget 8192\n").starts_with(b"OK"));
        let (memory, _) = session.backend_mut().allocate_video_memory(8192, MemoryKind::PLAIN).unwrap();
        assert!(session.backend_mut().allocate_video_memory(1, MemoryKind::PLAIN).unwrap_err().is_no_video_memory());
        let lowered = String::from_utf8(answer(&mut session, b"budget 4096\n")).unwrap();
        assert!(lowered.contains("budget_limit_bytes=4096"));
        assert!(lowered.contains("charged_bytes=8192"));
        assert!(lowered.contains("admission_reservation_bytes=8192"));
        assert!(lowered.contains("guest_budget_update=future-queries"));
        assert_eq!(guest_budget(&mut session), 4096);
        session.backend_mut().release_memory(memory);
        let status = String::from_utf8(answer(&mut session, b"status\n")).unwrap();
        assert!(status.contains("charged_bytes=0 "));
        assert!(status.contains("admission_reservation_bytes=4096"));
        assert!(answer(&mut session, b"budget 12288\n").starts_with(b"OK"));
        assert_eq!(guest_budget(&mut session), 12288);
        session.backend_mut().allocate_video_memory(12288, MemoryKind::PLAIN).unwrap();
        assert!(status.contains("requested_timeslice_us=0"));
        assert!(status.contains("budget_scope=rm-explicit-not-nvidia-smi"));
    }

    #[test]
    fn control_budget_failure_preserves_guest_configuration_and_unavailable_is_unknown() {
        let mut session = test_session(StandIn::new());
        assert_eq!(guest_budget(&mut session), 1 << 20);
        assert!(answer(&mut session, b"budget 4096\n").starts_with(b"ERROR"));
        assert_eq!(guest_budget(&mut session), 1 << 20);
        let status = String::from_utf8(answer(&mut session, b"status\n")).unwrap();
        assert!(status.contains("admission_reservation_bytes=unknown"));
        assert!(!status.contains("admission_reservation_bytes=0"));
    }

    #[test]
    fn control_budget_snapshot_never_lends_retained_objects_or_an_unknown_limit() {
        let mut state = guesthouse_backend::video_budget::VideoBudgetState {
            limit: NonZeroU64::new(100), charged_bytes: 900, live_object_bytes: 500,
            retained_export_bytes: 400, failed_rollback_bytes: 200,
        };
        let text = budget_fields(state);
        assert!(text.contains("admission_reservation_bytes=900"));
        assert!(text.contains("live_object_bytes=500"));
        assert!(text.contains("retained_export_bytes=400"));
        assert!(text.contains("failed_rollback_bytes=200"));
        state.limit = None;
        assert!(budget_fields(state).contains("admission_reservation_bytes=unknown"));
        state.limit = NonZeroU64::new(1000);
        assert!(budget_fields(state).contains("admission_reservation_bytes=1000"));
    }

    #[test]
    fn control_interleave_validates_levels_and_keeps_unset_distinct_from_low() {
        let mut session = test_session(fake_backend(FakeRm::new()));
        let initial = String::from_utf8(answer(&mut session, b"status\n")).unwrap();
        assert!(initial.contains("requested_interleave_level=unset"));
        for request in [b"interleave 3\n".as_slice(), b"interleave -1\n", b"interleave 00\n",
            b"interleave 1 \n", b"interleave 1\nstatus\n", b"interleave 4294967296\n"] {
            assert!(answer(&mut session, request).starts_with(b"ERROR"));
        }
        assert_eq!(session.backend().graphics_interleave_state().unwrap().requested_level, None);
        for level in [2, 1, 0] {
            let reply = String::from_utf8(answer(&mut session, format!("interleave {level}\n").as_bytes())).unwrap();
            assert!(reply.starts_with("OK "));
            assert!(reply.contains("attempted=0 accepted=0 failed=0 scope=future-request-only"));
            let status = String::from_utf8(answer(&mut session, b"status\n")).unwrap();
            assert!(status.contains(&format!("requested_interleave_level={level} ")));
            assert!(status.contains("interleave_scope=requested-not-hardware"));
            assert!(status.contains("requested_timeslice_us=0"));
            assert!(status.contains("admission_reservation_bytes=unknown"));
        }
        let mut other = test_session(StandIn::new());
        assert!(String::from_utf8(answer(&mut other, b"interleave 1\n")).unwrap()
            .contains("ERROR version=1 reason=graphics_interleave_control_unsupported"));
    }

    #[test]
    fn control_interleave_all_refused_is_error_and_mixed_report_is_bounded_partial() {
        use guesthouse_backend::scheduling::{GraphicsInterleaveLevel, GraphicsInterleaveUpdate};
        let all_failed = GraphicsInterleaveUpdate {
            requested_level: GraphicsInterleaveLevel::High,
            queues: vec![(7, Err(BackendError::new("control 0xa06c0107: status 0x1b")))],
        };
        let text = String::from_utf8(format_interleave_report(all_failed)).unwrap();
        assert!(text.starts_with("ERROR "));
        assert!(text.contains("accepted=0 failed=1"));
        assert!(text.contains("queue=7 error=control_0xa06c0107:_status_0x1b"));
        let mixed = GraphicsInterleaveUpdate {
            requested_level: GraphicsInterleaveLevel::Low,
            queues: (0..400).map(|id| (id, if id == 0 { Ok(()) } else {
                Err(BackendError::new("unsupported\nspoof".repeat(30)))
            })).collect(),
        };
        let reply = format_interleave_report(mixed);
        assert!(reply.len() <= MAX_BYTES);
        let text = String::from_utf8(reply).unwrap();
        assert!(text.starts_with("PARTIAL "));
        assert!(text.contains("requested_interleave_level=0 attempted=400 accepted=1 failed=399"));
        assert!(text.contains("omitted_errors="));
        assert!(!text.contains("\nspoof"));
    }

}
