//! Three-event ticket broker (`ready`/`commit`/`done`) for persistent administrator startup
//! and clean elevated <-> standard privilege transitions.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_SUCCESS, FALSE, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
    SE_KERNEL_OBJECT, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSidSubAuthority, GetSidSubAuthorityCount,
    GetTokenInformation, IsTokenRestricted, NO_INHERITANCE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    TokenIntegrityLevel, TokenIsAppContainer,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, GetCurrentProcess, OpenEventW, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
    TerminateProcess, WaitForSingleObject,
};

use crate::handle::{OwnedHandle, to_wide_null};
use crate::taskmgr::launch;
use crate::taskmgr::ready::{HANDSHAKE_ARG, OwnedSecurityDescriptor, generate_nonce, valid_nonce};

use super::{
    FALLBACK_FLAG, STANDARD_BROKER_FLAG, STANDARD_ONCE_FLAG, STARTED_FLAG, canonical_exe,
    is_elevated, load_grant, task,
};

/// Minimum mandatory integrity level required for a caller requesting elevation (`Medium`).
const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;

/// Maximum encoded hex ticket length accepted by the elevated broker.
const MAX_TICKET_LEN: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskMgrHandshake {
    nonce: String,
    pid: u32,
    created: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    nonce: String,
    pid: u32,
    created: u64,
    #[serde(default)]
    restore_route: Option<String>,
    #[serde(default)]
    task_manager: Option<TaskMgrHandshake>,
}

impl Request {
    fn from_args(args: &[String]) -> Result<Self, String> {
        let task_manager = if let Some(at) = args.iter().position(|a| a == HANDSHAKE_ARG) {
            match &args[at + 1..] {
                [nonce, pid, created, ..] if valid_nonce(nonce) => {
                    let pid = pid
                        .parse::<u32>()
                        .map_err(|_| "invalid task manager handshake PID".to_owned())?;
                    let created = created
                        .parse::<u64>()
                        .map_err(|_| "invalid task manager handshake create time".to_owned())?;
                    if pid == 0 || created == 0 {
                        return Err("task manager handshake identity cannot be zero".into());
                    }
                    Some(TaskMgrHandshake {
                        nonce: nonce.clone(),
                        pid,
                        created,
                    })
                }
                _ => return Err("malformed task manager handshake arguments".into()),
            }
        } else {
            None
        };

        let restore_route = extract_restore_route(args);
        if let Some(ref route) = restore_route
            && !valid_route_token(route)
        {
            return Err("invalid restore-route value".into());
        }

        let created = launch::process_create_time(unsafe { GetCurrentProcess() })?;
        Ok(Self {
            nonce: generate_nonce(),
            pid: std::process::id(),
            created,
            restore_route,
            task_manager,
        })
    }

    fn encode(&self) -> Result<String, String> {
        let text = toml::to_string(self).map_err(|e| format!("failed to serialize ticket: {e}"))?;
        let hex: String = text.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        if hex.len() > MAX_TICKET_LEN {
            return Err("encoded elevation ticket exceeds maximum length".into());
        }
        Ok(hex)
    }

    fn decode(token: &str) -> Result<Self, String> {
        if token.is_empty()
            || token.len() > MAX_TICKET_LEN
            || !token.len().is_multiple_of(2)
            || !token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid elevation ticket format".into());
        }
        let mut bytes = Vec::with_capacity(token.len() / 2);
        let (pairs, _) = token.as_bytes().as_chunks::<2>();
        for chunk in pairs {
            let s = std::str::from_utf8(chunk).map_err(|_| "invalid hex byte".to_owned())?;
            let val = u8::from_str_radix(s, 16).map_err(|_| "invalid hex nibble".to_owned())?;
            bytes.push(val);
        }
        let text = String::from_utf8(bytes).map_err(|_| "ticket is not valid UTF-8".to_owned())?;
        let req: Self =
            toml::from_str(&text).map_err(|e| format!("invalid elevation ticket payload: {e}"))?;

        if !valid_nonce(&req.nonce) || req.pid == 0 || req.created == 0 {
            return Err("elevation ticket identity fields are invalid".into());
        }
        if let Some(ref route) = req.restore_route
            && !valid_route_token(route)
        {
            return Err("elevation ticket restore-route is invalid".into());
        }
        if let Some(ref tm) = req.task_manager
            && (!valid_nonce(&tm.nonce) || tm.pid == 0 || tm.created == 0)
        {
            return Err("elevation ticket task manager handshake is invalid".into());
        }
        Ok(req)
    }

    fn child_args(&self, elevated: bool) -> Vec<String> {
        let mut args = Vec::with_capacity(8);
        if !elevated {
            args.push(STANDARD_ONCE_FLAG.to_owned());
        }
        args.push(STARTED_FLAG.to_owned());
        args.push(self.nonce.clone());
        if let Some(ref route) = self.restore_route {
            args.push(format!("--restore-route={route}"));
        }
        if let Some(ref tm) = self.task_manager {
            args.push(HANDSHAKE_ARG.to_owned());
            args.push(tm.nonce.clone());
            args.push(tm.pid.to_string());
            args.push(tm.created.to_string());
        }
        args
    }
}

fn extract_restore_route(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(val) = arg.strip_prefix("--restore-route=") {
            return Some(val.trim_matches('"').to_owned());
        }
        if arg == "--restore-route"
            && let Some(val) = iter.next()
        {
            return Some(val.trim_matches('"').to_owned());
        }
    }
    None
}

fn valid_route_token(route: &str) -> bool {
    !route.is_empty()
        && route.len() <= 32
        && route
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn event_name(nonce: &str, suffix: &str) -> Vec<u16> {
    to_wide_null(&format!("Local\\Loom.Admin.{nonce}.{suffix}"))
}

fn create_owner_event(
    nonce: &str,
    suffix: &str,
    sd: &OwnedSecurityDescriptor,
) -> Result<OwnedHandle, String> {
    let wide = event_name(nonce, suffix);
    let attrs = sd.attributes();
    // SAFETY: Creates a manual-reset, unsignaled session-local event with owner-restricted ACL.
    let raw = unsafe { CreateEventW(&attrs, 1, 0, wide.as_ptr()) };
    let handle = OwnedHandle::from_raw(raw).ok_or_else(|| {
        format!("CreateEventW({suffix}) failed (os error {})", unsafe {
            GetLastError()
        })
    })?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(format!("admin handshake event {suffix} already exists"));
    }
    Ok(handle)
}

fn open_event(nonce: &str, suffix: &str) -> Result<OwnedHandle, String> {
    let wide = event_name(nonce, suffix);
    // SAFETY: Opens existing session-local event created by the caller.
    let raw = unsafe {
        OpenEventW(
            EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            FALSE,
            wide.as_ptr(),
        )
    };
    OwnedHandle::from_raw(raw).ok_or_else(|| {
        format!("OpenEventW({suffix}) failed (os error {})", unsafe {
            GetLastError()
        })
    })
}

/// Returns the Windows session ID of `pid`, rejecting session 0 (non-interactive services).
pub(crate) fn interactive_session_id(pid: u32) -> Result<u32, String> {
    let mut session_id: u32 = 0;
    // SAFETY: Queries interactive session ID for `pid`.
    if unsafe { ProcessIdToSessionId(pid, &mut session_id) } == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("ProcessIdToSessionId failed (os error {code})"));
    }
    if session_id == 0 {
        return Err("non-interactive session 0 is not permitted".into());
    }
    Ok(session_id)
}

/// Grants `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE` on the current (elevated)
/// process handle to `owner_sid` so a de-elevated standard broker can verify our identity and
/// wait for our clean exit before spawning the standard UI.
fn allow_owner_process_query(owner_sid: &str) -> Result<(), String> {
    let wide_sid = to_wide_null(owner_sid);
    let mut psid: *mut c_void = ptr::null_mut();
    // SAFETY: Parses validated user SID string into a binary SID.
    if unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut psid) } == 0 || psid.is_null() {
        let code = unsafe { GetLastError() };
        return Err(format!("ConvertStringSidToSidW failed (os error {code})"));
    }
    struct LocalSid(*mut c_void);
    impl Drop for LocalSid {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }
    let _sid_guard = LocalSid(psid);

    let mut old_dacl: *mut ACL = ptr::null_mut();
    let mut sd: *mut c_void = ptr::null_mut();
    // SAFETY: Reads the current process's DACL; `sd` owns `old_dacl` and is freed with `LocalFree`.
    let rc = unsafe {
        GetSecurityInfo(
            GetCurrentProcess(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut old_dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "GetSecurityInfo(current process) failed (os error {rc})"
        ));
    }
    let _sd_guard = LocalSid(sd);

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: psid.cast::<u16>(),
        },
    };

    let mut new_dacl: *mut ACL = ptr::null_mut();
    // SAFETY: Merges the new allow ACE into a newly allocated ACL.
    let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
    if rc != ERROR_SUCCESS || new_dacl.is_null() {
        return Err(format!("SetEntriesInAclW failed (os error {rc})"));
    }
    let _dacl_guard = LocalSid(new_dacl.cast::<c_void>());

    // SAFETY: Updates current process DACL to include owner query/wait rights.
    let rc = unsafe {
        SetSecurityInfo(
            GetCurrentProcess(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            new_dacl,
            ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "SetSecurityInfo(current process) failed (os error {rc})"
        ));
    }
    Ok(())
}

/// Prepared privilege-switch handoff whose broker is already running and waiting for `commit`.
pub struct PreparedSwitch {
    commit: OwnedHandle,
    _ready: OwnedHandle,
    _done: OwnedHandle,
}

impl PreparedSwitch {
    /// Signals the waiting broker to wait for this process to exit and launch the replacement UI.
    pub fn commit(self) -> Result<(), String> {
        // SAFETY: Signals the manual-reset `commit` event.
        if unsafe { SetEvent(self.commit.as_raw()) } == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!("SetEvent(commit) failed (os error {code})"));
        }
        Ok(())
    }
}

/// Prepares either an elevated broker (via the registered scheduled task) or a standard-user
/// broker (via `standard_spawn` / Explorer parent) and waits up to 15 s for broker `ready`.
pub fn prepare(args: &[String], elevated: bool) -> Result<PreparedSwitch, String> {
    let sid = launch::current_user_sid()?;
    let request = Request::from_args(args)?;
    let sd = OwnedSecurityDescriptor::for_owner(&sid)?;
    let ready = create_owner_event(&request.nonce, "ready", &sd)?;
    let commit = create_owner_event(&request.nonce, "commit", &sd)?;
    let done = create_owner_event(&request.nonce, "done", &sd)?;
    let token = request.encode()?;

    let _child = if elevated {
        let current_exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
        let grant = load_grant(&sid)?
            .ok_or_else(|| "persistent administrator startup is not enabled".to_owned())?;
        if !grant
            .exe_path
            .to_string_lossy()
            .eq_ignore_ascii_case(&current_exe.to_string_lossy())
        {
            return Err(
                "registered administrator task points to a different Loom executable".into(),
            );
        }
        let session_id = interactive_session_id(std::process::id())?;
        let scheduler = task::Scheduler::connect()?;
        scheduler.run(&sid, &current_exe, &token, session_id)?;
        None
    } else {
        let _ = allow_owner_process_query(&sid);
        let current_exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
        let child =
            launch::standard_spawn(&current_exe, &[STANDARD_BROKER_FLAG.to_owned(), token])?;
        Some(child)
    };

    // SAFETY: Waits up to 15 s for the broker to validate our identity and signal `ready`.
    if unsafe { WaitForSingleObject(ready.as_raw(), 15_000) } != WAIT_OBJECT_0 {
        return Err("privilege switch broker did not become ready within 15s".into());
    }

    Ok(PreparedSwitch {
        commit,
        _ready: ready,
        _done: done,
    })
}

fn verify_caller(request: &Request, strict_token_checks: bool) -> Result<OwnedHandle, String> {
    // SAFETY: Opens caller process solely to verify identity and wait for exit.
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            FALSE,
            request.pid,
        )
    };
    let process = OwnedHandle::from_raw(raw).ok_or_else(|| {
        let code = unsafe { GetLastError() };
        format!("OpenProcess(caller) failed (os error {code})")
    })?;

    if launch::process_create_time(process.as_raw())? != request.created {
        return Err("caller process creation time mismatch".into());
    }
    if interactive_session_id(request.pid)? != interactive_session_id(std::process::id())? {
        return Err("caller is in a different Windows session".into());
    }

    if strict_token_checks {
        let mut raw_token: HANDLE = ptr::null_mut();
        // SAFETY: Opens caller primary token for SID, sandbox, and integrity verification.
        if unsafe { OpenProcessToken(process.as_raw(), TOKEN_QUERY, &mut raw_token) } == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!("OpenProcessToken(caller) failed (os error {code})"));
        }
        let token = OwnedHandle::from_raw(raw_token)
            .ok_or_else(|| "OpenProcessToken returned null".to_owned())?;

        if launch::token_user_sid(token.as_raw())? != launch::current_user_sid()? {
            return Err("caller belongs to a different user account".into());
        }

        // Reject AppContainer and restricted tokens.
        let mut is_app_container: u32 = 0;
        let mut size: u32 = 0;
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw(),
                TokenIsAppContainer,
                (&mut is_app_container as *mut u32).cast::<c_void>(),
                size_of::<u32>() as u32,
                &mut size,
            )
        };
        if ok == 0 || is_app_container != 0 || unsafe { IsTokenRestricted(token.as_raw()) } != 0 {
            return Err("sandboxed or restricted caller token is not permitted".into());
        }

        // Require at least Medium mandatory integrity level (`0x2000`).
        let mut buf = [0u64; 64];
        let mut il_size: u32 = 0;
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw(),
                TokenIntegrityLevel,
                buf.as_mut_ptr().cast::<c_void>(),
                std::mem::size_of_val(&buf) as u32,
                &mut il_size,
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "GetTokenInformation(TokenIntegrityLevel) failed (os error {code})"
            ));
        }
        // SAFETY: `buf` holds `TOKEN_MANDATORY_LABEL` populated by `GetTokenInformation`.
        let label = unsafe { &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
        let sub_count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
        if sub_count == 0 {
            return Err("caller integrity SID has no sub-authorities".into());
        }
        let rid = unsafe { *GetSidSubAuthority(label.Label.Sid, u32::from(sub_count) - 1) };
        if rid < SECURITY_MANDATORY_MEDIUM_RID {
            return Err("caller integrity level is below Medium".into());
        }
    }

    Ok(process)
}

/// Runs the short-lived broker process spawned either by Task Scheduler (`elevated = true`)
/// or via `standard_spawn` (`elevated = false`).
pub(crate) fn run_broker(token: &str, elevated: bool) -> Result<(), String> {
    let request = Request::decode(token)?;
    if is_elevated() != elevated {
        return Err("broker elevation state does not match requested mode".into());
    }

    let current_exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
    if elevated {
        let sid = launch::current_user_sid()?;
        let grant = load_grant(&sid)?
            .ok_or_else(|| "persistent administrator grant record is missing".to_owned())?;
        if !grant
            .exe_path
            .to_string_lossy()
            .eq_ignore_ascii_case(&current_exe.to_string_lossy())
        {
            return Err("broker executable does not match authorized grant".into());
        }
    }

    let caller = verify_caller(&request, elevated)?;
    let ready = open_event(&request.nonce, "ready")?;
    let commit = open_event(&request.nonce, "commit")?;
    let done = open_event(&request.nonce, "done")?;

    // Signal `ready` so the caller knows the broker is live and validated.
    if unsafe { SetEvent(ready.as_raw()) } == 0 {
        return Err("SetEvent(ready) failed".into());
    }

    // Wait up to 20 s for caller `commit`, checking `commit` before caller exit so a fast
    // caller that exits right after signaling `commit` is accepted cleanly.
    let commit_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if unsafe { WaitForSingleObject(commit.as_raw(), 25) } == WAIT_OBJECT_0 {
            break;
        }
        if unsafe { WaitForSingleObject(caller.as_raw(), 0) } == WAIT_OBJECT_0
            || Instant::now() >= commit_deadline
        {
            return Err("caller aborted or timed out before committing privilege switch".into());
        }
    }

    // Wait up to 20 s for the caller process to fully exit so single-instance lock and
    // config writers are cleanly released before spawning the replacement UI.
    if unsafe { WaitForSingleObject(caller.as_raw(), 20_000) } != WAIT_OBJECT_0 {
        return Err("caller process did not exit within 20s after commit".into());
    }

    let mut child_args = request.child_args(elevated);
    let child = launch::spawn(&current_exe, &child_args, false)?;

    // Wait up to 60 s for the spawned UI to acknowledge readiness via `done`.
    let ui_deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < ui_deadline {
        if unsafe { WaitForSingleObject(done.as_raw(), 50) } == WAIT_OBJECT_0 {
            return Ok(());
        }
        if unsafe { WaitForSingleObject(child.as_raw(), 0) } == WAIT_OBJECT_0 {
            break;
        }
    }

    // Terminate a hung child before falling back so we never leave two competing UI writers.
    if unsafe { WaitForSingleObject(child.as_raw(), 0) } != WAIT_OBJECT_0 {
        unsafe {
            let _ = TerminateProcess(child.as_raw(), 1460);
            let _ = WaitForSingleObject(child.as_raw(), 10_000);
        }
    }

    if elevated {
        child_args.insert(0, FALLBACK_FLAG.to_owned());
        let _ = spawn_standard_fallback(&current_exe, &child_args);
    }
    Err("spawned UI did not acknowledge readiness within 60s".into())
}

fn spawn_standard_fallback(exe: &Path, args: &[String]) -> Result<(), String> {
    let _child = launch::standard_spawn(exe, args)?;
    Ok(())
}

/// Signals the broker's `done` event once the UI window is created and live.
pub fn acknowledge(args: &[String]) {
    if let Some(at) = args.iter().position(|a| a == STARTED_FLAG)
        && let Some(nonce) = args.get(at + 1).filter(|s| valid_nonce(s))
        && let Ok(done) = open_event(nonce, "done")
    {
        // SAFETY: Signals the manual-reset `done` event created by the launcher.
        unsafe {
            let _ = SetEvent(done.as_raw());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_round_trips_fixed_intents_and_rejects_malformed_tokens() {
        let tm_nonce = generate_nonce();
        let args = vec![
            "loom-app.exe".to_owned(),
            "--restore-route=settings".to_owned(),
            HANDSHAKE_ARG.to_owned(),
            tm_nonce.clone(),
            "4321".to_owned(),
            "987654321".to_owned(),
        ];
        let req = Request::from_args(&args).expect("build request");
        let encoded = req.encode().expect("encode request");
        let decoded = Request::decode(&encoded).expect("decode request");
        assert_eq!(decoded, req);
        assert_eq!(decoded.restore_route.as_deref(), Some("settings"));
        assert_eq!(
            decoded.task_manager,
            Some(TaskMgrHandshake {
                nonce: tm_nonce.clone(),
                pid: 4321,
                created: 987654321,
            })
        );

        let elevated_args = decoded.child_args(true);
        assert_eq!(elevated_args[0], STARTED_FLAG);
        assert_eq!(elevated_args[2], "--restore-route=settings");
        assert_eq!(elevated_args[3], HANDSHAKE_ARG);
        assert_eq!(elevated_args[4], tm_nonce);

        let standard_args = decoded.child_args(false);
        assert_eq!(standard_args[0], STANDARD_ONCE_FLAG);
        assert_eq!(standard_args[1], STARTED_FLAG);

        for invalid in [
            "",
            "a",
            "zz",
            "00 00",
            "$(Arg0)",
            "--loom-admin-run",
            &"a".repeat(MAX_TICKET_LEN + 2),
        ] {
            assert!(Request::decode(invalid).is_err(), "should reject {invalid}");
        }
    }

    #[test]
    fn ticket_rejects_invalid_route_and_malformed_taskmgr_handshake() {
        assert!(Request::from_args(&["--restore-route=settings --evil".to_owned()]).is_err());
        assert!(Request::from_args(&[HANDSHAKE_ARG.to_owned(), "bad".to_owned()]).is_err());
    }
}
