//! 機能の停止（設計書5章）: 停止手段・計画・ジャーナル。
use crate::SafetyClass;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StopMethod {
    /// ウィンドウを持つアプリへ WM_CLOSE（GUI 側で実行、最大 10 秒待つ）。今回のみ。
    GracefulClose,
    /// プロセスツリーを TerminateProcess。今回のみ・未保存データは失われる。
    ForceKill,
    /// ControlService(STOP)。今回のみ（再起動で戻る）。
    ServiceStop,
    /// スタートアップ種別を「手動」へ（永続）。
    ServiceStartupManual,
    /// スタートアップ種別を「無効」へ（永続）。
    ServiceStartupDisabled,
    /// StartupApproved を無効に書き換える（登録自体は消さない・永続）。
    DisableStartup,
    /// スケジュールタスクを無効化（永続）。
    DisableTask,
    /// メモリ優先度を下げる（今回のみ）。停止の代替。
    LowerPriority,
}

impl StopMethod {
    /// 次回以降にも影響する変更か（2 段階確認が必要）。
    pub fn persistent(self) -> bool {
        matches!(
            self,
            Self::ServiceStartupManual | Self::ServiceStartupDisabled | Self::DisableStartup | Self::DisableTask
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::GracefulClose => "正常終了",
            Self::ForceKill => "強制終了",
            Self::ServiceStop => "サービス停止（今回のみ）",
            Self::ServiceStartupManual => "スタートアップを手動に変更",
            Self::ServiceStartupDisabled => "スタートアップを無効に変更",
            Self::DisableStartup => "次回から自動起動しない",
            Self::DisableTask => "スケジュールタスクを無効化",
            Self::LowerPriority => "メモリ優先度を下げる",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartupRef {
    pub name: String,
    /// None = マシン全体 (HKLM)、Some(sid) = そのユーザーの HKU。
    pub user_sid: Option<String>,
}

/// 機能に紐づく停止対象（サービス再スキャンのたびに組み立てる）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StopTargets {
    pub services: Vec<String>,
    pub startup: Vec<StartupRef>,
    pub tasks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorityItem {
    pub pid: u32,
    pub start_time: u64,
    pub before: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MethodOption {
    pub method: StopMethod,
    /// サービス名・スタートアップ名・タスクパスなど、手段の対象（単一対象なら自動で入る）。
    pub target: Option<String>,
    pub persistent: bool,
    pub available: bool,
    /// 使えない理由 / 注意。
    pub note: Option<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopPlan {
    pub feature_id: String,
    pub name: String,
    pub safety: SafetyClass,
    pub process_count: usize,
    /// 停止後に空く見込み（プライベートワーキングセットの上限値）。
    pub est_release_bytes: u64,
    pub dependents: Vec<String>,
    pub options: Vec<MethodOption>,
    /// 影響の説明（確認ダイアログに出す）。
    pub impacts: Vec<String>,
}

/// 復元に必要な「操作前の値」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Change {
    /// 元に戻せない（強制終了/正常終了）。ユーザーが再起動する。
    None,
    /// 先に止めた依存サービス(`dependents`)も、復元時に本体の後で起動し直す。
    ServiceStopped { name: String, #[serde(default)] dependents: Vec<String> },
    ServiceStartType { name: String, before: u32, delayed_before: bool },
    StartupApproved { user_sid: Option<String>, value_name: String, before_raw: Option<Vec<u8>> },
    TaskEnabled { path: String, before_enabled: bool },
    /// 機能内の複数プロセスの変更前メモリ優先度。
    MemoryPriority { items: Vec<PriorityItem> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Ok,
    Failed(String),
    /// 正常終了を依頼済み（結果待ち）。
    Requested,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: u64,
    pub ts_unix: i64,
    pub feature_id: String,
    pub feature_name: String,
    pub method: StopMethod,
    pub target: Option<String>,
    pub change: Change,
    pub outcome: Outcome,
    pub avail_before: Option<u64>,
    pub avail_after: Option<u64>,
    /// 復元済みなら復元時刻。
    #[serde(default)]
    pub undone_ts: Option<i64>,
    /// 操作を要求したユーザーの SID（パイプの接続元から OS が取った値）。標準ユーザーは自分の記録だけを見て、戻せる。
    #[serde(default)]
    pub actor_sid: Option<String>,
}

impl JournalEntry {
    /// 永続変更で、まだ復元されていないか。
    pub fn pending_persistent(&self) -> bool {
        self.undone_ts.is_none() && self.outcome == Outcome::Ok && self.method.persistent()
    }

    /// 「元に戻す」ボタンを出せるか（復元手段があり、未復元で、成功している）。
    pub fn undoable(&self) -> bool {
        self.undone_ts.is_none() && self.outcome == Outcome::Ok && !matches!(self.change, Change::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_startup_and_task_and_service_config_changes_are_persistent() {
        use StopMethod::*;
        for m in [ServiceStartupManual, ServiceStartupDisabled, DisableStartup, DisableTask] {
            assert!(m.persistent(), "{m:?}");
        }
        for m in [GracefulClose, ForceKill, ServiceStop, LowerPriority] {
            assert!(!m.persistent(), "{m:?}");
        }
    }

    fn entry(method: StopMethod, change: Change, outcome: Outcome) -> JournalEntry {
        JournalEntry {
            id: 1, ts_unix: 0, feature_id: "f".into(), feature_name: "F".into(), method, target: None, change,
            outcome, avail_before: None, avail_after: None, undone_ts: None, actor_sid: None,
        }
    }

    #[test]
    fn undoable_requires_success_a_restore_action_and_not_undone() {
        let svc = Change::ServiceStopped { name: "x".into(), dependents: vec![] };
        assert!(entry(StopMethod::ServiceStop, svc.clone(), Outcome::Ok).undoable());
        assert!(!entry(StopMethod::ServiceStop, svc.clone(), Outcome::Failed("e".into())).undoable());
        assert!(!entry(StopMethod::ForceKill, Change::None, Outcome::Ok).undoable());
        let mut done = entry(StopMethod::ServiceStop, svc, Outcome::Ok);
        done.undone_ts = Some(5);
        assert!(!done.undoable());
    }

    #[test]
    fn pending_persistent_excludes_transient_and_undone() {
        let c = Change::TaskEnabled { path: "\\t".into(), before_enabled: true };
        assert!(entry(StopMethod::DisableTask, c.clone(), Outcome::Ok).pending_persistent());
        assert!(!entry(StopMethod::ServiceStop, Change::ServiceStopped { name: "s".into(), dependents: vec![] }, Outcome::Ok).pending_persistent());
        let mut u = entry(StopMethod::DisableTask, c, Outcome::Ok);
        u.undone_ts = Some(1);
        assert!(!u.pending_persistent());
    }

    #[test]
    fn journal_entry_json_roundtrips_with_binary_before_value() {
        let e = entry(
            StopMethod::DisableStartup,
            Change::StartupApproved { user_sid: Some("S-1-5-21-1".into()), value_name: "App".into(), before_raw: Some(vec![2, 0, 0, 0, 1, 2]) },
            Outcome::Ok,
        );
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<JournalEntry>(&s).unwrap(), e);
    }
}

/// レジストリのルート。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegRoot {
    LocalMachine,
    Users,
}

const RUN_REL: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_REL: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Run / StartupApproved の場所。ユーザー分は HKEY_USERS\<SID> 配下（サービスは SYSTEM のため HKCU を使えない）。
/// SID に区切り文字などが混ざっていたら None（別キーへの書き込みに悪用されないよう検証する）。
pub fn startup_key(user_sid: Option<&str>, approved: bool) -> Option<(RegRoot, String)> {
    let rel = if approved { APPROVED_REL } else { RUN_REL };
    match user_sid {
        None => Some((RegRoot::LocalMachine, rel.to_string())),
        Some(sid) => {
            let ok = sid.starts_with("S-1-5-21-")
                && !sid.ends_with("_Classes")
                && sid.chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'S');
            ok.then(|| (RegRoot::Users, format!(r"{sid}\{rel}")))
        }
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn machine_and_user_paths() {
        let (r, p) = startup_key(None, false).unwrap();
        assert_eq!(r, RegRoot::LocalMachine);
        assert!(p.ends_with(r"CurrentVersion\Run"));
        let (r, p) = startup_key(Some("S-1-5-21-1-2-3-1001"), true).unwrap();
        assert_eq!(r, RegRoot::Users);
        assert!(p.starts_with(r"S-1-5-21-1-2-3-1001\Software") && p.ends_with(r"StartupApproved\Run"));
    }

    #[test]
    fn malformed_or_classes_sids_are_rejected() {
        for bad in ["", r"S-1-5-21-1\..\x", "S-1-5-18", "S-1-5-21-1_Classes", ".DEFAULT", "S-1-5-21-1/2"] {
            assert_eq!(startup_key(Some(bad), true), None, "{bad}");
        }
    }
}
