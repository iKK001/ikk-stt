//! Integrity level of the target window: UIPI drops input to a window of
//! higher integrity *silently*, so the level is compared before writing.
//! With the UIAccess token flag (signed, installed build) Windows relaxes
//! UIPI and the input is allowed.
//!
//! Chain: HWND → PID → process handle → token → integrity label → RID.

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TokenUIAccess,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    InputAllowed,
    InputAllowedViaUiAccess,
    Blocked,
}

/// Pure decision: target × own integrity × own UIAccess flag.
pub fn classify(target_rid: u32, own_rid: u32, own_uiaccess: bool) -> Verdict {
    if target_rid > own_rid {
        if own_uiaccess {
            Verdict::InputAllowedViaUiAccess
        } else {
            Verdict::Blocked
        }
    } else {
        Verdict::InputAllowed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowIntegrity {
    InputAllowed {
        target_rid: u32,
        own_rid: u32,
    },
    InputAllowedViaUiAccess {
        target_rid: u32,
        own_rid: u32,
    },
    Elevated {
        target_rid: u32,
        own_rid: u32,
    },
    /// Process/token could not be opened: for a foreground window the usual
    /// sign of higher privileges; treated as blocked (fail-closed).
    QueryDenied {
        detail: String,
    },
    Unknown {
        detail: String,
    },
}

impl WindowIntegrity {
    pub fn blocks_input(&self) -> bool {
        matches!(self, Self::Elevated { .. } | Self::QueryDenied { .. })
    }
    pub fn detail(&self) -> String {
        match self {
            Self::InputAllowed { target_rid, own_rid } => format!("target {target_rid}, own {own_rid}: allowed"),
            Self::InputAllowedViaUiAccess { target_rid, own_rid } => {
                format!("target {target_rid} > own {own_rid}, but UIAccess: allowed")
            }
            Self::Elevated { target_rid, own_rid } => format!("target {target_rid} > own {own_rid}: UIPI blocks input"),
            Self::QueryDenied { detail } => format!("integrity not queryable ({detail}); treated as elevated"),
            Self::Unknown { detail } => format!("integrity unknown ({detail})"),
        }
    }
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn compare_with_own(hwnd: HWND) -> WindowIntegrity {
    let own_rid = match current_process_rid() {
        Ok(r) => r,
        Err(e) => {
            return WindowIntegrity::QueryDenied {
                detail: format!("own integrity unreadable: {e}"),
            }
        }
    };
    let own_uiaccess = current_process_has_uiaccess().unwrap_or(false);
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return WindowIntegrity::Unknown {
            detail: "window has no process id".into(),
        };
    }
    match process_rid(pid) {
        Ok(target_rid) => match classify(target_rid, own_rid, own_uiaccess) {
            Verdict::InputAllowed => WindowIntegrity::InputAllowed { target_rid, own_rid },
            Verdict::InputAllowedViaUiAccess => WindowIntegrity::InputAllowedViaUiAccess { target_rid, own_rid },
            Verdict::Blocked => WindowIntegrity::Elevated { target_rid, own_rid },
        },
        Err(e) => {
            if own_uiaccess {
                WindowIntegrity::Unknown {
                    detail: format!("{e}; UIAccess present, input allowed"),
                }
            } else {
                WindowIntegrity::QueryDenied { detail: e }
            }
        }
    }
}

pub fn current_process_has_uiaccess() -> Result<bool, String> {
    unsafe {
        let mut raw = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw).map_err(|e| format!("OpenProcessToken: {e}"))?;
        let token = OwnedHandle(raw);
        let mut ui_access: u32 = 0;
        let mut needed: u32 = 0;
        GetTokenInformation(
            token.0,
            TokenUIAccess,
            Some(&mut ui_access as *mut u32 as *mut c_void),
            std::mem::size_of::<u32>() as u32,
            &mut needed,
        )
        .map_err(|e| format!("GetTokenInformation(TokenUIAccess): {e}"))?;
        Ok(ui_access != 0)
    }
}

pub fn current_process_rid() -> Result<u32, String> {
    unsafe { rid_from_process(GetCurrentProcess()) }
}

fn process_rid(pid: u32) -> Result<u32, String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
    let process = OwnedHandle(process);
    unsafe { rid_from_process(process.0) }
}

unsafe fn rid_from_process(process: HANDLE) -> Result<u32, String> {
    let mut raw = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut raw).map_err(|e| format!("OpenProcessToken: {e}"))?;
    let token = OwnedHandle(raw);
    let mut needed: u32 = 0;
    let _ = GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &mut needed);
    if needed == 0 {
        return Err("GetTokenInformation returned no buffer size".into());
    }
    let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
    let mut buf: Vec<u64> = vec![0; words.max(1)];
    GetTokenInformation(
        token.0,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut c_void),
        needed,
        &mut needed,
    )
    .map_err(|e| format!("GetTokenInformation(TokenIntegrityLevel): {e}"))?;
    let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    if sid.is_invalid() {
        return Err("integrity label without SID".into());
    }
    let count_ptr = GetSidSubAuthorityCount(sid);
    if count_ptr.is_null() || *count_ptr == 0 {
        return Err("SID without sub-authorities".into());
    }
    let rid_ptr = GetSidSubAuthority(sid, (*count_ptr - 1) as u32);
    if rid_ptr.is_null() {
        return Err("GetSidSubAuthority returned NULL".into());
    }
    Ok(*rid_ptr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_table() {
        assert_eq!(classify(8192, 8192, false), Verdict::InputAllowed);
        assert_eq!(classify(4096, 8192, false), Verdict::InputAllowed);
        assert_eq!(classify(12288, 8192, false), Verdict::Blocked);
        assert_eq!(classify(12288, 8192, true), Verdict::InputAllowedViaUiAccess);
    }
}
