use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
use windows::Win32::Security::*;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 設計書6章: 有効化するのはこの3つだけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    ProfileSingleProcess,
    IncreaseQuota,
    Debug,
}

impl Privilege {
    fn name(self) -> &'static str {
        match self {
            Self::ProfileSingleProcess => "SeProfileSingleProcessPrivilege",
            Self::IncreaseQuota => "SeIncreaseQuotaPrivilege",
            Self::Debug => "SeDebugPrivilege",
        }
    }
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut t = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut n = 0u32;
        let ok = GetTokenInformation(
            t,
            TokenElevation,
            Some(&mut e as *mut _ as *mut _),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        )
        .is_ok();
        let _ = CloseHandle(t);
        ok && e.TokenIsElevated != 0
    }
}

/// 特権を有効化。トークンに無い／付与されなかった場合は Err（レベルは「利用不可」表示）。
pub fn enable_privilege(p: Privilege) -> Result<(), String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)
            .map_err(|e| format!("OpenProcessToken: {e}"))?;
        let r = (|| {
            let name: Vec<u16> = p.name().encode_utf16().chain(Some(0)).collect();
            let mut luid = LUID::default();
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name.as_ptr()), &mut luid)
                .map_err(|e| format!("LookupPrivilegeValue: {e}"))?;
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None)
                .map_err(|e| format!("AdjustTokenPrivileges: {e}"))?;
            // 呼び出しが成功でも ERROR_NOT_ALL_ASSIGNED が返ることがある。
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                return Err(format!("{} はこのトークンに無い（管理者権限が必要）", p.name()));
            }
            Ok(())
        })();
        let _ = CloseHandle(token);
        r
    }
}
