//! Raw Task Scheduler 2.0 COM client built directly on `windows-sys`.
//!
//! Avoids pulling in the full `windows` crate while preserving exact COM ABI
//! compatibility with `ITaskService`, `ITaskFolder`, `IRegisteredTask`,
//! `ITaskDefinition`, `IPrincipal`, `IActionCollection`, `IAction`, and `IExecAction`.

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, SysAllocStringLen, SysFreeString, SysStringLen, VARIANT_BOOL,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoInitializeSecurity, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows_sys::Win32::System::Variant::{VARIANT, VT_BSTR, VT_I4, VariantClear, VariantInit};
use windows_sys::core::{BSTR, GUID, HRESULT};

use crate::handle::{LocalAllocGuard, to_wide_null};

use super::RUN_FLAG;

/// `RPC_E_CHANGED_MODE` (`0x80010106`).
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
/// `RPC_E_TOO_LATE` (`0x80010119`).
const RPC_E_TOO_LATE: i32 = 0x8001_0119u32 as i32;

const TASK_CREATE_OR_UPDATE: i32 = 6;
const TASK_DONT_ADD_PRINCIPAL_ACE: i32 = 16;
const TASK_LOGON_INTERACTIVE_TOKEN: i32 = 3;
const TASK_RUNLEVEL_HIGHEST: i32 = 1;
const TASK_RUN_USE_SESSION_ID: i32 = 4;
const TASK_ENUM_HIDDEN: i32 = 1;

const CLSID_TASK_SCHEDULER: GUID = GUID::from_u128(0x0f87369f_a4e5_4cfc_bd3e_73e6154572dd);
const IID_ITASK_SERVICE: GUID = GUID::from_u128(0x2faba4c7_4da9_4013_9697_20cc3fd40f85);
const IID_IEXEC_ACTION: GUID = GUID::from_u128(0x4c3d624d_fd6b_49a3_b9b7_09cb3cd3f047);

const TASK_NAME_PREFIX: &str = "Loom.Admin.";

#[repr(C)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct IDispatchVtbl {
    base: IUnknownVtbl,
    _get_type_info_count: usize,
    _get_type_info: usize,
    _get_ids_of_names: usize,
    _invoke: usize,
}

#[repr(C)]
struct ITaskServiceVtbl {
    base: IDispatchVtbl,
    get_folder: unsafe extern "system" fn(*mut c_void, BSTR, *mut *mut c_void) -> HRESULT,
    _get_running_tasks: usize,
    new_task: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    connect: unsafe extern "system" fn(*mut c_void, VARIANT, VARIANT, VARIANT, VARIANT) -> HRESULT,
    _connected: usize,
    _target_server: usize,
    _connected_user: usize,
    _connected_domain: usize,
    _highest_version: usize,
}

#[repr(C)]
struct ITaskFolderVtbl {
    base: IDispatchVtbl,
    _name: usize,
    _path: usize,
    _get_folder: usize,
    _get_folders: usize,
    _create_folder: usize,
    _delete_folder: usize,
    get_task: unsafe extern "system" fn(*mut c_void, BSTR, *mut *mut c_void) -> HRESULT,
    get_tasks: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT,
    delete_task: unsafe extern "system" fn(*mut c_void, BSTR, i32) -> HRESULT,
    register_task: unsafe extern "system" fn(
        *mut c_void,
        BSTR,
        BSTR,
        i32,
        VARIANT,
        VARIANT,
        i32,
        VARIANT,
        *mut *mut c_void,
    ) -> HRESULT,
    _register_task_definition: usize,
    _get_security_descriptor: usize,
    _set_security_descriptor: usize,
}

#[repr(C)]
struct IRegisteredTaskCollectionVtbl {
    base: IDispatchVtbl,
    count: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    get_item: unsafe extern "system" fn(*mut c_void, VARIANT, *mut *mut c_void) -> HRESULT,
    _new_enum: usize,
}

#[repr(C)]
struct IRegisteredTaskVtbl {
    base: IDispatchVtbl,
    name: unsafe extern "system" fn(*mut c_void, *mut BSTR) -> HRESULT,
    _path: usize,
    _state: usize,
    enabled: unsafe extern "system" fn(*mut c_void, *mut VARIANT_BOOL) -> HRESULT,
    _set_enabled: usize,
    _run: usize,
    run_ex: unsafe extern "system" fn(
        *mut c_void,
        VARIANT,
        i32,
        i32,
        BSTR,
        *mut *mut c_void,
    ) -> HRESULT,
    _get_instances: usize,
    _last_run_time: usize,
    _last_task_result: usize,
    _number_of_missed_runs: usize,
    _next_run_time: usize,
    definition: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    _xml: usize,
    _get_security_descriptor: usize,
    _set_security_descriptor: usize,
}

#[repr(C)]
struct ITaskDefinitionVtbl {
    base: IDispatchVtbl,
    _registration_info: usize,
    _set_registration_info: usize,
    _triggers: usize,
    _set_triggers: usize,
    _settings: usize,
    _set_settings: usize,
    _data: usize,
    _set_data: usize,
    principal: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    _set_principal: usize,
    actions: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    _set_actions: usize,
    _xml_text: usize,
    set_xml_text: unsafe extern "system" fn(*mut c_void, BSTR) -> HRESULT,
}

#[repr(C)]
struct IPrincipalVtbl {
    base: IDispatchVtbl,
    _id: usize,
    _set_id: usize,
    _display_name: usize,
    _set_display_name: usize,
    user_id: unsafe extern "system" fn(*mut c_void, *mut BSTR) -> HRESULT,
    _set_user_id: usize,
    logon_type: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    _set_logon_type: usize,
    _group_id: usize,
    _set_group_id: usize,
    run_level: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    _set_run_level: usize,
}

#[repr(C)]
struct IActionCollectionVtbl {
    base: IDispatchVtbl,
    count: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    get_item: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> HRESULT,
    _new_enum: usize,
    _xml_text: usize,
    _set_xml_text: usize,
    _create: usize,
    _remove: usize,
    _clear: usize,
    _context: usize,
    _set_context: usize,
}

#[repr(C)]
struct IActionVtbl {
    base: IDispatchVtbl,
    _id: usize,
    _set_id: usize,
    _action_type: usize,
}

#[repr(C)]
struct IExecActionVtbl {
    base: IActionVtbl,
    path: unsafe extern "system" fn(*mut c_void, *mut BSTR) -> HRESULT,
    _set_path: usize,
    arguments: unsafe extern "system" fn(*mut c_void, *mut BSTR) -> HRESULT,
    _set_arguments: usize,
    _working_directory: usize,
    _set_working_directory: usize,
}

/// RAII COM interface pointer calling `IUnknown::Release` on drop.
struct ComPtr<V> {
    raw: *mut c_void,
    _marker: std::marker::PhantomData<*const V>,
}

impl<V> ComPtr<V> {
    unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self {
                raw,
                _marker: std::marker::PhantomData,
            })
        }
    }

    #[inline]
    fn as_raw(&self) -> *mut c_void {
        self.raw
    }

    #[inline]
    unsafe fn vtbl(&self) -> &V {
        unsafe { &**(self.raw as *const *const V) }
    }

    unsafe fn query_interface<U>(&self, iid: &GUID) -> Result<ComPtr<U>, HRESULT> {
        let mut out: *mut c_void = ptr::null_mut();
        let unk = unsafe { &**(self.raw as *const *const IUnknownVtbl) };
        let hr = unsafe { (unk.query_interface)(self.raw, iid, &mut out) };
        if hr < 0 {
            return Err(hr);
        }
        unsafe { ComPtr::from_raw(out) }.ok_or(0x8000_4002u32 as i32)
    }
}

impl<V> Drop for ComPtr<V> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                let unk = &**(self.raw as *const *const IUnknownVtbl);
                (unk.release)(self.raw);
            }
        }
    }
}

/// RAII wrapper for a Win32 `BSTR`.
struct OwnedBstr(BSTR);

impl OwnedBstr {
    fn from_str(text: &str) -> Self {
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: Allocates a COM BSTR of exact UTF-16 code-unit length.
        let raw = unsafe { SysAllocStringLen(wide.as_ptr(), wide.len() as u32) };
        Self(raw)
    }

    fn empty() -> Self {
        Self(ptr::null_mut())
    }

    #[inline]
    fn raw(&self) -> BSTR {
        self.0
    }

    fn into_string(self) -> String {
        if self.0.is_null() {
            return String::new();
        }
        // SAFETY: `self.0` is a valid BSTR allocated by COM/oleaut32.
        let len = unsafe { SysStringLen(self.0) } as usize;
        let slice = unsafe { std::slice::from_raw_parts(self.0, len) };
        String::from_utf16_lossy(slice)
    }
}

impl Drop for OwnedBstr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Frees BSTR allocated by `SysAllocStringLen` or returned by COM out-param.
            unsafe {
                SysFreeString(self.0);
            }
        }
    }
}

/// RAII wrapper for a Win32 `VARIANT`.
struct OwnedVariant(VARIANT);

impl OwnedVariant {
    fn empty() -> Self {
        let mut v = VARIANT::default();
        // SAFETY: Initializes `v` as `VT_EMPTY`.
        unsafe {
            VariantInit(&mut v);
        }
        Self(v)
    }

    fn from_str(text: &str) -> Self {
        let mut v = Self::empty();
        let wide: Vec<u16> = text.encode_utf16().collect();
        let bstr = unsafe { SysAllocStringLen(wide.as_ptr(), wide.len() as u32) };
        v.0.Anonymous.Anonymous.vt = VT_BSTR;
        v.0.Anonymous.Anonymous.Anonymous.bstrVal = bstr;
        v
    }

    fn from_i32(value: i32) -> Self {
        let mut v = Self::empty();
        v.0.Anonymous.Anonymous.vt = VT_I4;
        v.0.Anonymous.Anonymous.Anonymous.lVal = value;
        v
    }

    /// Returns a bitwise `VARIANT` value to pass by value into `[in]` COM methods while
    /// keeping `self` alive until the call completes so the inner `BSTR` is not freed early.
    #[inline]
    fn as_in(&self) -> VARIANT {
        self.0
    }
}

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        // SAFETY: Releases any owned BSTR or COM value inside `self.0`.
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

struct ComApartment(bool);

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: Paired with a successful `CoInitializeEx` on this thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Returns the deterministic Task Scheduler task name scoped to `sid`.
pub fn task_name(sid: &str) -> String {
    format!("{TASK_NAME_PREFIX}{sid}")
}

/// Extracts the owner SID if `name` is a Loom persistent-admin task name.
pub fn parse_task_owner(name: &str) -> Option<&str> {
    let sid = name.strip_prefix(TASK_NAME_PREFIX)?;
    if valid_user_sid(sid) { Some(sid) } else { None }
}

/// Validates that `sid` is a well-formed Windows user account SID (`S-1-5-21-...` or `S-1-...`).
pub fn valid_user_sid(sid: &str) -> bool {
    if !sid.starts_with("S-1-") || sid.len() > 128 {
        return false;
    }
    // Reject well-known built-in machine accounts (`S-1-5-18`, `S-1-5-32-544`, etc.).
    let parts: Vec<&str> = sid.split('-').collect();
    if parts.len() < 6 || parts[0] != "S" || parts[1] != "1" || parts[2] != "5" {
        return false;
    }
    parts[3..]
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Builds the Task Scheduler 1.3 XML definition for `sid` launching `image`.
pub fn build_task_xml(sid: &str, image: &Path) -> String {
    let image_str = xml_escape(&image.to_string_lossy());
    let dir_str = xml_escape(
        &image
            .parent()
            .unwrap_or_else(|| Path::new(r"C:\"))
            .to_string_lossy(),
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<Principals><Principal id="Owner"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>false</AllowHardTerminate><StartWhenAvailable>false</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>4</Priority></Settings>
<Actions Context="Owner"><Exec><Command>{image_str}</Command><Arguments>{RUN_FLAG} $(Arg0)</Arguments><WorkingDirectory>{dir_str}</WorkingDirectory></Exec></Actions></Task>"#
    )
}

fn hresult_is_not_found(hr: HRESULT) -> bool {
    let code = hr as u32;
    code == 0x8007_0002 || code == ERROR_FILE_NOT_FOUND
}

fn fmt_hr(stage: &str, hr: HRESULT) -> String {
    format!("{stage} failed (HRESULT 0x{:08x})", hr as u32)
}

/// Resolves either a canonical SID string or a Windows `DOMAIN\User` principal name to a SID.
fn resolve_account_sid(principal: &str) -> Result<String, String> {
    let trimmed = principal.trim();
    if trimmed.is_empty() {
        return Err("empty task principal".into());
    }
    if trimmed.starts_with("S-1-") {
        let wide = to_wide_null(trimmed);
        let mut psid: *mut c_void = ptr::null_mut();
        // SAFETY: Validates that `trimmed` is a syntactically valid Windows SID string.
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) } != 0 && !psid.is_null() {
            let _psid_guard = LocalAllocGuard::new(psid);
            return Ok(trimmed.to_owned());
        }
    }

    let wide = to_wide_null(trimmed);
    let mut sid_size: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_kind: SID_NAME_USE = 0;
    // SAFETY: Queries required buffer sizes for `LookupAccountNameW`.
    unsafe {
        LookupAccountNameW(
            ptr::null(),
            wide.as_ptr(),
            ptr::null_mut(),
            &mut sid_size,
            ptr::null_mut(),
            &mut domain_len,
            &mut use_kind,
        );
    }
    if sid_size == 0 || sid_size > 1024 {
        return Err("LookupAccountNameW failed for task principal".into());
    }
    let mut sid_buf = vec![0u8; sid_size as usize];
    let mut domain_buf = vec![0u16; domain_len.max(1) as usize];
    // SAFETY: Buffers match queried sizes.
    let ok = unsafe {
        LookupAccountNameW(
            ptr::null(),
            wide.as_ptr(),
            sid_buf.as_mut_ptr().cast::<c_void>(),
            &mut sid_size,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut use_kind,
        )
    };
    if ok == 0 {
        return Err("LookupAccountNameW resolution failed".into());
    }
    let mut sid_str: *mut u16 = ptr::null_mut();
    // SAFETY: `sid_buf` contains a valid binary SID populated by `LookupAccountNameW`.
    if unsafe { ConvertSidToStringSidW(sid_buf.as_mut_ptr().cast::<c_void>(), &mut sid_str) } == 0
        || sid_str.is_null()
    {
        return Err("ConvertSidToStringSidW failed".into());
    }
    let _sid_guard = LocalAllocGuard::new(sid_str.cast::<c_void>());
    let resolved = unsafe {
        let mut len = 0usize;
        while *sid_str.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, len))
    };
    Ok(resolved)
}

/// Connected Windows Task Scheduler root folder session.
pub(crate) struct Scheduler {
    folder: ComPtr<ITaskFolderVtbl>,
    _service: ComPtr<ITaskServiceVtbl>,
    _apartment: ComApartment,
}

impl Scheduler {
    pub fn connect() -> Result<Self, String> {
        // SAFETY: Initializes COM MTA (tolerating threads already initialized as STA by UI/Shell).
        let init_hr = unsafe { CoInitializeEx(ptr::null(), COINIT_MULTITHREADED as u32) };
        if init_hr < 0 && init_hr != RPC_E_CHANGED_MODE {
            return Err(fmt_hr("CoInitializeEx", init_hr));
        }
        let apartment = ComApartment(init_hr >= 0);

        // Task Scheduler may impersonate the caller during registration. Tolerate `RPC_E_TOO_LATE`
        // when COM process security was already configured earlier in the process lifetime.
        let sec_hr = unsafe {
            CoInitializeSecurity(
                ptr::null_mut(),
                -1,
                ptr::null(),
                ptr::null(),
                RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                ptr::null(),
                EOAC_NONE as u32,
                ptr::null(),
            )
        };
        if sec_hr < 0 && sec_hr != RPC_E_TOO_LATE {
            return Err(fmt_hr("CoInitializeSecurity", sec_hr));
        }

        let mut raw_service: *mut c_void = ptr::null_mut();
        // SAFETY: Creates in-proc TaskScheduler COM object requesting `ITaskService`.
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_TASK_SCHEDULER,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASK_SERVICE,
                &mut raw_service,
            )
        };
        if hr < 0 {
            return Err(fmt_hr("CoCreateInstance(TaskScheduler)", hr));
        }
        let service: ComPtr<ITaskServiceVtbl> = unsafe { ComPtr::from_raw(raw_service) }
            .ok_or_else(|| "CoCreateInstance returned null ITaskService".to_owned())?;

        let empty = OwnedVariant::empty();
        // SAFETY: Connects to local Task Scheduler service with four empty `VARIANT`s.
        let hr = unsafe {
            (service.vtbl().connect)(
                service.as_raw(),
                empty.as_in(),
                empty.as_in(),
                empty.as_in(),
                empty.as_in(),
            )
        };
        if hr < 0 {
            return Err(fmt_hr("ITaskService::Connect", hr));
        }

        let root_path = OwnedBstr::from_str("\\");
        let mut raw_folder: *mut c_void = ptr::null_mut();
        // SAFETY: Retrieves root `ITaskFolder` (`\`).
        let hr = unsafe {
            (service.vtbl().get_folder)(service.as_raw(), root_path.raw(), &mut raw_folder)
        };
        if hr < 0 {
            return Err(fmt_hr("ITaskService::GetFolder", hr));
        }
        let folder: ComPtr<ITaskFolderVtbl> = unsafe { ComPtr::from_raw(raw_folder) }
            .ok_or_else(|| "ITaskService::GetFolder returned null".to_owned())?;

        Ok(Self {
            folder,
            _service: service,
            _apartment: apartment,
        })
    }

    fn get_task(&self, sid: &str) -> Result<ComPtr<IRegisteredTaskVtbl>, HRESULT> {
        let name = OwnedBstr::from_str(&task_name(sid));
        let mut raw_task: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            (self.folder.vtbl().get_task)(self.folder.as_raw(), name.raw(), &mut raw_task)
        };
        if hr < 0 {
            return Err(hr);
        }
        unsafe { ComPtr::from_raw(raw_task) }.ok_or(0x8007_0002u32 as i32)
    }

    pub fn exists(&self, sid: &str) -> Result<bool, String> {
        match self.get_task(sid) {
            Ok(_) => Ok(true),
            Err(hr) if hresult_is_not_found(hr) => Ok(false),
            Err(hr) => Err(fmt_hr("ITaskFolder::GetTask", hr)),
        }
    }

    pub fn register(&self, sid: &str, image: &Path) -> Result<(), String> {
        let xml = build_task_xml(sid, image);
        let first = self.register_xml(sid, &xml);
        if let Err(first_hr) = first {
            // An orphaned TaskCache Tree entry without its backing XML file reports
            // `ERROR_FILE_NOT_FOUND` on lookup yet rejects `RegisterTask` until deleted.
            if matches!(self.get_task(sid), Err(hr) if hresult_is_not_found(hr)) {
                let _ = self.remove(sid);
                return self
                    .register_xml(sid, &xml)
                    .map_err(|hr| fmt_hr("ITaskFolder::RegisterTask(retry)", hr));
            }
            return Err(fmt_hr("ITaskFolder::RegisterTask", first_hr));
        }
        Ok(())
    }

    fn register_xml(&self, sid: &str, xml: &str) -> Result<(), HRESULT> {
        let acl = format!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;{sid})");
        let name = OwnedBstr::from_str(&task_name(sid));
        let xml_bstr = OwnedBstr::from_str(xml);
        let user = OwnedVariant::from_str(sid);
        let password = OwnedVariant::empty();
        let sddl = OwnedVariant::from_str(&acl);
        let mut raw_task: *mut c_void = ptr::null_mut();

        let hr = unsafe {
            (self.folder.vtbl().register_task)(
                self.folder.as_raw(),
                name.raw(),
                xml_bstr.raw(),
                TASK_CREATE_OR_UPDATE | TASK_DONT_ADD_PRINCIPAL_ACE,
                user.as_in(),
                password.as_in(),
                TASK_LOGON_INTERACTIVE_TOKEN,
                sddl.as_in(),
                &mut raw_task,
            )
        };
        if hr < 0 {
            return Err(hr);
        }
        let _registered: Option<ComPtr<IRegisteredTaskVtbl>> =
            unsafe { ComPtr::from_raw(raw_task) };
        Ok(())
    }

    pub fn remove(&self, sid: &str) -> Result<(), String> {
        let name = OwnedBstr::from_str(&task_name(sid));
        let hr = unsafe { (self.folder.vtbl().delete_task)(self.folder.as_raw(), name.raw(), 0) };
        if hr < 0 && !hresult_is_not_found(hr) {
            return Err(fmt_hr("ITaskFolder::DeleteTask", hr));
        }
        Ok(())
    }

    pub fn remove_all(&self) -> Result<(), String> {
        let mut raw_coll: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            (self.folder.vtbl().get_tasks)(self.folder.as_raw(), TASK_ENUM_HIDDEN, &mut raw_coll)
        };
        if hr < 0 {
            return Err(fmt_hr("ITaskFolder::GetTasks", hr));
        }
        let coll: ComPtr<IRegisteredTaskCollectionVtbl> = unsafe { ComPtr::from_raw(raw_coll) }
            .ok_or_else(|| "ITaskFolder::GetTasks returned null".to_owned())?;
        let mut count: i32 = 0;
        let hr = unsafe { (coll.vtbl().count)(coll.as_raw(), &mut count) };
        if hr < 0 {
            return Err(fmt_hr("IRegisteredTaskCollection::Count", hr));
        }
        let mut sids = Vec::new();
        for index in 1..=count {
            let idx = OwnedVariant::from_i32(index);
            let mut raw_task: *mut c_void = ptr::null_mut();
            let hr = unsafe { (coll.vtbl().get_item)(coll.as_raw(), idx.as_in(), &mut raw_task) };
            if hr < 0 {
                continue;
            }
            let Some(task) = (unsafe { ComPtr::<IRegisteredTaskVtbl>::from_raw(raw_task) }) else {
                continue;
            };
            let mut raw_name: BSTR = ptr::null_mut();
            if unsafe { (task.vtbl().name)(task.as_raw(), &mut raw_name) } >= 0 {
                let name = OwnedBstr(raw_name).into_string();
                if let Some(owner) = parse_task_owner(&name) {
                    sids.push(owner.to_owned());
                }
            }
        }
        for sid in sids {
            self.remove(&sid)?;
        }
        Ok(())
    }

    pub fn validate(&self, sid: &str, image: &Path) -> Result<(), String> {
        let task = self
            .get_task(sid)
            .map_err(|hr| fmt_hr("ITaskFolder::GetTask", hr))?;
        validate_registered_task(&task, sid, image)
    }

    pub fn run(
        &self,
        sid: &str,
        image: &Path,
        ticket: &str,
        session_id: u32,
    ) -> Result<(), String> {
        let task = self
            .get_task(sid)
            .map_err(|hr| fmt_hr("ITaskFolder::GetTask", hr))?;
        validate_registered_task(&task, sid, image)?;

        let params = OwnedVariant::from_str(ticket);
        let user = OwnedBstr::empty();
        let mut raw_running: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            (task.vtbl().run_ex)(
                task.as_raw(),
                params.as_in(),
                TASK_RUN_USE_SESSION_ID,
                session_id as i32,
                user.raw(),
                &mut raw_running,
            )
        };
        if hr < 0 {
            return Err(fmt_hr("IRegisteredTask::RunEx", hr));
        }
        // Release the returned `IRunningTask` COM interface immediately.
        let _running: Option<ComPtr<IUnknownVtbl>> = unsafe { ComPtr::from_raw(raw_running) };
        Ok(())
    }
}

fn validate_registered_task(
    task: &ComPtr<IRegisteredTaskVtbl>,
    expected_sid: &str,
    expected_image: &Path,
) -> Result<(), String> {
    let mut enabled_raw: VARIANT_BOOL = 0;
    let hr = unsafe { (task.vtbl().enabled)(task.as_raw(), &mut enabled_raw) };
    if hr < 0 {
        return Err(fmt_hr("IRegisteredTask::Enabled", hr));
    }
    let mut raw_def: *mut c_void = ptr::null_mut();
    let hr = unsafe { (task.vtbl().definition)(task.as_raw(), &mut raw_def) };
    if hr < 0 {
        return Err(fmt_hr("IRegisteredTask::Definition", hr));
    }
    let def: ComPtr<ITaskDefinitionVtbl> = unsafe { ComPtr::from_raw(raw_def) }
        .ok_or_else(|| "IRegisteredTask::Definition returned null".to_owned())?;
    validate_definition(&def, enabled_raw != 0, expected_sid, expected_image)
}

fn validate_definition(
    def: &ComPtr<ITaskDefinitionVtbl>,
    enabled: bool,
    expected_sid: &str,
    expected_image: &Path,
) -> Result<(), String> {
    if !enabled {
        return Err("scheduled task is disabled".into());
    }

    let mut raw_principal: *mut c_void = ptr::null_mut();
    let hr = unsafe { (def.vtbl().principal)(def.as_raw(), &mut raw_principal) };
    if hr < 0 {
        return Err(fmt_hr("ITaskDefinition::Principal", hr));
    }
    let principal: ComPtr<IPrincipalVtbl> = unsafe { ComPtr::from_raw(raw_principal) }
        .ok_or_else(|| "ITaskDefinition::Principal returned null".to_owned())?;

    let mut raw_user: BSTR = ptr::null_mut();
    let hr = unsafe { (principal.vtbl().user_id)(principal.as_raw(), &mut raw_user) };
    if hr < 0 {
        return Err(fmt_hr("IPrincipal::UserId", hr));
    }
    let user = OwnedBstr(raw_user).into_string();
    let owner_sid = resolve_account_sid(&user)?;
    if owner_sid != expected_sid {
        return Err("scheduled task principal does not match current account".into());
    }

    let mut logon: i32 = 0;
    let hr = unsafe { (principal.vtbl().logon_type)(principal.as_raw(), &mut logon) };
    if hr < 0 {
        return Err(fmt_hr("IPrincipal::LogonType", hr));
    }
    if logon != TASK_LOGON_INTERACTIVE_TOKEN {
        return Err("scheduled task logon type is not InteractiveToken".into());
    }

    let mut run_level: i32 = 0;
    let hr = unsafe { (principal.vtbl().run_level)(principal.as_raw(), &mut run_level) };
    if hr < 0 {
        return Err(fmt_hr("IPrincipal::RunLevel", hr));
    }
    if run_level != TASK_RUNLEVEL_HIGHEST {
        return Err("scheduled task run level is not HighestAvailable".into());
    }

    let mut raw_actions: *mut c_void = ptr::null_mut();
    let hr = unsafe { (def.vtbl().actions)(def.as_raw(), &mut raw_actions) };
    if hr < 0 {
        return Err(fmt_hr("ITaskDefinition::Actions", hr));
    }
    let actions: ComPtr<IActionCollectionVtbl> = unsafe { ComPtr::from_raw(raw_actions) }
        .ok_or_else(|| "ITaskDefinition::Actions returned null".to_owned())?;

    let mut count: i32 = 0;
    let hr = unsafe { (actions.vtbl().count)(actions.as_raw(), &mut count) };
    if hr < 0 {
        return Err(fmt_hr("IActionCollection::Count", hr));
    }
    if count != 1 {
        return Err(format!("scheduled task expected 1 action, found {count}"));
    }

    let mut raw_action: *mut c_void = ptr::null_mut();
    let hr = unsafe { (actions.vtbl().get_item)(actions.as_raw(), 1, &mut raw_action) };
    if hr < 0 {
        return Err(fmt_hr("IActionCollection::get_Item", hr));
    }
    let action: ComPtr<IActionVtbl> = unsafe { ComPtr::from_raw(raw_action) }
        .ok_or_else(|| "IActionCollection::get_Item returned null".to_owned())?;

    let exec: ComPtr<IExecActionVtbl> = unsafe { action.query_interface(&IID_IEXEC_ACTION) }
        .map_err(|hr| fmt_hr("IAction::QueryInterface(IExecAction)", hr))?;

    let mut raw_path: BSTR = ptr::null_mut();
    let hr = unsafe { (exec.vtbl().path)(exec.as_raw(), &mut raw_path) };
    if hr < 0 {
        return Err(fmt_hr("IExecAction::Path", hr));
    }
    let action_path = OwnedBstr(raw_path).into_string();
    let expected_str = expected_image.to_string_lossy();
    if !action_path.eq_ignore_ascii_case(&expected_str) {
        return Err("scheduled task executable path does not match current Loom binary".into());
    }

    let mut raw_args: BSTR = ptr::null_mut();
    let hr = unsafe { (exec.vtbl().arguments)(exec.as_raw(), &mut raw_args) };
    if hr < 0 {
        return Err(fmt_hr("IExecAction::Arguments", hr));
    }
    let action_args = OwnedBstr(raw_args).into_string();
    let expected_args = format!("{RUN_FLAG} $(Arg0)");
    if action_args != expected_args {
        return Err("scheduled task arguments do not match Loom broker signature".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_name_and_sid_validation_scope_strictly_to_user_accounts() {
        let sid = "S-1-5-21-1000-2000-3000-1001";
        assert_eq!(parse_task_owner(&task_name(sid)), Some(sid));
        for invalid in [
            "Loom.Admin.S-1-5-18",
            "Loom.Admin.S-1-5-32-544",
            "Loom.Admin.not-a-sid",
            "Other.Admin.S-1-5-21-1-2-3-1001",
            "Loom.Admin.S-1-5-21-1-2-3-1001-extra",
        ] {
            assert_eq!(parse_task_owner(invalid), None, "should reject {invalid}");
        }
    }

    #[test]
    fn in_memory_task_xml_parses_and_validates_through_real_task_scheduler_com() {
        // Instantiates TaskScheduler COM in-memory via `ITaskService::NewTask` + `SetXmlText`
        // without connecting to the daemon, registering a task, or requiring administrator rights.
        let init_hr = unsafe { CoInitializeEx(ptr::null(), COINIT_MULTITHREADED as u32) };
        assert!(init_hr >= 0 || init_hr == RPC_E_CHANGED_MODE);
        let _apt = ComApartment(init_hr >= 0);

        let mut raw_service: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_TASK_SCHEDULER,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASK_SERVICE,
                &mut raw_service,
            )
        };
        assert!(hr >= 0, "CoCreateInstance failed: 0x{:08x}", hr as u32);
        let service: ComPtr<ITaskServiceVtbl> = unsafe { ComPtr::from_raw(raw_service) }.unwrap();

        let mut raw_def: *mut c_void = ptr::null_mut();
        let hr = unsafe { (service.vtbl().new_task)(service.as_raw(), 0, &mut raw_def) };
        assert!(hr >= 0, "ITaskService::NewTask failed: 0x{:08x}", hr as u32);
        let def: ComPtr<ITaskDefinitionVtbl> = unsafe { ComPtr::from_raw(raw_def) }.unwrap();

        let sid = "S-1-5-21-1-2-3-1001";
        let image = Path::new(r"C:\Program Files\Loom & Unicode 目录\loom-app.exe");
        let xml = build_task_xml(sid, image);
        let xml_bstr = OwnedBstr::from_str(&xml);
        let hr = unsafe { (def.vtbl().set_xml_text)(def.as_raw(), xml_bstr.raw()) };
        assert!(
            hr >= 0,
            "ITaskDefinition::SetXmlText failed: 0x{:08x}",
            hr as u32
        );

        // Exact match + case-insensitive Windows path match succeed.
        assert_eq!(validate_definition(&def, true, sid, image), Ok(()));
        assert_eq!(
            validate_definition(
                &def,
                true,
                sid,
                Path::new(&image.to_string_lossy().to_uppercase()),
            ),
            Ok(())
        );
        // Disabled, wrong SID, or wrong binary path are rejected cleanly.
        assert!(validate_definition(&def, false, sid, image).is_err());
        assert!(validate_definition(&def, true, "S-1-5-21-1-2-3-1002", image).is_err());
        assert!(validate_definition(&def, true, sid, Path::new(r"C:\Other\loom-app.exe")).is_err());
    }
}
