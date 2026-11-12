//! タスクスケジューラ（COM）。タスクの列挙（実行ファイルとの対応）と有効/無効の切り替え。
use windows::core::{Interface, BSTR, PCWSTR, VARIANT};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::*;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::TaskScheduler::*;

#[derive(Debug, Clone, PartialEq)]
pub struct TaskInfo {
    pub path: String,
    /// 最初の「プログラムの開始」アクションの実行ファイル（環境変数展開済み・引用符なし）。
    pub exe_path: Option<String>,
    pub enabled: bool,
}

/// COM の初期化を対にする。すでに別モードで初期化済みでも続行する。
struct Com(bool);
impl Com {
    fn new() -> Self {
        Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() })
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn service() -> Result<ITaskService, String> {
    unsafe {
        let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).map_err(|e| format!("タスクスケジューラ: {e}"))?;
        let empty = VARIANT::default();
        svc.Connect(&empty, &empty, &empty, &empty).map_err(|e| format!("タスクスケジューラへ接続できません: {e}"))?;
        Ok(svc)
    }
}

fn expand(s: &str) -> String {
    let src: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 2048];
    let n = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) } as usize;
    if n == 0 || n > buf.len() { s.to_string() } else { String::from_utf16_lossy(&buf[..n - 1]) }
}

/// アクションのパス文字列を実行ファイルのパスに整える（引用符除去・環境変数展開）。
pub fn normalize_exe(raw: &str) -> Option<String> {
    let t = expand(raw.trim()).trim_matches('"').trim().to_string();
    (!t.is_empty()).then_some(t)
}

unsafe fn exec_path(task: &IRegisteredTask) -> Option<String> {
    let actions = task.Definition().ok()?.Actions().ok()?;
    let mut n = 0i32;
    actions.Count(&mut n).ok()?;
    for i in 1..=n {
        let a = actions.get_Item(i).ok()?;
        let mut ty = TASK_ACTION_TYPE::default();
        a.Type(&mut ty).ok()?;
        if ty == TASK_ACTION_EXEC {
            let e: IExecAction = a.cast().ok()?;
            let mut path = BSTR::new();
            e.Path(&mut path).ok()?;
            return normalize_exe(&path.to_string());
        }
    }
    None
}

unsafe fn walk(folder: &ITaskFolder, out: &mut Vec<TaskInfo>) {
    if let Ok(tasks) = folder.GetTasks(TASK_ENUM_HIDDEN.0) {
        for i in 1..=tasks.Count().unwrap_or(0) {
            let Ok(t) = tasks.get_Item(&VARIANT::from(i)) else { continue };
            let (Ok(path), Ok(en)) = (t.Path(), t.Enabled()) else { continue };
            out.push(TaskInfo { path: path.to_string(), exe_path: exec_path(&t), enabled: en.as_bool() });
        }
    }
    if let Ok(subs) = folder.GetFolders(0) {
        for i in 1..=subs.Count().unwrap_or(0) {
            if let Ok(f) = subs.get_Item(&VARIANT::from(i)) {
                walk(&f, out);
            }
        }
    }
}

/// 全タスク（隠しタスク含む）。件数が多いので呼び出し側でキャッシュすること。
pub fn list_tasks() -> Result<Vec<TaskInfo>, String> {
    let _com = Com::new();
    let svc = service()?;
    let mut out = Vec::new();
    unsafe {
        let root = svc.GetFolder(&BSTR::from("\\")).map_err(|e| e.to_string())?;
        walk(&root, &mut out);
    }
    Ok(out)
}

fn get_task(svc: &ITaskService, path: &str) -> Result<IRegisteredTask, String> {
    unsafe {
        let root = svc.GetFolder(&BSTR::from("\\")).map_err(|e| e.to_string())?;
        root.GetTask(&BSTR::from(path)).map_err(|e| format!("タスク {path} を開けません: {e}"))
    }
}

/// 有効/無効を設定し、変更前の状態を返す。
pub fn set_enabled(path: &str, enabled: bool) -> Result<bool, String> {
    let _com = Com::new();
    let svc = service()?;
    let t = get_task(&svc, path)?;
    unsafe {
        let before = t.Enabled().map_err(|e| e.to_string())?.as_bool();
        t.SetEnabled(VARIANT_BOOL::from(enabled)).map_err(|e| format!("タスクの有効/無効を変更できません: {e}"))?;
        Ok(before)
    }
}

pub fn is_enabled(path: &str) -> Result<bool, String> {
    let _com = Com::new();
    let svc = service()?;
    let t = get_task(&svc, path)?;
    unsafe { t.Enabled().map(|v| v.as_bool()).map_err(|e| e.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_quotes_and_expands_env() {
        assert_eq!(normalize_exe(r#""C:\Program Files\A\a.exe""#).as_deref(), Some(r"C:\Program Files\A\a.exe"));
        let win = std::env::var("windir").unwrap();
        assert_eq!(normalize_exe(r"%windir%\system32\x.exe").as_deref(), Some(format!(r"{win}\system32\x.exe").as_str()));
        assert_eq!(normalize_exe("  "), None);
    }

    #[test]
    fn lists_tasks_with_paths_and_some_exec_actions() {
        let tasks = list_tasks().expect("タスク一覧");
        assert!(tasks.len() > 5, "Windows には多数の標準タスクがある");
        assert!(tasks.iter().all(|t| t.path.starts_with('\\')));
        assert!(tasks.iter().any(|t| t.exe_path.is_some()));
    }

    #[test]
    fn unknown_task_is_an_error() {
        assert!(is_enabled(r"\MemOpt\NoSuchTask").is_err());
        assert!(set_enabled(r"\MemOpt\NoSuchTask", false).is_err());
    }

    /// 使い捨てのユーザータスクで 無効化 → 復元 の往復を確認する（schtasks で作成・削除）。
    #[test]
    fn disable_and_restore_roundtrip_on_a_throwaway_task() {
        use std::process::{Command, Stdio};
        let name = format!(r"MemOptTest\T{}", std::process::id());
        let made = Command::new("schtasks")
            .args(["/Create", "/TN", &name, "/TR", "cmd.exe /c exit", "/SC", "ONCE", "/ST", "23:59", "/F"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        assert!(made, "schtasks でテスト用タスクを作れませんでした");
        let path = format!(r"\{name}");
        let r = (|| -> Result<(), String> {
            assert!(is_enabled(&path)?);
            let before = set_enabled(&path, false)?;
            assert!(before);
            assert!(!is_enabled(&path)?);
            // 一覧にも載り、実行ファイルが対応付く
            let listed = list_tasks()?.into_iter().find(|t| t.path.eq_ignore_ascii_case(&path)).ok_or("一覧に無い")?;
            assert!(!listed.enabled);
            assert!(listed.exe_path.as_deref().is_some_and(|p| p.to_ascii_lowercase().contains("cmd.exe")));
            set_enabled(&path, before)?;
            assert!(is_enabled(&path)?);
            Ok(())
        })();
        let _ = Command::new("schtasks").args(["/Delete", "/TN", &name, "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        r.unwrap();
    }
}
