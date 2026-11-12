use std::ffi::c_void;
use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

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
