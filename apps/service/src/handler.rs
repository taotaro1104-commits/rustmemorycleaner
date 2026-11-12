//! IPC 要求のハンドラ。パイプ越しの内容はここで必ず再検証する（設計書6章）。
use crate::sync::{LockExt, RwLockExt};
use crate::runner::RunRequest;
use crate::state::{Shared, FEATURES_TTL_SECS, MAX_PAUSE_SECS};
use crate::config_io;
use memopt_core::ipc::{Msg, RunSummary};
use memopt_core::Trigger;
use memopt_actions::plan::CallerInfo;
use memopt_ipc::{BoxFut, Caller, Handler};
use memopt_store::RunRecord;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

const MAX_HISTORY: u32 = 200;
const MAX_AUDIO_PIDS: usize = 512;

pub struct ServiceHandler {
    pub shared: Arc<Shared>,
    pub run_tx: mpsc::Sender<RunRequest>,
    /// サーバー作成後にセットされる通知用チャンネル。
    pub notify: std::sync::OnceLock<tokio::sync::broadcast::Sender<memopt_core::ipc::Frame>>,
}

fn err(code: &str, message: impl Into<String>) -> Msg {
    Msg::Error { code: code.into(), message: message.into() }
}

pub fn summarize(r: &RunRecord) -> RunSummary {
    RunSummary {
        id: r.id,
        ts_unix: r.ts_unix,
        level: r.level,
        trigger: r.trigger.clone(),
        avail_before: r.before.avail_phys,
        avail_after: r.after.avail_phys,
        standby_before: r.before.standby,
        standby_after: r.after.standby,
        avail_after60: r.after60.map(|m| m.avail_phys),
        note: r.note.clone(),
    }
}

impl ServiceHandler {
    fn snapshot(&self) -> Msg {
        let s = &self.shared;
        let mem = memopt_probe::memory_snapshot().or(*s.mem.lock_ok()).unwrap_or_default();
        // エンジンのロックは 1 回だけ取る。同じ式の中で 2 回 lock() すると、最初のガードが式の終わりまで
        // 生きているためデッドロックする（std の Mutex は再入不可）。
        let (emergency_stop, adaptive) = {
            let mut e = s.engine.lock_ok();
            (e.emergency_stopped(), e.adaptive_notes(s.now_secs()))
        };
        Msg::Snapshot {
            mem,
            auto_enabled: s.cfg.read_ok().auto_clean.enabled,
            paused: s.is_paused(),
            emergency_stop,
            adaptive,
        }
    }

    async fn features(&self) -> Msg {
        {
            let g = self.shared.features.lock_ok();
            if let Some((at, f)) = g.as_ref() {
                if at.elapsed().as_secs() < FEATURES_TTL_SECS {
                    return Msg::Features { features: f.clone() };
                }
            }
        }
        // プロセス列挙は重いのでブロッキングスレッドで行い、結果を短時間キャッシュする。
        let sh = self.shared.clone();
        let f = tokio::task::spawn_blocking(move || crate::stopsvc::list_features(&sh)).await;
        match f {
            Ok(features) => {
                *self.shared.features.lock_ok() = Some((Instant::now(), features.clone()));
                Msg::Features { features }
            }
            Err(e) => err("internal", e.to_string()),
        }
    }

    fn run_clean(&self, level: memopt_core::Level) -> Msg {
        if self.shared.running.load(Ordering::SeqCst) {
            return err("busy", "別の解放を実行中です");
        }
        match self.run_tx.try_send(RunRequest { trigger: Trigger::Manual, levels: vec![level] }) {
            Ok(()) => Msg::Ack,
            Err(_) => err("busy", "解放要求が混み合っています"),
        }
    }

    fn update_config(&self, text: &str) -> Msg {
        match config_io::apply_update(&self.shared.config_path, text) {
            Ok((cfg, warnings)) => {
                self.shared.engine.lock_ok().update_config(&cfg);
                let toml = toml::to_string_pretty(&cfg).unwrap_or_default();
                *self.shared.cfg.write_ok() = cfg;
                Msg::Config { toml, warnings }
            }
            Err(e) => err("bad_config", e),
        }
    }

    async fn dispatch(&self, req: Msg, caller: &CallerInfo) -> Msg {
        match req {
            Msg::GetSnapshot => self.snapshot(),
            Msg::GetFeatures => self.features().await,
            Msg::GetHistory { limit } => match self.shared.store.lock_ok().list_runs(limit.clamp(1, MAX_HISTORY) as usize) {
                Ok(v) => Msg::History { runs: v.iter().map(summarize).collect() },
                Err(e) => err("internal", e.to_string()),
            },
            Msg::RunClean { level } => self.run_clean(level),
            Msg::ReportSession(mut s) => {
                s.audio_pids.truncate(MAX_AUDIO_PIDS);
                *self.shared.session.lock_ok() = Some((s, Instant::now()));
                Msg::Ack
            }
            Msg::SetPaused { secs } => {
                self.shared.set_paused(secs.min(MAX_PAUSE_SECS));
                Msg::Ack
            }
            Msg::EmergencyStop { on } => {
                self.shared.engine.lock_ok().set_emergency_stop(on);
                tracing::warn!("緊急停止 {}", if on { "ON" } else { "OFF" });
                Msg::Ack
            }
            Msg::GetConfig => {
                let cfg = self.shared.cfg.read_ok().clone();
                Msg::Config { toml: toml::to_string_pretty(&cfg).unwrap_or_default(), warnings: vec![] }
            }
            Msg::UpdateConfig { toml } => self.update_config(&toml),
            Msg::PlanStop { feature_id } => self.plan_stop(feature_id, caller).await,
            Msg::StopFeature { feature_id, method, target, confirmed } => self.stop_feature(feature_id, method, target, confirmed, caller).await,
            Msg::FinishGraceful { journal_id, ok } => self.finish_graceful(journal_id, ok, caller),
            Msg::Undo { journal_id } => self.undo(journal_id, caller).await,
            Msg::UndoAll => self.undo_all(caller).await,
            Msg::GetJournal { limit } => self.get_journal(limit, caller),
            Msg::ResetAdaptive => {
                self.shared.engine.lock_ok().reset_adaptive();
                tracing::info!("自動調整を手動で解除しました");
                Msg::Ack
            }
            // 応答・通知の種類をクライアントが送ってきた場合は拒否する。
            _ => err("unsupported", "サービスが受け付けない種類のメッセージです"),
        }
    }
}

impl ServiceHandler {
    fn notify_clone(&self) -> std::sync::OnceLock<tokio::sync::broadcast::Sender<memopt_core::ipc::Frame>> {
        let c = std::sync::OnceLock::new();
        if let Some(n) = self.notify.get() {
            let _ = c.set(n.clone());
        }
        c
    }
}

impl Handler for ServiceHandler {
    fn handle(&self, req: Msg, caller: &Caller) -> BoxFut {
        // dispatch は &self を借りるので、必要な Arc を複製して 'static にする。
        let me = ServiceHandler { shared: self.shared.clone(), run_tx: self.run_tx.clone(), notify: self.notify_clone() };
        // OS が保証する接続元の身元。要求の内容には書かせない。
        let who = CallerInfo { is_admin: caller.is_admin, user_sid: caller.user_sid.clone() };
        Box::pin(async move { me.dispatch(req, &who).await })
    }
}
