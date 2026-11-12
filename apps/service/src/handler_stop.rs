//! 停止・復元・ジャーナル関連の IPC ハンドラ。
use crate::sync::{RwLockExt};
use crate::handler::ServiceHandler;
use crate::stopsvc;
use memopt_actions::exec::{self, Ctx};
use memopt_actions::plan::{authorize, authorize_caller, CallerInfo};
use memopt_core::ipc::{Frame, Msg};
use memopt_core::stop::{JournalEntry, Outcome, StopMethod};
use std::time::Duration;

const MAX_JOURNAL: u32 = 500;

/// この呼び出し元がその手段を実行してよいか（影響するプロセスの所有者まで確認する）。
fn check_caller(r: &stopsvc::Resolved, method: StopMethod, target: Option<&str>, who: &CallerInfo) -> Result<(), memopt_actions::plan::Deny> {
    let pids = match method {
        StopMethod::ForceKill => exec::kill_pids(&r.feature, &r.procs),
        _ => r.feature.pids.clone(),
    };
    authorize_caller(method, target, &r.feature, &|pid| memopt_probe::process_owner_sid(pid), &pids, who)
}

/// 計画の選択肢のうち、この呼び出し元には実行できないものを「使えない」にして理由を付ける
/// （画面でボタンが無効になり、押しても拒否される理由が分かる）。
fn restrict_plan(r: stopsvc::Resolved, who: &CallerInfo) -> memopt_core::stop::StopPlan {
    let mut plan = r.plan.clone();
    for o in plan.options.iter_mut() {
        if let Err(d) = check_caller(&r, o.method, o.target.as_deref(), who) {
            o.available = false;
            o.recommended = false;
            o.note = Some(d.text());
        }
    }
    plan
}
const AFTER_MEASURE_WAIT: Duration = Duration::from_millis(1500);

fn err(code: &str, message: impl Into<String>) -> Msg {
    Msg::Error { code: code.into(), message: message.into() }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn filetime_now() -> u64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + (d.subsec_nanos() / 100) as u64
}

impl ServiceHandler {
    fn broadcast(&self, msg: Msg) {
        if let Some(n) = self.notify.get() {
            let _ = n.send(Frame::new(None, msg));
        }
    }

    pub(crate) async fn plan_stop(&self, feature_id: String, caller: &CallerInfo) -> Msg {
        let s = self.shared.clone();
        let who = caller.clone();
        match tokio::task::spawn_blocking(move || stopsvc::resolve(&s, &feature_id).map(|r| restrict_plan(r, &who))).await {
            Ok(Ok(plan)) => Msg::Plan { plan },
            Ok(Err(m)) => err("not_found", m),
            Err(e) => err("internal", e.to_string()),
        }
    }

    pub(crate) async fn stop_feature(&self, feature_id: String, method: StopMethod, target: Option<String>, confirmed: bool, caller: &CallerInfo) -> Msg {
        let s = self.shared.clone();
        let who = caller.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<(JournalEntry, Vec<(u32, u64)>), Msg> {
            // 1 件ずつ。実行中に別の要求が来たら断る。
            let Ok(_one_at_a_time) = s.stop_lock.try_lock() else { return Err(err("busy", "別の停止操作を実行中です")) };
            let r = stopsvc::resolve(&s, &feature_id).map_err(|m| err("not_found", m))?;
            let opt = authorize(&r.plan, method, target.as_deref(), confirmed).map_err(|d| err("denied", d.text()))?.clone();
            // 計画の認可に加えて、呼び出し元の権限（標準ユーザーは自分のものだけ）を必ず確認する。
            check_caller(&r, method, opt.target.as_deref(), &who).map_err(|d| err("denied", d.text()))?;
            let avail_before = memopt_probe::memory_snapshot().map(|m| m.avail_phys);
            let user_protected = s.cfg.read_ok().protect.user_paths.clone();
            let ctx = Ctx {
                journal: &s.journal, now_unix: unix_now(), now_filetime: filetime_now(), avail_before,
                user_protected: &user_protected, actor_sid: who.user_sid.clone(),
            };
            let mut entry = exec::execute(&ctx, &r.feature, &r.procs, &opt).map_err(|m| err("internal", m))?;
            let graceful = if method == StopMethod::GracefulClose && entry.outcome == Outcome::Requested {
                r.procs.iter().filter(|p| r.feature.pids.contains(&p.pid)).map(|p| (p.pid, p.start_time)).collect()
            } else {
                // 結果が出る手段は、少し待って解放後の空きを記録する。
                std::thread::sleep(AFTER_MEASURE_WAIT);
                entry.avail_after = memopt_probe::memory_snapshot().map(|m| m.avail_phys);
                let _ = s.journal.set_outcome(entry.id, entry.outcome.clone(), entry.avail_after);
                vec![]
            };
            tracing::info!("停止 {} / {} ({:?}) -> {:?}", entry.feature_name, entry.method.label(), entry.target, entry.outcome);
            Ok((entry, graceful))
        })
        .await;
        match res {
            Ok(Ok((entry, graceful_targets))) => {
                self.broadcast(Msg::StopNotice { entry: entry.clone() });
                Msg::StopResult { entry, graceful_targets }
            }
            Ok(Err(m)) => m,
            Err(e) => err("internal", e.to_string()),
        }
    }

    /// GUI が行った正常終了の結果を記録する。依頼済み(Requested)の正常終了にだけ有効。
    pub(crate) fn finish_graceful(&self, id: u64, ok: bool, caller: &CallerInfo) -> Msg {
        let Some(e) = self.shared.journal.load().into_iter().find(|e| e.id == id) else { return err("not_found", "その記録はありません") };
        // 依頼した本人（または管理者）だけが結果を記録できる。
        if !caller.is_admin && (caller.user_sid.is_none() || e.actor_sid != caller.user_sid) {
            return err("denied", "他のユーザーの操作の結果は記録できません");
        }
        if e.method != StopMethod::GracefulClose || e.outcome != Outcome::Requested {
            return err("bad_request", "結果を記録できる状態ではありません");
        }
        let avail_after = memopt_probe::memory_snapshot().map(|m| m.avail_phys);
        let outcome = if ok { Outcome::Ok } else { Outcome::TimedOut };
        match self.shared.journal.set_outcome(id, outcome, avail_after) {
            Ok(()) => Msg::Ack,
            Err(e) => err("internal", e.to_string()),
        }
    }

    pub(crate) async fn undo(&self, id: u64, caller: &CallerInfo) -> Msg {
        let s = self.shared.clone();
        let who = caller.clone();
        let r = tokio::task::spawn_blocking(move || {
            let Ok(_g) = s.stop_lock.try_lock() else { return Err("別の停止/復元操作を実行中です".to_string()) };
            exec::undo_as(&s.journal, id, unix_now(), &who)
        })
        .await;
        match r {
            Ok(Ok(())) => {
                if let Some(e) = self.shared.journal.load().into_iter().find(|e| e.id == id) {
                    self.broadcast(Msg::StopNotice { entry: e });
                }
                Msg::Ack
            }
            Ok(Err(m)) => err("undo_failed", m),
            Err(e) => err("internal", e.to_string()),
        }
    }

    pub(crate) async fn undo_all(&self, caller: &CallerInfo) -> Msg {
        let s = self.shared.clone();
        let who = caller.clone();
        let r = tokio::task::spawn_blocking(move || {
            let Ok(_g) = s.stop_lock.try_lock() else { return Err("別の停止/復元操作を実行中です".to_string()) };
            Ok(exec::undo_all_as(&s.journal, unix_now(), &who))
        })
        .await;
        match r {
            Ok(Ok(rep)) => Msg::UndoDone { restored: rep.restored, failed: rep.failed },
            Ok(Err(m)) => err("busy", m),
            Err(e) => err("internal", e.to_string()),
        }
    }

    pub(crate) fn get_journal(&self, limit: u32, caller: &CallerInfo) -> Msg {
        let mut v = self.shared.journal.load();
        if !caller.is_admin {
            // 標準ユーザーには自分が行った操作だけを見せる（他のユーザーの操作は見せない）。
            v.retain(|e| caller.user_sid.is_some() && e.actor_sid == caller.user_sid);
        }
        v.reverse(); // 新しい順
        v.truncate(limit.clamp(1, MAX_JOURNAL) as usize);
        Msg::Journal { entries: v }
    }
}
