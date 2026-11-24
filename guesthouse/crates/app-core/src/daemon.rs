//! How the daemon is started and recognised. Its start-up is changing, so this is the one place that
//! knows it: `guesthouse-daemon BASE`, with the slot in `GUESTHOUSE_SLOT`, serves `BASE/run[-N]`.

use crate::context::{Context, SLOT_VARIABLE};
use crate::failure::Result;
use crate::guest_memory::GuestMemory;
use guesthouse_daemon::slot::Slot;
use guesthouse_daemon::{card_choice, jail};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::os::unix::fs::FileTypeExt;
use std::process::{Child, Command, Stdio};

pub const DAEMON_PROGRAM: &str = "guesthouse-daemon";
/// The sockets QEMU connects to, which the daemon makes once it's ready.
pub const SOCKETS: [&str; 2] = ["vgpu.sock", "global.sock"];
pub const OUTPUT_FILE: &str = "daemon.out";
/// Every new managed launch requires control v1, even when its socket is missing or deleted.
pub const CONTROL_VERSION_VARIABLE: &str = "GUESTHOUSE_CONTROL_VERSION";

/// What it takes to start the daemon for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStart {
    pub program: PathBuf,
    pub base: PathBuf,
    pub environment: BTreeMap<String, String>,
}

impl DaemonStart {
    /// `ram` and `vram` are the daemon's words for the guest's memory, as `16G`.
    pub fn new(context: &Context, slot: Slot, ram: &str, vram: Option<&str>) -> Self {
        let mut environment = BTreeMap::from([
            (SLOT_VARIABLE.to_owned(), slot.number().to_string()),
            ("GUESTHOUSE_RAM".to_owned(), ram.to_owned()),
            (CONTROL_VERSION_VARIABLE.to_owned(), "1".to_owned()),
        ]);
        if let Some(vram) = vram {
            environment.insert("GUESTHOUSE_VRAM".into(), vram.to_owned());
        }
        Self {
            program: context.paths.binary(DAEMON_PROGRAM),
            base: context.paths.runtime.clone(),
            environment,
        }
    }

    /// For a guest on an AMD card, which the daemon then opens instead of the NVIDIA card.
    pub fn on_amd_card(mut self, pci: &str) -> Self {
        self.environment.insert(card_choice::AMD_CARD_VARIABLE.to_owned(), pci.to_owned());
        self
    }

    /// For a machine allowed to start with no jail, on a PC that can't give it one.
    pub fn without_jail(mut self) -> Self {
        self.environment.insert(jail::VARIABLE.to_owned(), jail::OFF.to_owned());
        self
    }

    /// Starts it with the guest's memory open, where it looks for it. An omitted VRAM cap leaves
    /// GUESTHOUSE_VRAM inherited; legacy accounting therefore reads the live daemon's environment.
    pub fn spawn(&self, run_folder: &Path, memory: &GuestMemory) -> Result<Child> {
        let output = File::create(run_folder.join(OUTPUT_FILE))?;
        let mut command = Command::new(&self.program);
        memory.hand_to(&mut command);
        let spawned = detached(command.arg(&self.base))
            .envs(&self.environment)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()?;
        Ok(spawned)
    }
}

/// A process group of its own, so that what it starts outlives the task, and the app can close at any
/// moment. A task can then also be stopped with everything it started.
pub fn detached(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    command.process_group(0)
}

fn words_of(file: &Path) -> Vec<Vec<u8>> {
    let data = fs::read(file).unwrap_or_default();
    data.split(|byte| *byte == 0).filter(|word| !word.is_empty()).map(<[u8]>::to_vec).collect()
}

/// Whether this process is this install's daemon for this slot. Its slot isn't in its arguments, so
/// that comes from its environment.
pub fn is_own_daemon(context: &Context, pid: &str, slot: Slot) -> bool {
    let folder = context.processes.join(pid);
    let words = words_of(&folder.join("cmdline"));
    let [program, base, ..] = &words[..] else { return false };
    let is_daemon = Path::new(OsStr::new(&*String::from_utf8_lossy(program))).ends_with(DAEMON_PROGRAM);
    let wanted = format!("{SLOT_VARIABLE}={}", slot.number());
    let names_slot = words_of(&folder.join("environ")).iter().any(|word| word == wanted.as_bytes());
    is_daemon && *base == context.paths.runtime.as_os_str().as_encoded_bytes() && names_slot
}

/// Budget accounting must never truncate a partial GiB. Unrepresentable sizes return None,
/// which the caller treats as an unknown/whole-card reservation.
fn cap_gib_rounded_up(bytes: u64) -> Option<u32> {
    u32::try_from(bytes.div_ceil(1 << 30)).ok()
}

/// Only unmarked legacy daemons with a definitely absent control path may use startup env.
/// A new managed daemon's marker prevents a deleted socket from reviving a stale startup cap.
fn effective_cap(context: &Context, slot: Slot, pid: u32, environment: &[u8]) -> Option<u32> {
    let values = |prefix: &[u8]| environment.split(|byte| *byte == 0)
        .filter_map(move |word| word.strip_prefix(prefix)).collect::<Vec<_>>();
    let marker = values(b"GUESTHOUSE_CONTROL_VERSION=");
    let expected = match marker.as_slice() {
        [] => false,
        [b"1"] => true,
        _ => return None,
    };
    let socket = context.paths.run_folder(slot).join(guesthouse_daemon::control::SOCKET_NAME);
    match fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            return crate::daemon_control::reservation(&socket, pid).and_then(cap_gib_rounded_up);
        }
        Ok(_) => return None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !expected => {}
        Err(_) => return None,
    }
    let caps = values(b"GUESTHOUSE_VRAM=");
    let [value] = caps.as_slice() else { return None };
    let text = std::str::from_utf8(value).ok()?;
    guesthouse_daemon::environment::video_cap(Some(text)).and_then(cap_gib_rounded_up)
}

/// The live reservation (cap or already-charged memory) of a Windows daemon, not mutable settings. Missing,
/// unreadable or ambiguous identity/cap conservatively reserves the entire card (zero).
pub fn video_cap_in_use(context: &Context, slot: Slot) -> u32 {
    let mut found = Vec::new();
    for pid in crate::machine::sorted_names(&context.processes) {
        let Some(seen) = pid.parse().ok().and_then(|pid| crate::process_stat::Seen::now(context, pid)) else {
            continue;
        };
        if !is_own_daemon(context, &pid, slot) {
            continue;
        }
        let environment = fs::read(context.processes.join(&pid).join("environ"));
        let cap = environment.ok()
            .and_then(|bytes| effective_cap(context, slot, seen.pid, &bytes))
            .unwrap_or(0);
        if is_own_daemon(context, &pid, slot) && seen.is_alive(context) {
            found.push(cap);
        }
    }
    match found.as_slice() {
        [cap] => *cap,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};

    #[test]
    fn the_daemon_gets_the_guests_memory_and_its_slot() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let start = DaemonStart::new(&context, slots::number(2), "16G", Some("8G"));
        assert_eq!(start.environment["GUESTHOUSE_SLOT"], "2");
        assert_eq!(start.environment[CONTROL_VERSION_VARIABLE], "1");
        assert_eq!(
            (start.environment["GUESTHOUSE_RAM"].as_str(), start.environment["GUESTHOUSE_VRAM"].as_str()),
            ("16G", "8G")
        );
        assert_eq!(start.base, context.paths.runtime);
        assert_eq!(context.paths.run_folder(slots::number(2)), start.base.join("run-2"));
    }

    #[test]
    fn a_guest_on_an_amd_card_names_it_and_one_on_nvidia_names_none() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let start = DaemonStart::new(&context, slots::first(), "16G", None);
        assert!(!start.environment.contains_key("GUESTHOUSE_AMD_CARD"));
        let start = start.on_amd_card("0000:7c:00.0");
        assert_eq!(start.environment["GUESTHOUSE_AMD_CARD"], "0000:7c:00.0");
    }

    #[test]
    fn an_omitted_cap_preserves_inheritance_and_accounting_reads_the_effective_value() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        for (requested, effective, budget) in
            [(None, "3G", 3), (Some("5G"), "5G", 5), (Some("1536M"), "1536M", 0)]
        {
            let start = DaemonStart::new(&context, slots::first(), "16G", requested);
            // Match spawn's envs overlay without changing this test process's global environment.
            let output = Command::new("/bin/sh")
                .args(["-c", r#"printf %s "$GUESTHOUSE_VRAM""#])
                .env("GUESTHOUSE_VRAM", "3G")
                .envs(&start.environment)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, effective.as_bytes());
            let variable = format!("GUESTHOUSE_VRAM={effective}");
            crate::processes::stand_in::fake_process(
                &context,
                91,
                &["/build/guesthouse-daemon", context.paths.runtime.to_str().unwrap()],
                &["GUESTHOUSE_SLOT=0", &variable],
                "",
            );
            assert_eq!(video_cap_in_use(&context, slots::first()), budget);
        }
        // This daemon version accepts only integer G. An unsupported M cap is uncapped, and
        // accounting conservatively reserves the whole card rather than inventing 1 GiB.
        assert_eq!(guesthouse_daemon::environment::video_cap(Some("1536M")), None);
    }

    #[test]
    fn missing_socket_is_legacy_only_without_a_protocol_marker() {
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let legacy = b"GUESTHOUSE_SLOT=0\0GUESTHOUSE_VRAM=3G\0";
        assert_eq!(effective_cap(&context, slots::first(), 101, legacy), Some(3));
        for marker in ["1", "2", "", "1\0GUESTHOUSE_CONTROL_VERSION=1"] {
            let env = format!("GUESTHOUSE_SLOT=0\0GUESTHOUSE_VRAM=3G\0GUESTHOUSE_CONTROL_VERSION={marker}\0");
            assert_eq!(effective_cap(&context, slots::first(), 101, env.as_bytes()), None);
        }
        let folder = context.paths.run_folder(slots::first());
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("control.sock"), b"not a socket").unwrap();
        assert_eq!(effective_cap(&context, slots::first(), 101, legacy), None);
    }

    #[test]
    fn live_control_reservation_replaces_startup_env_and_preserves_seen_checks() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let folder = context.paths.run_folder(slots::first());
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("control.sock");
        let pid = std::process::id();
        for exits in [false, true] {
            crate::processes::stand_in::fake_process(
                &context, pid, &["/build/guesthouse-daemon", context.paths.runtime.to_str().unwrap()],
                &["GUESTHOUSE_SLOT=0", "GUESTHOUSE_VRAM=1G", "GUESTHOUSE_CONTROL_VERSION=1"], "",
            );
            let _ = fs::remove_file(&path);
            let listener = UnixListener::bind(&path).unwrap();
            let stat = context.processes.join(format!("{pid}/stat"));
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 7];
                stream.read_exact(&mut request).unwrap();
                if exits { fs::remove_file(stat).unwrap(); }
                stream.write_all(b"OK version=1 budget_scope=rm-explicit-not-nvidia-smi admission_reservation_bytes=4294967297\n").unwrap();
            });
            assert_eq!(video_cap_in_use(&context, slots::first()), if exits { 0 } else { 5 });
            thread.join().unwrap();
        }
        // A new managed daemon cannot fall back to its old 1G env after the socket is deleted.
        fs::remove_file(&path).unwrap();
        assert_eq!(effective_cap(&context, slots::first(), pid,
            b"GUESTHOUSE_SLOT=0\0GUESTHOUSE_VRAM=1G\0GUESTHOUSE_CONTROL_VERSION=1\0"), None);
    }

    #[test]
    fn present_but_broken_control_socket_never_uses_legacy_env() {
        use std::os::unix::net::UnixListener;
        let scratch = Scratch::new();
        let context = scratch_context(&scratch);
        let folder = context.paths.run_folder(slots::first());
        fs::create_dir_all(&folder).unwrap();
        let listener = UnixListener::bind(folder.join("control.sock")).unwrap();
        drop(listener); // Path exists, but nobody serves it.
        assert_eq!(effective_cap(&context, slots::first(), 101, b"GUESTHOUSE_VRAM=3G\0"), None);
    }

    #[test]
    fn partial_gib_caps_round_up_without_overflow() {
        assert_eq!(cap_gib_rounded_up(1536 << 20), Some(2));
        assert_eq!(cap_gib_rounded_up((1 << 30) + 1), Some(2));
        assert_eq!(cap_gib_rounded_up(4 << 30), Some(4));
        assert_eq!(cap_gib_rounded_up(1), Some(1));
        assert_eq!(cap_gib_rounded_up(u64::MAX), None);
    }
}
