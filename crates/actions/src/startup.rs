//! スタートアップの無効化（タスクマネージャーと同じ StartupApproved の値を書き換える。登録自体は消さない）。
//! ユーザー分は HKEY_USERS\<SID> 配下を直接編集する（サービスは SYSTEM で HKCU を持たないため）。
use memopt_core::stop::{startup_key, RegRoot};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn root(r: RegRoot) -> HKEY {
    match r {
        RegRoot::LocalMachine => HKEY_LOCAL_MACHINE,
        RegRoot::Users => HKEY_USERS,
    }
}

/// 無効を表す 12 バイト（先頭 DWORD = 3、続けて無効にした FILETIME）。
pub fn disabled_value(now_filetime: u64) -> Vec<u8> {
    let mut v = vec![3, 0, 0, 0];
    v.extend_from_slice(&now_filetime.to_le_bytes());
    v
}

/// StartupApproved の値が「有効」を表すか（先頭バイトが偶数 = 有効）。
pub fn is_enabled_value(raw: &[u8]) -> bool {
    raw.first().is_none_or(|b| b & 1 == 0)
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open(r: HKEY, path: &str, access: REG_SAM_FLAGS, create: bool) -> Result<Key, String> {
    let w = wide(path);
    let mut k = HKEY::default();
    unsafe {
        let rc = if create {
            RegCreateKeyExW(r, PCWSTR(w.as_ptr()), 0, PCWSTR::null(), REG_OPTION_NON_VOLATILE, access, None, &mut k, None)
        } else {
            RegOpenKeyExW(r, PCWSTR(w.as_ptr()), 0, access, &mut k)
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("レジストリキー {path} を開けません (エラー {})", rc.0));
        }
    }
    Ok(Key(k))
}

fn value_exists(k: &Key, name: &str) -> bool {
    let w = wide(name);
    unsafe { RegQueryValueExW(k.0, PCWSTR(w.as_ptr()), None, None, None, None) == ERROR_SUCCESS }
}

fn get_raw(k: &Key, name: &str) -> Option<Vec<u8>> {
    let w = wide(name);
    unsafe {
        let mut len = 0u32;
        if RegQueryValueExW(k.0, PCWSTR(w.as_ptr()), None, None, None, Some(&mut len)) != ERROR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        (RegQueryValueExW(k.0, PCWSTR(w.as_ptr()), None, None, Some(buf.as_mut_ptr()), Some(&mut len)) == ERROR_SUCCESS).then(|| {
            buf.truncate(len as usize);
            buf
        })
    }
}

fn set_raw(k: &Key, name: &str, raw: Option<&[u8]>) -> Result<(), String> {
    let w = wide(name);
    unsafe {
        let rc = match raw {
            Some(b) => RegSetValueExW(k.0, PCWSTR(w.as_ptr()), 0, REG_BINARY, Some(b)),
            None => {
                // 事前確認せず直接消す（読み取り権限の有無で黙ってスキップしないため）。無ければ成功扱い。
                let rc = RegDeleteValueW(k.0, PCWSTR(w.as_ptr()));
                if rc == ERROR_FILE_NOT_FOUND { ERROR_SUCCESS } else { rc }
            }
        };
        (rc == ERROR_SUCCESS).then_some(()).ok_or(format!("StartupApproved の値 {name} を書き換えられません (エラー {})", rc.0))
    }
}

/// 明示パス版（テストでも使う）。Run に登録がある名前だけを対象にし、変更前の生データを返す。
fn disable_at(r: HKEY, run: &str, approved: &str, name: &str, now_ft: u64) -> Result<Option<Vec<u8>>, String> {
    let runk = open(r, run, KEY_READ, false)?;
    if !value_exists(&runk, name) {
        return Err(format!("Run に {name} の登録がありません"));
    }
    let ak = open(r, approved, KEY_READ | KEY_SET_VALUE, true)?;
    let before = get_raw(&ak, name);
    set_raw(&ak, name, Some(&disabled_value(now_ft)))?;
    Ok(before)
}

/// 実行中ユーザーの SID 文字列（HKEY_USERS 配下のキー名と一致する）。テストや診断用。
pub fn current_user_sid() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).ok()?;
        let mut need = 0u32;
        let _ = GetTokenInformation(tok, TokenUser, None, 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        let r = GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr() as *mut _), need, &mut need)
            .ok()
            .and_then(|_| {
                let mut s = PWSTR::null();
                ConvertSidToStringSidW((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, &mut s).ok()?;
                let out = s.to_string().ok();
                let _ = LocalFree(HLOCAL(s.0 as *mut _));
                out
            });
        let _ = CloseHandle(tok);
        r
    }
}

fn paths(user_sid: Option<&str>) -> Result<(HKEY, String, String), String> {
    let (r, run) = startup_key(user_sid, false).ok_or("不正なユーザー SID です")?;
    let (_, appr) = startup_key(user_sid, true).ok_or("不正なユーザー SID です")?;
    Ok((root(r), run, appr))
}

/// スタートアップを無効化し、変更前の値（無ければ None）を返す。
pub fn disable(user_sid: Option<&str>, name: &str, now_filetime: u64) -> Result<Option<Vec<u8>>, String> {
    let (r, run, appr) = paths(user_sid)?;
    disable_at(r, &run, &appr, name, now_filetime)
}

/// 変更前の値に戻す（None なら値を削除 = 既定の有効）。
pub fn restore(user_sid: Option<&str>, name: &str, before: Option<&[u8]>) -> Result<(), String> {
    let (r, _, appr) = paths(user_sid)?;
    let ak = open(r, &appr, KEY_SET_VALUE, true)?;
    set_raw(&ak, name, before)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本物の Run キーには触れず、HKCU の使い捨てキーで往復を確認する。
    struct Scratch(String);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let base = format!(r"Software\MemOptTest_{tag}_{}", std::process::id());
            let run = open(HKEY_CURRENT_USER, &format!(r"{base}\Run"), KEY_SET_VALUE, true).unwrap();
            let val = wide(r#""C:\x\app.exe" --tray"#);
            let bytes = unsafe { std::slice::from_raw_parts(val.as_ptr() as *const u8, val.len() * 2) };
            let n = wide("MyApp");
            unsafe { assert_eq!(RegSetValueExW(run.0, PCWSTR(n.as_ptr()), 0, REG_SZ, Some(bytes)), ERROR_SUCCESS) };
            Self(base)
        }
        fn run(&self) -> String { format!(r"{}\Run", self.0) }
        fn approved(&self) -> String { format!(r"{}\StartupApproved\Run", self.0) }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let w = wide(&self.0);
            unsafe { let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(w.as_ptr())); }
        }
    }

    fn approved_raw(s: &Scratch, name: &str) -> Option<Vec<u8>> {
        open(HKEY_CURRENT_USER, &s.approved(), KEY_READ, false).ok().and_then(|k| get_raw(&k, name))
    }

    #[test]
    fn disabled_value_layout_and_enabled_detection() {
        let v = disabled_value(0x0102030405060708);
        assert_eq!(v.len(), 12);
        assert_eq!(&v[..4], &[3, 0, 0, 0]);
        assert_eq!(&v[4..], &0x0102030405060708u64.to_le_bytes());
        assert!(!is_enabled_value(&v));
        assert!(is_enabled_value(&[2, 0, 0, 0]) && is_enabled_value(&[]));
    }

    #[test]
    fn disable_then_restore_roundtrips_when_no_prior_value() {
        let s = Scratch::new("none");
        let before = disable_at(HKEY_CURRENT_USER, &s.run(), &s.approved(), "MyApp", 123).unwrap();
        assert_eq!(before, None);
        assert!(!is_enabled_value(&approved_raw(&s, "MyApp").unwrap()));
        // 復元: 元は値なし → 削除
        let ak = open(HKEY_CURRENT_USER, &s.approved(), KEY_SET_VALUE, true).unwrap();
        set_raw(&ak, "MyApp", before.as_deref()).unwrap();
        assert_eq!(approved_raw(&s, "MyApp"), None);
    }

    #[test]
    fn disable_preserves_and_restores_an_existing_enabled_value_byte_for_byte() {
        let s = Scratch::new("prior");
        let prior = vec![2, 0, 0, 0, 9, 9, 9, 9, 9, 9, 9, 9];
        let ak = open(HKEY_CURRENT_USER, &s.approved(), KEY_SET_VALUE, true).unwrap();
        set_raw(&ak, "MyApp", Some(&prior)).unwrap();
        let before = disable_at(HKEY_CURRENT_USER, &s.run(), &s.approved(), "MyApp", 1).unwrap();
        assert_eq!(before.as_deref(), Some(prior.as_slice()));
        set_raw(&ak, "MyApp", before.as_deref()).unwrap();
        assert_eq!(approved_raw(&s, "MyApp").unwrap(), prior);
    }

    #[test]
    fn unregistered_name_is_refused_and_creates_nothing() {
        let s = Scratch::new("ghost");
        assert!(disable_at(HKEY_CURRENT_USER, &s.run(), &s.approved(), "NoSuch", 1).is_err());
        assert_eq!(approved_raw(&s, "NoSuch"), None);
    }

    #[test]
    fn invalid_sid_is_rejected_before_touching_the_registry() {
        assert!(disable(Some(r"S-1-5-21-1\..\Evil"), "x", 1).is_err());
        assert!(restore(Some("S-1-5-18"), "x", None).is_err());
    }
}
