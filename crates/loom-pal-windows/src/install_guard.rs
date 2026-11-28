//! Windows installation-path ACL verification for high-privilege opt-in features.
//!
//! Ensures persistent administrator startup (`Loom.Admin.<SID>`) and IFEO Task Manager
//! replacement (`taskmgr.exe`) can only be registered for executables located in an
//! administrator-protected local directory (such as `C:\Program Files\Loom`), preventing
//! local privilege escalation when running portable builds from user-writable folders
//! like `Downloads` or `Desktop`.

use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    GetAce, GetAclInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
    FILE_WRITE_EA, GetDriveTypeW, WRITE_DAC, WRITE_OWNER,
};

use crate::handle::to_wide_null;

/// `ACCESS_ALLOWED_ACE_TYPE` (`0x00`).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// `INHERIT_ONLY_ACE` (`0x08`): template ACEs that apply only to newly created children.
const INHERIT_ONLY_ACE: u8 = 0x08;
/// `DRIVE_FIXED` (`3`).
const DRIVE_FIXED: u32 = 3;
/// `GENERIC_ALL` (`0x1000_0000`).
const GENERIC_ALL: u32 = 0x1000_0000;
/// `GENERIC_WRITE` (`0x4000_0000`).
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Trusted administrative SIDs permitted to own or modify a protected installation location:
/// - `NT AUTHORITY\SYSTEM` (`S-1-5-18`)
/// - `BUILTIN\Administrators` (`S-1-5-32-544`)
/// - `NT SERVICE\TrustedInstaller` (`S-1-5-80-956008885-...`)
const TRUSTED_ADMIN_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// Write/modify/delete rights forbidden for non-admin principals on the executable file itself.
const EXE_FORBIDDEN_MASK: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

/// Write/create/delete rights forbidden for non-admin principals on the executable's direct
/// containing directory (prevents file swap, DLL drop, or rename).
const PARENT_DIR_FORBIDDEN_MASK: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

/// Delete/rename/DACL rights forbidden for non-admin principals on higher ancestor directories
/// (prevents folder swap while permitting Windows' standard `C:\` `Authenticated Users:(AD)`).
const ANCESTOR_DIR_FORBIDDEN_MASK: u32 =
    FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

/// RAII guard releasing a Win32 `LocalAlloc` security descriptor or SID string.
struct LocalAllocGuard(*mut c_void);

impl Drop for LocalAllocGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Paired with `GetNamedSecurityInfoW` or `ConvertSidToStringSidW`.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

/// Reason why an executable path is not considered administrator-protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnprotectedReason {
    /// Executable is on a network share, removable medium, or non-local path.
    NonFixedDrive,
    /// Executable file or directory security descriptor could not be inspected.
    InspectionFailed(String),
    /// Executable file or containing directory is not owned by a trusted administrative account.
    UntrustedOwner(String),
    /// Object has a NULL DACL (effectively grants full control to Everyone).
    NullDacl(String),
    /// A non-administrative principal holds write/modify/delete permissions on the path.
    UserWritable {
        /// Path that failed the DACL check.
        path: String,
        /// Non-admin SID holding disallowed access bits.
        sid: String,
    },
}

impl std::fmt::Display for UnprotectedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFixedDrive => write!(
                f,
                "executable must be installed on a local fixed drive (such as Program Files)"
            ),
            Self::InspectionFailed(detail) => {
                write!(f, "failed to verify installation path ACL: {detail}")
            }
            Self::UntrustedOwner(path) => write!(
                f,
                "'{path}' is not owned by SYSTEM, Administrators, or TrustedInstaller; install Loom in a protected directory (such as Program Files)"
            ),
            Self::NullDacl(path) => write!(
                f,
                "'{path}' has no DACL restriction; install Loom in a protected directory (such as Program Files)"
            ),
            Self::UserWritable { path, sid } => write!(
                f,
                "'{path}' is writable by non-administrative principal {sid}; install Loom in a protected directory (such as Program Files)"
            ),
        }
    }
}

/// Returns `true` when `sid` is one of Windows' built-in administrative security principals.
#[inline]
pub(crate) fn is_trusted_admin_sid(sid: &str) -> bool {
    TRUSTED_ADMIN_SIDS
        .iter()
        .any(|trusted| sid.eq_ignore_ascii_case(trusted))
}

/// Verifies that `exe_path` resides in an administrator-protected local directory whose ACLs
/// prevent non-administrative processes from modifying, replacing, or swapping the executable.
pub(crate) fn verify_protected_executable(exe_path: &Path) -> Result<(), UnprotectedReason> {
    let canonical = exe_path
        .canonicalize()
        .unwrap_or_else(|_| exe_path.to_path_buf());
    let raw = canonical.to_string_lossy();
    let clean_str = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let clean_path = Path::new(clean_str);

    // 1. Require a local fixed drive (`C:\...`); reject UNC shares (`\\server\share`) and removable drives.
    if clean_str.starts_with(r"\\") || !is_fixed_local_drive(clean_str) {
        return Err(UnprotectedReason::NonFixedDrive);
    }

    // 2. Verify executable file itself: trusted owner + no non-admin write/delete/ACL rights.
    verify_object_security(clean_path, true, EXE_FORBIDDEN_MASK)?;

    // 3. Verify direct parent directory: trusted owner + no non-admin create/write/delete/ACL rights.
    let Some(parent) = clean_path.parent() else {
        return Err(UnprotectedReason::InspectionFailed(
            "executable has no parent directory".into(),
        ));
    };
    verify_object_security(parent, true, PARENT_DIR_FORBIDDEN_MASK)?;

    // 4. Verify higher ancestor directories up to drive root cannot be deleted/renamed by non-admins.
    let mut cursor = parent.parent();
    while let Some(ancestor) = cursor {
        verify_object_security(ancestor, false, ANCESTOR_DIR_FORBIDDEN_MASK)?;
        cursor = ancestor.parent();
    }

    Ok(())
}

fn is_fixed_local_drive(clean_path: &str) -> bool {
    let bytes = clean_path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return false;
    }
    let root = format!("{}:\\", bytes[0] as char);
    let wide = to_wide_null(&root);
    // SAFETY: `wide` is a valid NUL-terminated drive root path such as `C:\`.
    unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED }
}

fn verify_object_security(
    path: &Path,
    require_trusted_owner: bool,
    forbidden_mask: u32,
) -> Result<(), UnprotectedReason> {
    let display = path.display().to_string();
    let wide = to_wide_null(&display);

    let mut owner_sid: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();

    // SAFETY: Queries owner and DACL of `path`; `sd` is released via `LocalAllocGuard`.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner_sid,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    let _sd_guard = LocalAllocGuard(sd);

    if rc != ERROR_SUCCESS {
        return Err(UnprotectedReason::InspectionFailed(format!(
            "GetNamedSecurityInfoW('{display}') failed (os error {rc})"
        )));
    }

    if require_trusted_owner {
        let owner_str = sid_to_string(owner_sid).ok_or_else(|| {
            UnprotectedReason::InspectionFailed(format!(
                "could not decode owner SID for '{display}'"
            ))
        })?;
        if !is_trusted_admin_sid(&owner_str) {
            return Err(UnprotectedReason::UntrustedOwner(display));
        }
    }

    if dacl.is_null() {
        return Err(UnprotectedReason::NullDacl(display));
    }

    let mut size_info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is a valid non-null ACL owned by `sd`.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut size_info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(UnprotectedReason::InspectionFailed(format!(
            "GetAclInformation('{display}') failed"
        )));
    }

    for idx in 0..size_info.AceCount {
        let mut ace_ptr: *mut c_void = ptr::null_mut();
        // SAFETY: `idx < size_info.AceCount` on valid `dacl`.
        if unsafe { GetAce(dacl, idx, &mut ace_ptr) } == 0 || ace_ptr.is_null() {
            continue;
        }
        // SAFETY: Every ACE starts with `ACE_HEADER`; `ACCESS_ALLOWED_ACE_TYPE` matches `ACCESS_ALLOWED_ACE`.
        let ace = unsafe { &*ace_ptr.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue;
        }
        // Skip inherit-only template ACEs (e.g., `CREATOR OWNER:(OI)(CI)(IO)(F)`).
        if (ace.Header.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        if (ace.Mask & forbidden_mask) == 0 {
            continue;
        }

        let sid_ptr = (&ace.SidStart as *const u32).cast::<c_void>().cast_mut();
        let Some(sid_str) = sid_to_string(sid_ptr) else {
            return Err(UnprotectedReason::InspectionFailed(format!(
                "could not decode ACE SID for '{display}'"
            )));
        };
        if !is_trusted_admin_sid(&sid_str) {
            return Err(UnprotectedReason::UserWritable {
                path: display,
                sid: sid_str,
            });
        }
    }

    Ok(())
}

fn sid_to_string(sid: PSID) -> Option<String> {
    if sid.is_null() {
        return None;
    }
    let mut sid_str: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid Win32 SID pointer.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut sid_str) };
    if ok == 0 || sid_str.is_null() {
        return None;
    }
    let _guard = LocalAllocGuard(sid_str.cast::<c_void>());
    // SAFETY: `sid_str` is a NUL-terminated UTF-16 buffer allocated by `ConvertSidToStringSidW`.
    let text = unsafe {
        let mut len = 0usize;
        while *sid_str.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, len))
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system32_binary_is_recognized_as_protected_and_temp_file_is_rejected() {
        let system_cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        if system_cmd.exists() {
            assert_eq!(
                verify_protected_executable(system_cmd),
                Ok(()),
                "System32 executable must pass protected-directory ACL check"
            );
        }

        // A file created in the current user's temp directory must be rejected as user-writable.
        let temp_file =
            std::env::temp_dir().join(format!("loom-acl-guard-test-{}.exe", std::process::id()));
        std::fs::write(&temp_file, b"test").expect("write temp exe fixture");
        let res = verify_protected_executable(&temp_file);
        let _ = std::fs::remove_file(&temp_file);
        assert!(
            res.is_err(),
            "user-writable temp executable must be rejected by install_guard, got {res:?}"
        );

        // UNC network paths must always be rejected.
        assert_eq!(
            verify_protected_executable(Path::new(r"\\server\share\loom-app.exe")),
            Err(UnprotectedReason::NonFixedDrive)
        );
    }

    #[test]
    fn trusted_admin_sids_match_case_insensitively() {
        assert!(is_trusted_admin_sid("S-1-5-18"));
        assert!(is_trusted_admin_sid("s-1-5-32-544"));
        assert!(is_trusted_admin_sid(
            "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        ));
        assert!(!is_trusted_admin_sid("S-1-5-32-545")); // BUILTIN\Users
        assert!(!is_trusted_admin_sid("S-1-5-11")); // Authenticated Users
        assert!(!is_trusted_admin_sid("S-1-1-0")); // Everyone
    }
}
