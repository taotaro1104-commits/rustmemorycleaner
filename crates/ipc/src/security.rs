use crate::Caller;
use std::ffi::c_void;
use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, BOOL, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, RevertToSelf, TokenUser, WinBuiltinAdministratorsSid,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken};

/// SYSTEM と Administrators は全権、対話ユーザー(IU)は読み書きのみ。
/// ネットワーク経由(NU)や他のアカウントは許可しない。
const SDDL: windows::core::PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");

/// DACL 付き SECURITY_ATTRIBUTES。Drop で記述子を解放する。
pub struct PipeSecurity {
    sd: PSECURITY_DESCRIPTOR,
    attrs: Box<SECURITY_ATTRIBUTES>,
}

// 記述子は不変で、作成後は読み取りのみ。
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    pub fn new() -> std::io::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut sd, None)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        }
        let attrs = Box::new(SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        });
        Ok(Self { sd, attrs })
    }

    pub fn as_raw(&self) -> *mut c_void {
        &*self.attrs as *const SECURITY_ATTRIBUTES as *mut c_void
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.sd.0));
        }
    }
}

/// トークンのユーザー SID（文字列）。
unsafe fn token_user_sid(tok: HANDLE) -> Option<String> {
    let mut need = 0u32;
    let _ = GetTokenInformation(tok, TokenUser, None, 0, &mut need);
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), need, &mut need).ok()?;
    let mut s = PWSTR::null();
    ConvertSidToStringSidW((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, &mut s).ok()?;
    let out = s.to_string().ok();
    let _ = LocalFree(HLOCAL(s.0 as *mut c_void));
    out
}

/// 偽装トークンが（昇格済みの）Administrators のメンバーか。UAC の非昇格トークンでは Administrators は
/// 「拒否専用」なので false になる。
unsafe fn token_is_admin(tok: HANDLE) -> bool {
    let mut sid = [0u8; 68]; // SECURITY_MAX_SID_SIZE
    let mut len = sid.len() as u32;
    if CreateWellKnownSid(WinBuiltinAdministratorsSid, PSID::default(), PSID(sid.as_mut_ptr() as *mut c_void), &mut len).is_err() {
        return false;
    }
    let mut member = BOOL(0);
    CheckTokenMembership(tok, PSID(sid.as_mut_ptr() as *mut c_void), &mut member).is_ok() && member.as_bool()
}

/// 接続中のクライアントの身元を取得する。偽装（ImpersonateNamedPipeClient）はこの関数の中だけで行い、
/// 必ず元に戻す（await をまたがない。tokio のスレッド移動で偽装が漏れないようにするため）。
/// 取得に失敗したら「身元不明＝最低権限」を返す。
pub fn identify_client(pipe: std::os::windows::io::RawHandle) -> Caller {
    unsafe {
        if ImpersonateNamedPipeClient(HANDLE(pipe as *mut c_void)).is_err() {
            return Caller::unknown();
        }
        let mut tok = HANDLE::default();
        let caller = if OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut tok).is_ok() {
            let c = Caller { is_admin: token_is_admin(tok), user_sid: token_user_sid(tok) };
            let _ = CloseHandle(tok);
            c
        } else {
            Caller::unknown()
        };
        let _ = RevertToSelf();
        caller
    }
}

/// このプロセスのユーザー SID（テストや診断用）。
pub fn current_process_user_sid() -> Option<String> {
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).ok()?;
        let s = token_user_sid(tok);
        let _ = CloseHandle(tok);
        s
    }
}
