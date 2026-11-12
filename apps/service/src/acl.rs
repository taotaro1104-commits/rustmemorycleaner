//! データフォルダーの ACL（設計書6章）: 書き込みは SYSTEM と Administrators のみ、一般ユーザーは読み取りのみ。
use std::path::Path;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::*;

/// 継承を切った DACL: SYSTEM/Administrators は全権、Users は読み取りと実行。
const SDDL: PCWSTR = w!("D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)");

pub fn secure_data_dir(dir: &Path) -> Result<(), String> {
    let mut path: Vec<u16> = dir.as_os_str().to_string_lossy().encode_utf16().collect();
    path.push(0);
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut sd, None)
            .map_err(|e| format!("SDDL 変換: {e}"))?;
        let r = (|| {
            let mut present = false.into();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut defaulted = false.into();
            GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted).map_err(|e| format!("DACL 取得: {e}"))?;
            let rc = SetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            );
            if rc.0 != 0 {
                return Err(format!("SetNamedSecurityInfo: エラー {}", rc.0));
            }
            Ok(())
        })();
        let _ = LocalFree(HLOCAL(sd.0));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際の適用は利用者のフォルダーを締め出し得るのでテストしない。SDDL が妥当かだけ確認する。
    #[test]
    fn sddl_is_valid_and_has_a_dacl() {
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut sd, None).unwrap();
            let mut present = false.into();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut defaulted = false.into();
            GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted).unwrap();
            assert!(present.as_bool() && !dacl.is_null());
            let _ = LocalFree(HLOCAL(sd.0));
        }
    }
}
