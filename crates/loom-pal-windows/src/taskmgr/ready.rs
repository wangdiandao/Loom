//! Three-event readiness handshake (`ready`/`commit`/`done`) and zero-data foreground named pipe
//! (`AllowSetForegroundWindow`) between the IFEO launcher and the Loom UI process.

use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_WRITE_DATA, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_REJECT_REMOTE_CLIENTS,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, GetCurrentProcess, GetExitCodeProcess, OpenEventW,
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

use crate::handle::{OwnedHandle, to_wide_null};

use super::launch;

/// CLI flag passed from the IFEO launcher to the Loom UI process.
pub const HANDSHAKE_ARG: &str = "--loom-taskmgr-entry";

/// SDDL revision 1 (`SDDL_REVISION_1`).
const SDDL_REVISION_1: u32 = 1;

static PENDING: Mutex<Vec<HandshakeRequest>> = Mutex::new(Vec::new());

#[derive(Clone, Debug)]
struct HandshakeRequest {
    nonce: String,
    launcher_pid: u32,
    launcher_created: u64,
}

/// Owned Windows `SECURITY_DESCRIPTOR` allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
pub(crate) struct OwnedSecurityDescriptor {
    raw: PSECURITY_DESCRIPTOR,
}

impl OwnedSecurityDescriptor {
    pub(crate) fn for_owner(owner_sid: &str) -> Result<Self, String> {
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{owner_sid})");
        let wide = to_wide_null(&sddl);
        let mut raw: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: Converts valid SDDL string into an OS-allocated security descriptor.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut raw,
                ptr::null_mut(),
            )
        };
        if ok == 0 || raw.is_null() {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW failed (os error {code})"
            ));
        }
        Ok(Self { raw })
    }

    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.raw,
            bInheritHandle: FALSE,
        }
    }
}

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: Allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
            unsafe {
                LocalFree(self.raw.cast::<c_void>());
            }
        }
    }
}

fn event_name(nonce: &str, suffix: &str) -> Vec<u16> {
    to_wide_null(&format!("Local\\Loom.Taskmgr.{nonce}.{suffix}"))
}

fn foreground_pipe_name(nonce: &str) -> Vec<u16> {
    to_wide_null(&format!(r"\\.\pipe\Loom.Taskmgr.{nonce}.foreground"))
}

pub(crate) fn valid_nonce(nonce: &str) -> bool {
    (16..=64).contains(&nonce.len()) && nonce.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Generates a non-predictable hex nonce using process ID, monotonic counter, and high-res time.
pub fn generate_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in tid.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{pid:08x}-{seq:08x}-{:016x}-{hash:016x}", nanos as u64)
}

/// Parses `--loom-taskmgr-entry <nonce> <pid> <created>` from CLI or single-instance args
/// and enqueues it for [`complete_pending`]. Returns `true` if the flag was present.
pub fn queue_args(args: &[String]) -> bool {
    let Some(at) = args.iter().position(|a| a == HANDSHAKE_ARG) else {
        return false;
    };
    if let [nonce, pid, created, ..] = &args[at + 1..]
        && valid_nonce(nonce)
        && let (Ok(launcher_pid), Ok(launcher_created)) =
            (pid.parse::<u32>(), created.parse::<u64>())
        && launcher_pid != 0
        && launcher_created != 0
        && let Ok(mut pending) = PENDING.lock()
        && pending.len() < 8
    {
        pending.push(HandshakeRequest {
            nonce: nonce.clone(),
            launcher_pid,
            launcher_created,
        });
    }
    true
}

/// Validated handshake completion token returned to the Loom UI process.
pub struct Completion {
    parent: OwnedHandle,
    done: OwnedHandle,
}

impl Completion {
    /// Returns `true` if the launching process is still waiting (not expired).
    pub fn should_show(&self) -> bool {
        unsafe { WaitForSingleObject(self.parent.as_raw(), 0) != WAIT_OBJECT_0 }
    }

    /// Signals `done` so the IFEO launcher knows the Loom window is live and exits cleanly.
    pub fn acknowledge(self) {
        unsafe {
            let _ = SetEvent(self.done.as_raw());
        }
    }
}

/// Completes any pending readiness handshakes queued from launch or single-instance wake args.
pub fn complete_pending() -> Vec<Completion> {
    let requests = match PENDING.lock() {
        Ok(mut guard) => std::mem::take(&mut *guard),
        Err(_) => return Vec::new(),
    };
    let Ok(expected_exe) = std::env::current_exe() else {
        return Vec::new();
    };
    requests
        .into_iter()
        .filter_map(|req| complete_one(req, &expected_exe).ok())
        .collect()
}

fn complete_one(req: HandshakeRequest, expected_launcher: &Path) -> Result<Completion, String> {
    // SAFETY: Opens the launching process for identity & lifetime verification.
    let raw_parent = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            FALSE,
            req.launcher_pid,
        )
    };
    let parent =
        OwnedHandle::from_raw(raw_parent).ok_or_else(|| "launcher process gone".to_owned())?;

    if launch::process_create_time(parent.as_raw())? != req.launcher_created {
        return Err("launcher creation time mismatch".into());
    }

    let mut img_buf = [0u16; 32_768];
    let mut img_len = img_buf.len() as u32;
    // SAFETY: Reads the Win32 executable path of the launcher process.
    let ok = unsafe {
        QueryFullProcessImageNameW(parent.as_raw(), 0, img_buf.as_mut_ptr(), &mut img_len)
    };
    if ok == 0 {
        return Err("QueryFullProcessImageNameW failed".into());
    }
    let parent_path = String::from_utf16_lossy(&img_buf[..img_len as usize]);
    let expected_str = canonical_win32_path(expected_launcher);
    if !parent_path.eq_ignore_ascii_case(&expected_str) {
        return Err("unexpected launcher image path".into());
    }

    let open_ev = |suffix: &str| -> Result<OwnedHandle, String> {
        let wide = event_name(&req.nonce, suffix);
        // SAFETY: Opens event created by the launcher in the current session.
        let raw = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
                FALSE,
                wide.as_ptr(),
            )
        };
        OwnedHandle::from_raw(raw).ok_or_else(|| format!("OpenEventW({suffix}) failed"))
    };

    let ready = open_ev("ready")?;
    let commit = open_ev("commit")?;
    let done = open_ev("done")?;

    // Connect to the zero-data foreground named pipe so the launcher learns our real UI PID
    // (even if the spawned child forwarded via single-instance and exited) and calls
    // `AllowSetForegroundWindow(our_pid)`.
    let _fg_pipe = connect_foreground_pipe(&req.nonce);

    // SAFETY: Signals `ready` to the launcher.
    if unsafe { SetEvent(ready.as_raw()) } == 0 {
        return Err("SetEvent(ready) failed".into());
    }

    if !launch::wait_event_while_alive(commit.as_raw(), parent.as_raw(), Duration::from_secs(2)) {
        return Err("launcher did not commit handshake in time".into());
    }

    Ok(Completion { parent, done })
}

fn canonical_win32_path(path: &Path) -> String {
    let raw = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_owned()
}

fn listen_foreground_pipe(nonce: &str, sd: &OwnedSecurityDescriptor) -> Option<OwnedHandle> {
    let wide = foreground_pipe_name(nonce);
    let attrs = sd.attributes();
    // SAFETY: Creates a single-instance local inbound named pipe rejecting remote clients.
    let raw = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            0,
            0,
            &attrs,
        )
    };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        None
    } else {
        OwnedHandle::from_raw(raw)
    }
}

fn connect_foreground_pipe(nonce: &str) -> Option<OwnedHandle> {
    let wide = foreground_pipe_name(nonce);
    // SAFETY: Connects with anonymous impersonation solely to expose client PID to launcher.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_DATA,
            0,
            ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        None
    } else {
        OwnedHandle::from_raw(raw)
    }
}

fn grant_foreground_from_pipe(pipe: &OwnedHandle) {
    let mut client_pid: u32 = 0;
    // SAFETY: Queries the PID of the UI process that connected to the foreground pipe.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw(), &mut client_pid) } != 0
        && client_pid != 0
    {
        unsafe {
            let _ = AllowSetForegroundWindow(client_pid);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReadyWait {
    Ready,
    TimedOut,
    ChildExited(u32),
}

fn wait_for_ready(ready: HANDLE, child: HANDLE, timeout: Duration) -> Result<ReadyWait, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match unsafe { WaitForSingleObject(ready, 25) } {
            WAIT_OBJECT_0 => return Ok(ReadyWait::Ready),
            WAIT_FAILED => {
                let code = unsafe { GetLastError() };
                return Err(format!("WaitForSingleObject failed (os error {code})"));
            }
            _ => {}
        }
        if unsafe { WaitForSingleObject(child, 0) } == WAIT_OBJECT_0 {
            let mut exit_code: u32 = 0;
            if unsafe { GetExitCodeProcess(child, &mut exit_code) } != 0 && exit_code != 0 {
                return Ok(ReadyWait::ChildExited(exit_code));
            }
            // Exit code 0 means a successful single-instance handover to an already running
            // Loom window; keep waiting for that window to signal `ready`.
        }
        if Instant::now() >= deadline {
            return Ok(ReadyWait::TimedOut);
        }
    }
}

/// Launcher-side activation: spawns Loom with standard privileges, runs the three-event
/// readiness handshake, grants foreground focus to the real UI PID, and confirms display.
pub fn activate(exe: &Path, owner_sid: &str) -> Result<(), String> {
    if launch::current_user_sid()? != owner_sid {
        return Err(
            "current user SID does not match authorized Task Manager replacement owner".into(),
        );
    }

    let nonce = generate_nonce();
    let sd = OwnedSecurityDescriptor::for_owner(owner_sid)?;
    let create_ev = |suffix: &str| -> Result<OwnedHandle, String> {
        let wide = event_name(&nonce, suffix);
        let attrs = sd.attributes();
        // SAFETY: Creates a manual-reset, unsignaled session-local event with owner ACL.
        let raw = unsafe { CreateEventW(&attrs, 1, 0, wide.as_ptr()) };
        let handle =
            OwnedHandle::from_raw(raw).ok_or_else(|| format!("CreateEventW({suffix}) failed"))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(format!("handshake event {suffix} already exists"));
        }
        Ok(handle)
    };

    let ready = create_ev("ready")?;
    let commit = create_ev("commit")?;
    let done = create_ev("done")?;
    let fg_pipe = listen_foreground_pipe(&nonce, &sd);

    let own_created = launch::process_create_time(unsafe { GetCurrentProcess() })?;
    let child = launch::standard_spawn(
        exe,
        &[
            HANDSHAKE_ARG.to_owned(),
            nonce,
            std::process::id().to_string(),
            own_created.to_string(),
        ],
    )?;

    match wait_for_ready(ready.as_raw(), child.as_raw(), Duration::from_secs(10))? {
        ReadyWait::Ready => {}
        ReadyWait::TimedOut => return Err("Loom UI did not signal ready within 10s".into()),
        ReadyWait::ChildExited(code) => {
            return Err(format!("Loom UI exited prematurely with code {code:#010x}"));
        }
    }

    if let Some(ref pipe) = fg_pipe {
        grant_foreground_from_pipe(pipe);
    }

    if unsafe { SetEvent(commit.as_raw()) } == 0 {
        return Err("SetEvent(commit) failed".into());
    }

    if unsafe { WaitForSingleObject(done.as_raw(), 2000) } != WAIT_OBJECT_0 {
        return Err("Loom UI did not acknowledge display completion within 2s".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_handshake_events_and_foreground_pipe_succeed_end_to_end() {
        let sid = launch::current_user_sid().expect("current user SID");
        let sd = OwnedSecurityDescriptor::for_owner(&sid).expect("owner SD");
        let nonce = generate_nonce();
        assert!(valid_nonce(&nonce));

        let create_ev = |suffix: &str| {
            let wide = event_name(&nonce, suffix);
            let attrs = sd.attributes();
            OwnedHandle::from_raw(unsafe { CreateEventW(&attrs, 1, 0, wide.as_ptr()) })
                .expect("CreateEventW")
        };

        let ready = create_ev("ready");
        let commit = create_ev("commit");
        let done = create_ev("done");
        let pipe = listen_foreground_pipe(&nonce, &sd).expect("foreground pipe");

        let own_pid = std::process::id();
        let own_created = launch::process_create_time(unsafe { GetCurrentProcess() }).unwrap();

        let nonce_clone = nonce.clone();
        let worker = std::thread::spawn(move || {
            let args = vec![
                HANDSHAKE_ARG.to_owned(),
                nonce_clone,
                own_pid.to_string(),
                own_created.to_string(),
            ];
            assert!(queue_args(&args));
            let completions = complete_pending();
            assert_eq!(completions.len(), 1);
            let completion = completions.into_iter().next().unwrap();
            assert!(completion.should_show());
            completion.acknowledge();
        });

        assert_eq!(
            unsafe { WaitForSingleObject(ready.as_raw(), 5000) },
            WAIT_OBJECT_0
        );
        let mut client_pid: u32 = 0;
        assert_ne!(
            unsafe { GetNamedPipeClientProcessId(pipe.as_raw(), &mut client_pid) },
            0
        );
        assert_eq!(client_pid, own_pid);
        assert_ne!(unsafe { SetEvent(commit.as_raw()) }, 0);
        assert_eq!(
            unsafe { WaitForSingleObject(done.as_raw(), 5000) },
            WAIT_OBJECT_0
        );
        worker.join().unwrap();
    }
}
