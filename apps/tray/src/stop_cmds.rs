//! 停止・復元コマンドの処理（接続スレッド内で実行）。正常終了は GUI セッション側で行い、結果をサービスへ返す。
use crate::graceful::{self, CloseResult};
use crate::service_link::View;
use memopt_core::ipc::Msg;
use memopt_core::stop::{JournalEntry, Outcome, StopMethod};
use memopt_ipc::Client;
use std::sync::Mutex;
use std::time::Duration;

/// サービス停止の待ち(最大 15 秒)＋解放後測定の待ちを含むので、通常の呼び出しより長い。
const STOP_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug)]
pub enum StopCmd {
    Plan(String),
    Execute { feature_id: String, method: StopMethod, target: Option<String>, confirmed: bool },
    Undo(u64),
    UndoAll,
    ClearPlan,
}

fn note(view: &Mutex<View>, s: impl Into<String>) {
    view.lock().unwrap().notice = Some(s.into());
}

/// 結果の説明文（ジャーナル記録を人が読める形に）。
pub fn describe_entry(e: &JournalEntry) -> String {
    let what = format!("{} / {}", e.feature_name, e.method.label());
    let freed = match (e.avail_before, e.avail_after) {
        (Some(a), Some(b)) => format!("（空き {} → {} MB）", a >> 20, b >> 20),
        _ => String::new(),
    };
    match &e.outcome {
        Outcome::Ok => format!("{what}: 完了{freed}"),
        Outcome::Failed(m) => format!("{what}: 失敗 — {m}"),
        Outcome::Requested => format!("{what}: 依頼しました"),
        Outcome::TimedOut => format!("{what}: タイムアウト"),
    }
}

/// 正常終了の依頼を受けたら、ここ（ユーザーセッション）で WM_CLOSE を送って結果をサービスへ報告する。
async fn graceful(c: &mut Client, view: &Mutex<View>, entry: &JournalEntry, targets: Vec<(u32, u64)>) -> Result<(), String> {
    note(view, format!("{}: 正常終了を依頼しました（最大 10 秒待ちます）…", entry.feature_name));
    let r = tokio::task::spawn_blocking(move || graceful::close(&targets, graceful::WAIT)).await.map_err(|e| e.to_string())?;
    c.call(Msg::FinishGraceful { journal_id: entry.id, ok: r == CloseResult::Closed }, Duration::from_secs(5)).await.map_err(|e| e.to_string())?;
    let (msg, retry_force) = match r {
        CloseResult::Closed => (format!("{}: 正常終了しました", entry.feature_name), false),
        // 自動で強制終了はしない。利用者に改めて確認する（設計書5章）。
        CloseResult::TimedOut => (format!("{}: 10 秒以内に終了しませんでした。強制終了する場合は確認してください", entry.feature_name), true),
        CloseResult::NoWindows => (format!("{}: 閉じられるウィンドウがありません。強制終了する場合は確認してください", entry.feature_name), true),
    };
    let mut v = view.lock().unwrap();
    v.notice = Some(msg);
    v.force_hint = retry_force.then(|| entry.feature_id.clone());
    Ok(())
}

pub async fn handle(c: &mut Client, view: &Mutex<View>, cmd: StopCmd) -> Result<(), String> {
    let io = |e: memopt_ipc::ClientError| e.to_string();
    match cmd {
        StopCmd::ClearPlan => {
            view.lock().unwrap().plan = None;
        }
        StopCmd::Plan(id) => match c.call(Msg::PlanStop { feature_id: id }, Duration::from_secs(30)).await.map_err(io)? {
            Msg::Plan { plan } => view.lock().unwrap().plan = Some(plan),
            Msg::Error { message, .. } => note(view, message),
            _ => {}
        },
        StopCmd::Execute { feature_id, method, target, confirmed } => {
            view.lock().unwrap().force_hint = None;
            match c.call(Msg::StopFeature { feature_id, method, target, confirmed }, STOP_TIMEOUT).await.map_err(io)? {
                Msg::StopResult { entry, graceful_targets } => {
                    view.lock().unwrap().plan = None;
                    if graceful_targets.is_empty() {
                        note(view, describe_entry(&entry));
                    } else {
                        graceful(c, view, &entry, graceful_targets).await?;
                    }
                    view.lock().unwrap().last_stop = Some(entry);
                }
                Msg::Error { message, .. } => note(view, message),
                _ => {}
            }
        }
        StopCmd::Undo(id) => match c.call(Msg::Undo { journal_id: id }, STOP_TIMEOUT).await.map_err(io)? {
            Msg::Ack => {
                let mut v = view.lock().unwrap();
                v.notice = Some("元に戻しました".into());
                if v.last_stop.as_ref().is_some_and(|e| e.id == id) {
                    v.last_stop = None;
                }
            }
            Msg::Error { message, .. } => note(view, message),
            _ => {}
        },
        StopCmd::UndoAll => match c.call(Msg::UndoAll, STOP_TIMEOUT).await.map_err(io)? {
            Msg::UndoDone { restored, failed } => {
                let mut v = view.lock().unwrap();
                v.last_stop = None;
                v.notice = Some(if failed.is_empty() {
                    format!("{} 件を元に戻しました", restored.len())
                } else {
                    format!("{} 件を元に戻し、{} 件は失敗しました: {}", restored.len(), failed.len(), failed[0].1)
                });
            }
            Msg::Error { message, .. } => note(view, message),
            _ => {}
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::stop::Change;

    fn entry(outcome: Outcome) -> JournalEntry {
        JournalEntry {
            id: 1, ts_unix: 0, feature_id: "f".into(), feature_name: "Chrome".into(), method: StopMethod::ForceKill, target: None,
            change: Change::None, outcome, avail_before: Some(100 << 20), avail_after: Some(300 << 20), undone_ts: None,
        }
    }

    #[test]
    fn describes_each_outcome_with_freed_memory_when_known() {
        assert_eq!(describe_entry(&entry(Outcome::Ok)), "Chrome / 強制終了: 完了（空き 100 → 300 MB）");
        assert!(describe_entry(&entry(Outcome::Failed("アクセス拒否".into()))).contains("アクセス拒否"));
        assert!(describe_entry(&entry(Outcome::TimedOut)).contains("タイムアウト"));
        let mut e = entry(Outcome::Requested);
        e.avail_before = None;
        assert_eq!(describe_entry(&e), "Chrome / 強制終了: 依頼しました");
    }
}
