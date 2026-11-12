//! プロセス → 機能への集約（設計書4章）。
pub mod trend;
pub use trend::TrendTracker;
use memopt_core::{Feature, FeatureKind, ProcessInfo, SafetyClass, StartupEntry};
use std::collections::BTreeMap;

/// 組み込み保護プロセス（フルパス照合が必須。名前だけでは保護扱いにしない）。
const PROTECTED_NAMES: &[&str] = &[
    "system", "registry", "smss.exe", "csrss.exe", "wininit.exe",
    "winlogon.exe", "services.exe", "lsass.exe", "dwm.exe", "memory compression",
];
const BROWSERS: &[&str] = &["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe"];

fn is_system_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.starts_with(r"c:\windows\system32\") || p.starts_with(r"c:\windows\syswow64\")
}

/// 組み込み保護（名前+フルパス照合）。ユーザー保護パスは大文字小文字無視の完全一致。
pub fn is_protected(p: &ProcessInfo, user_paths: &[String]) -> bool {
    let (kind, _, _) = kind_and_key(p);
    if classify(p, kind) == SafetyClass::Protected { return true; }
    p.exe_path.as_deref().is_some_and(|x| user_paths.iter().any(|u| u.eq_ignore_ascii_case(x)))
}

fn classify(p: &ProcessInfo, kind: FeatureKind) -> SafetyClass {
    let name = p.exe_name.to_ascii_lowercase();
    let pathed = p.exe_path.as_deref().map(is_system_path).unwrap_or(false);
    // 名前が保護対象で、パスが system32 配下か、パスを取得できない場合は保護（確認できないものは触らない）。
    if PROTECTED_NAMES.contains(&name.as_str()) && (pathed || p.exe_path.is_none()) {
        return SafetyClass::Protected;
    }
    match kind {
        FeatureKind::System => SafetyClass::Protected,
        FeatureKind::Service | FeatureKind::Wsl => SafetyClass::Caution,
        _ if name == "explorer.exe" || name == "audiodg.exe" => SafetyClass::Caution,
        _ => SafetyClass::Stoppable,
    }
}

fn kind_and_key(p: &ProcessInfo) -> (FeatureKind, String, String) {
    let name = p.exe_name.to_ascii_lowercase();
    if name.starts_with("vmmem") {
        return (FeatureKind::Wsl, "wsl".into(), p.exe_name.clone());
    }
    if let Some(svc) = p.services.first() {
        let key = format!("svc:{}", p.services.join("+"));
        return (FeatureKind::Service, key, svc.clone());
    }
    if let Some(pkg) = &p.package_family {
        return (FeatureKind::StoreApp, format!("pkg:{pkg}"), pkg.clone());
    }
    if BROWSERS.contains(&name.as_str()) {
        return (FeatureKind::Browser, format!("browser:{name}"), name.trim_end_matches(".exe").to_string());
    }
    if p.exe_path.is_none() && matches!(name.as_str(), "system" | "registry" | "memory compression") {
        return (FeatureKind::System, format!("sys:{name}"), p.exe_name.clone());
    }
    let key = p.exe_path.clone().unwrap_or_else(|| name.clone()).to_ascii_lowercase();
    (FeatureKind::DesktopApp, format!("app:{key}"), p.exe_name.clone())
}

pub fn group(procs: &[ProcessInfo]) -> Vec<Feature> {
    let mut map: BTreeMap<String, Feature> = BTreeMap::new();
    for p in procs {
        let (kind, key, name) = kind_and_key(p);
        let safety = classify(p, kind);
        let f = map.entry(key.clone()).or_insert_with(|| Feature {
            id: key, name, kind, pids: vec![], private_bytes: 0,
            working_set: 0, private_working_set: 0, safety, startup: false, targets: Default::default(),
        });
        f.pids.push(p.pid);
        f.private_bytes += p.private_bytes.unwrap_or(0);
        f.working_set += p.working_set.unwrap_or(0);
        f.private_working_set += p.private_working_set.unwrap_or(0);
        f.safety = f.safety.strictest(safety);
    }
    let mut v: Vec<Feature> = map.into_values().collect();
    v.sort_by(|a, b| b.private_bytes.cmp(&a.private_bytes));
    v
}

/// 有効なスタートアップ登録と実行パスが一致する機能に印を付ける（大文字小文字無視のフルパス一致）。
pub fn mark_startup(features: &mut [Feature], procs: &[ProcessInfo], entries: &[StartupEntry]) {
    use std::collections::HashSet;
    let reg: HashSet<String> = entries.iter().filter(|e| e.enabled)
        .map(|e| e.exe_path.to_ascii_lowercase()).collect();
    for f in features.iter_mut() {
        f.startup = procs.iter().any(|p| f.pids.contains(&p.pid)
            && p.exe_path.as_deref().is_some_and(|x| reg.contains(&x.to_ascii_lowercase())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::{FeatureKind, SafetyClass};

    fn p(pid: u32, ppid: u32, exe: &str, path: &str, cmd: &str, priv_b: u64) -> ProcessInfo {
        ProcessInfo {
            pid, parent_pid: ppid, start_time: 1, exe_name: exe.into(),
            exe_path: Some(path.into()), command_line: Some(cmd.into()),
            package_family: None, services: vec![],
            private_bytes: Some(priv_b), working_set: Some(priv_b), private_working_set: Some(priv_b / 2),
            ..Default::default()
        }
    }

    #[test]
    fn chrome_children_collapse_into_one_browser_row() {
        let c = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
        let procs = vec![
            p(10, 1, "chrome.exe", c, "chrome.exe", 100),
            p(11, 10, "chrome.exe", c, "chrome.exe --type=renderer", 200),
            p(12, 10, "chrome.exe", c, "chrome.exe --type=gpu-process", 300),
        ];
        let f = group(&procs);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FeatureKind::Browser);
        assert_eq!(f[0].private_bytes, 600);
        assert_eq!(f[0].pids.len(), 3);
    }

    #[test]
    fn svchost_services_get_one_row_each_group() {
        let mut s = p(20, 1, "svchost.exe", r"C:\Windows\System32\svchost.exe", "svchost.exe -k x", 50);
        s.services = vec!["WSearch".into()];
        let f = group(&[s]);
        assert_eq!(f[0].kind, FeatureKind::Service);
        assert_eq!(f[0].name, "WSearch");
    }

    #[test]
    fn wsl_vmmem_is_independent_feature() {
        let f = group(&[p(30, 4, "vmmemWSL", "", "", 900)]);
        assert_eq!(f[0].kind, FeatureKind::Wsl);
    }

    #[test]
    fn protected_names_need_full_path_match() {
        let real = p(1, 0, "lsass.exe", r"C:\Windows\System32\lsass.exe", "", 10);
        let fake = p(2, 0, "lsass.exe", r"C:\Users\x\AppData\lsass.exe", "", 10);
        let f = group(&[real, fake]);
        let real_f = f.iter().find(|x| x.pids == vec![1]).unwrap();
        let fake_f = f.iter().find(|x| x.pids == vec![2]).unwrap();
        assert_eq!(real_f.safety, SafetyClass::Protected);
        assert_ne!(fake_f.safety, SafetyClass::Protected);
    }

    #[test]
    fn unreadable_values_count_as_zero_not_panic() {
        let mut x = p(5, 0, "foo.exe", r"C:\a\foo.exe", "", 0);
        x.private_bytes = None; x.working_set = None; x.private_working_set = None;
        assert_eq!(group(&[x])[0].private_bytes, 0);
    }

    #[test]
    fn startup_mark_matches_full_path_and_respects_disabled() {
        let a = p(1, 0, "a.exe", r"C:\x\a.exe", "", 1);
        let b = p(2, 0, "b.exe", r"C:\x\b.exe", "", 1);
        let procs = vec![a, b];
        let mut f = group(&procs);
        let e = |path: &str, en| StartupEntry { name: "n".into(), exe_path: path.into(), machine_wide: false, user_sid: None, enabled: en };
        mark_startup(&mut f, &procs, &[e(r"c:\X\A.EXE", true), e(r"C:\x\b.exe", false)]);
        assert!(f.iter().find(|x| x.pids == vec![1]).unwrap().startup);
        assert!(!f.iter().find(|x| x.pids == vec![2]).unwrap().startup);
    }

    #[test]
    fn is_protected_covers_builtin_and_user_paths() {
        let real = p(1, 0, "lsass.exe", r"C:\Windows\System32\lsass.exe", "", 1);
        let fake = p(2, 0, "lsass.exe", r"C:\tmp\lsass.exe", "", 1);
        let obs = p(3, 0, "obs64.exe", r"C:\Program Files\OBS Studio\bin\64bit\obs64.exe", "", 1);
        let user = vec![r"c:\program files\obs studio\bin\64bit\OBS64.exe".to_string()];
        assert!(is_protected(&real, &[]));
        assert!(!is_protected(&fake, &[]));
        assert!(!is_protected(&obs, &[]));
        assert!(is_protected(&obs, &user));
    }

    #[test]
    fn protected_name_with_unreadable_path_is_treated_as_protected() {
        // パスを取得できない（権限不足）システムプロセスは、本物かどうか確認できないので保護側に倒す。
        let mut dwm = p(7, 0, "dwm.exe", "", "", 1);
        dwm.exe_path = None;
        assert_eq!(group(&[dwm])[0].safety, SafetyClass::Protected);
        // パスが分かっていて system32 以外なら、同名でも保護しない（偽装対策は従来どおり）。
        let fake = p(8, 0, "dwm.exe", r"C:\tmp\dwm.exe", "", 1);
        assert_ne!(group(&[fake])[0].safety, SafetyClass::Protected);
    }
}
