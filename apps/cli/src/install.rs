//! サービスの登録/削除（設計書6章: SCM の回復設定は「1 回だけ再起動、2 回目以降は停止のまま」）。
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows_service::service::*;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub const SERVICE_NAME: &str = "MemOptService";
const DISPLAY_NAME: &str = "MemOpt メモリ最適化サービス";
const DESCRIPTION: &str = "メモリ圧迫を検知したときだけ段階的に解放し、効果を測定します。";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "MemOpt";

/// CLI と同じフォルダーにある実行ファイルのパス。
pub fn sibling_exe(cli: &Path, name: &str) -> PathBuf {
    cli.with_file_name(name)
}

/// Run キーに書くコマンド（空白入りのパスでも壊れないよう引用符で囲む）。
pub fn run_value(tray_exe: &Path) -> String {
    format!("\"{}\"", tray_exe.display())
}

/// 回復設定: 1 回目の失敗で 60 秒後に再起動、2 回目以降は何もしない。24 時間で失敗回数をリセット。
pub fn failure_actions() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(60) },
            ServiceAction { action_type: ServiceActionType::None, delay: Duration::from_secs(0) },
        ]),
    }
}

fn exe_or_err(p: PathBuf) -> Result<PathBuf, String> {
    p.exists().then_some(p.clone()).ok_or(format!("{} が見つかりません（先にビルドしてください）", p.display()))
}

pub fn install() -> Result<(), String> {
    if !memopt_cleaner::is_elevated() {
        return Err("管理者権限が必要です（管理者として実行した PowerShell で実行してください）".into());
    }
    let cli = std::env::current_exe().map_err(|e| e.to_string())?;
    let svc_exe = exe_or_err(sibling_exe(&cli, "memopt-service.exe"))?;
    let tray_exe = exe_or_err(sibling_exe(&cli, "memopt-tray.exe"))?;

    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).map_err(|e| e.to_string())?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: svc_exe,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = mgr
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .map_err(|e| format!("サービスを作成できません（すでに登録済みの場合は uninstall を先に）: {e}"))?;
    let _ = svc.set_description(DESCRIPTION);
    svc.update_failure_actions(failure_actions()).map_err(|e| format!("回復設定: {e}"))?;
    svc.start::<&str>(&[]).map_err(|e| format!("サービスを開始できません: {e}"))?;
    println!("サービス {SERVICE_NAME} を登録して開始しました");

    set_autostart(&tray_exe)?;
    println!("トレイのログオン時自動起動を登録しました（次回ログオンから）");
    Ok(())
}

/// 未復元の永続変更があれば一覧を示し、復元するか確認する（設計書5章）。
fn offer_restore(restore: Option<bool>) -> Result<(), String> {
    let j = memopt_actions::journal::Journal::open(&crate::undo::default_journal());
    let n = {
        let pending = j.pending_persistent();
        if pending.is_empty() {
            return Ok(());
        }
        println!("本ツールが加えた未復元の永続変更が {} 件あります:", pending.len());
        crate::undo::print_pending(&j)
    };
    let yes = match restore {
        Some(v) => v,
        None => {
            use std::io::{BufRead, Write};
            print!("{n} 件を元に戻しますか？ [y/N] ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }
    };
    if yes {
        crate::undo::undo(true, None, None)?;
    } else {
        println!("復元しませんでした。後から `memopt-cli undo --all` で戻せます（ジャーナルは残します）");
    }
    Ok(())
}

pub fn uninstall(restore: Option<bool>) -> Result<(), String> {
    if !memopt_cleaner::is_elevated() {
        return Err("管理者権限が必要です".into());
    }
    offer_restore(restore)?;
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| e.to_string())?;
    match mgr.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS) {
        Ok(svc) => {
            let _ = svc.stop();
            for _ in 0..20 {
                if svc.query_status().map(|s| s.current_state == ServiceState::Stopped).unwrap_or(true) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            svc.delete().map_err(|e| format!("サービスを削除できません: {e}"))?;
            println!("サービスを削除しました");
        }
        Err(_) => println!("サービスは登録されていません"),
    }
    clear_autostart();
    println!("設定とジャーナルは %ProgramData%\\MemOpt に残しています（復元に必要なため削除しません）");
    Ok(())
}

fn set_autostart(tray_exe: &Path) -> Result<(), String> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::*;
    let value: Vec<u16> = run_value(tray_exe).encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut key = HKEY::default();
        let sub: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_SET_VALUE, &mut key).0 != 0 {
            return Err("Run キーを開けません".into());
        }
        let name: Vec<u16> = RUN_VALUE.encode_utf16().chain(Some(0)).collect();
        let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
        let rc = RegSetValueExW(key, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes));
        let _ = RegCloseKey(key);
        let _ = w!("");
        if rc.0 != 0 {
            return Err(format!("Run キーへ書き込めません: {}", rc.0));
        }
    }
    Ok(())
}

fn clear_autostart() {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::*;
    unsafe {
        let mut key = HKEY::default();
        let sub: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_SET_VALUE, &mut key).0 == 0 {
            let name: Vec<u16> = RUN_VALUE.encode_utf16().chain(Some(0)).collect();
            let _ = RegDeleteValueW(key, PCWSTR(name.as_ptr()));
            let _ = RegCloseKey(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_exe_replaces_file_name_only() {
        let p = sibling_exe(Path::new(r"C:\Program Files\MemOpt\memopt-cli.exe"), "memopt-service.exe");
        assert_eq!(p, PathBuf::from(r"C:\Program Files\MemOpt\memopt-service.exe"));
    }

    #[test]
    fn run_value_quotes_paths_with_spaces() {
        assert_eq!(run_value(Path::new(r"C:\Program Files\MemOpt\memopt-tray.exe")), r#""C:\Program Files\MemOpt\memopt-tray.exe""#);
    }

    #[test]
    fn failure_actions_restart_once_then_stay_stopped() {
        let a = failure_actions().actions.unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].action_type, ServiceActionType::Restart);
        assert_eq!(a[1].action_type, ServiceActionType::None);
    }

    #[test]
    fn missing_binary_is_reported_with_path() {
        let e = exe_or_err(PathBuf::from(r"C:\definitely\missing\x.exe")).unwrap_err();
        assert!(e.contains("missing"));
    }
}
