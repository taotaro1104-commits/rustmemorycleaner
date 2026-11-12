//! Run キーと StartupApproved の読み取り（変更はしない。P4 まで読み取り専用）。
use crate::extract_exe_path;
use memopt_core::StartupEntry;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;


/// 名前 -> 生のバイト列（REG_SZ/REG_BINARY）を列挙。
unsafe fn values(root: HKEY, sub: PCWSTR) -> Vec<(String, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut key = HKEY::default();
    if RegOpenKeyExW(root, sub, 0, KEY_READ, &mut key) != ERROR_SUCCESS { return out; }
    let mut i = 0;
    loop {
        let mut name = [0u16; 512];
        let mut nlen = name.len() as u32;
        let mut ty = 0u32;
        let mut data = vec![0u8; 4096];
        let mut dlen = data.len() as u32;
        let r = RegEnumValueW(key, i, windows::core::PWSTR(name.as_mut_ptr()), &mut nlen, None,
            Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut dlen));
        if r != ERROR_SUCCESS { break; }
        data.truncate(dlen as usize);
        out.push((String::from_utf16_lossy(&name[..nlen as usize]), ty, data));
        i += 1;
    }
    let _ = RegCloseKey(key);
    out
}

fn utf16(b: &[u8]) -> String {
    let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&u).trim_end_matches('\0').to_string()
}

unsafe fn expand(s: &str) -> String {
    let src: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 2048];
    let n = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) as usize;
    if n == 0 || n > buf.len() { return s.to_string(); }
    String::from_utf16_lossy(&buf[..n - 1])
}

/// HKEY_USERS 直下のユーザー SID（読み込み済みのプロファイルのみ。_Classes は除く）。
unsafe fn loaded_user_sids() -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        let r = RegEnumKeyExW(HKEY_USERS, i, windows::core::PWSTR(name.as_mut_ptr()), &mut len, None, windows::core::PWSTR::null(), None, None);
        if r != ERROR_SUCCESS {
            break;
        }
        let sid = String::from_utf16_lossy(&name[..len as usize]);
        if memopt_core::stop::startup_key(Some(&sid), false).is_some() {
            out.push(sid);
        }
        i += 1;
    }
    out
}

unsafe fn read_hive(root: HKEY, user_sid: Option<&str>, out: &mut Vec<StartupEntry>) {
    let (Some((_, run)), Some((_, approved_path))) =
        (memopt_core::stop::startup_key(user_sid, false), memopt_core::stop::startup_key(user_sid, true))
    else {
        return;
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let (run_w, appr_w) = (wide(&run), wide(&approved_path));
    let approved = values(root, PCWSTR(appr_w.as_ptr()));
    for (name, ty, data) in values(root, PCWSTR(run_w.as_ptr())) {
        if ty != REG_SZ.0 && ty != REG_EXPAND_SZ.0 {
            continue;
        }
        let Some(exe_path) = extract_exe_path(&expand(&utf16(&data))) else { continue };
        // StartupApproved: 先頭バイトの下位ビットが 1 なら無効（偶数=有効 / 奇数=無効）
        let enabled = approved
            .iter()
            .find(|(n, _, _)| n.eq_ignore_ascii_case(&name))
            .and_then(|(_, _, d)| d.first().copied())
            .map(|b| b & 1 == 0)
            .unwrap_or(true);
        out.push(StartupEntry {
            name,
            exe_path,
            machine_wide: user_sid.is_none(),
            user_sid: user_sid.map(str::to_string),
            enabled,
        });
    }
}

/// マシン全体 + 読み込み済みの全ユーザー（SYSTEM のサービスからも見える）。読み取り専用。
pub fn startup_entries() -> Vec<StartupEntry> {
    let mut out = Vec::new();
    unsafe {
        read_hive(HKEY_LOCAL_MACHINE, None, &mut out);
        for sid in loaded_user_sids() {
            read_hive(HKEY_USERS, Some(&sid), &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_without_panicking() {
        let e = super::startup_entries();
        assert!(e.iter().all(|x| !x.exe_path.is_empty()));
    }
}
