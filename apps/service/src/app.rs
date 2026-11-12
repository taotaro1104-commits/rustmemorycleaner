//! サービス本体の組み立て: 設定読み込み → 共有状態 → 解放ランナー/評価ループ/IPC サーバーを起動。
use crate::evalloop::eval_loop;
use crate::handler::ServiceHandler;
use crate::runner::run_loop;
use crate::state::Shared;
use crate::{config_io, paths};
use memopt_ipc::Server;
use memopt_store::Store;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

/// 停止シグナル(`stop` が true)まで動き続ける。パイプ名は本番では `memopt_core::ipc::PIPE_NAME`。
pub async fn serve(data_dir: &Path, pipe: &str, mut stop: watch::Receiver<bool>) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;

    let cfg_path = data_dir.join("config.toml");
    let (cfg, warnings) = config_io::load(&cfg_path);
    for w in &warnings {
        tracing::warn!("config: {w}");
    }
    let store = Store::open(&data_dir.join("history.db")).map_err(|e| format!("history.db: {e}"))?;
    let journal = memopt_actions::journal::Journal::open(&data_dir.join("journal.jsonl"));
    let shared = Arc::new(Shared::new(cfg_path, cfg, store, journal));

    let (run_tx, run_rx) = mpsc::channel(4);
    let handler = Arc::new(ServiceHandler { shared: shared.clone(), run_tx: run_tx.clone(), notify: Default::default() });
    let server = Arc::new(Server::new(pipe, handler.clone()).map_err(|e| format!("パイプを作れません: {e}"))?);
    let notify = server.notifier();
    let _ = handler.notify.set(notify.clone());

    let runner = tokio::spawn(run_loop(shared.clone(), run_rx, notify.clone()));
    let evals = tokio::spawn(eval_loop(shared.clone(), run_tx, notify, stop.clone()));
    let (srv_stop_tx, srv_stop_rx) = watch::channel(false);
    let srv = tokio::spawn(server.run(srv_stop_rx));

    tracing::info!("サービス開始 data={} pipe={pipe}", data_dir.display());
    // 停止要求を待つ。サーバーが先に落ちた場合（パイプの作成失敗など）はエラーとして返す。
    let result = tokio::select! {
        _ = async { while !*stop.borrow() { if stop.changed().await.is_err() { break; } } } => Ok(()),
        r = srv => match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("IPC サーバーが停止: {e}")),
            Err(e) => Err(format!("IPC タスクが異常終了: {e}")),
        },
    };
    let _ = srv_stop_tx.send(true);
    let _ = evals.await;
    runner.abort();
    tracing::info!("サービス停止");
    result
}

pub fn default_data_dir() -> std::path::PathBuf {
    paths::data_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::ipc::Msg;
    use memopt_ipc::Client;
    use std::time::Duration;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("memopt_svc_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    async fn start(tag: &str) -> (watch::Sender<bool>, tokio::task::JoinHandle<Result<(), String>>, String, std::path::PathBuf) {
        let dir = tmpdir(tag);
        let pipe = format!(r"\\.\pipe\memopt-svc-test-{tag}-{}", std::process::id());
        let (tx, rx) = watch::channel(false);
        let (d, p) = (dir.clone(), pipe.clone());
        let h = tokio::spawn(async move { serve(&d, &p, rx).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        (tx, h, pipe, dir)
    }

    #[tokio::test]
    async fn snapshot_config_and_flags_over_the_pipe() {
        let (stop, h, pipe, dir) = start("basic").await;
        let mut c = Client::connect(&pipe).unwrap();
        let t = Duration::from_secs(3);

        let Msg::Snapshot { mem, paused, emergency_stop, .. } = c.call(Msg::GetSnapshot, t).await.unwrap() else { panic!() };
        assert!(mem.total_phys > 0 && !paused && !emergency_stop);

        assert_eq!(c.call(Msg::EmergencyStop { on: true }, t).await.unwrap(), Msg::Ack);
        assert_eq!(c.call(Msg::SetPaused { secs: 60 }, t).await.unwrap(), Msg::Ack);
        let Msg::Snapshot { paused, emergency_stop, .. } = c.call(Msg::GetSnapshot, t).await.unwrap() else { panic!() };
        assert!(paused && emergency_stop);

        // 不正な設定値は補正され、警告つきで保存される。壊れた TOML は拒否される。
        let Msg::Config { warnings, .. } = c.call(Msg::UpdateConfig { toml: "[l1]\nmax_processes = 0".into() }, t).await.unwrap() else { panic!() };
        assert_eq!(warnings.len(), 1);
        assert!(dir.join("config.toml").exists());
        let r = c.call(Msg::UpdateConfig { toml: "= = [".into() }, t).await.unwrap();
        assert!(matches!(r, Msg::Error { ref code, .. } if code == "bad_config"));

        let _ = stop.send(true);
        assert!(h.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn features_are_served_and_response_kinds_are_rejected() {
        let (stop, h, pipe, _dir) = start("feat").await;
        let mut c = Client::connect(&pipe).unwrap();
        let t = Duration::from_secs(10);
        let Msg::Features { features } = c.call(Msg::GetFeatures, t).await.unwrap() else { panic!() };
        assert!(!features.is_empty());
        // クライアントが「応答」種別のメッセージを送ってきても受け付けない。
        let r = c.call(Msg::Ack, t).await.unwrap();
        assert!(matches!(r, Msg::Error { ref code, .. } if code == "unsupported"));
        let _ = stop.send(true);
        let _ = h.await;
    }

    #[tokio::test]
    async fn history_starts_empty_and_limit_is_clamped() {
        let (stop, h, pipe, _dir) = start("hist").await;
        let mut c = Client::connect(&pipe).unwrap();
        let Msg::History { runs } = c.call(Msg::GetHistory { limit: u32::MAX }, Duration::from_secs(3)).await.unwrap() else { panic!() };
        assert!(runs.is_empty());
        let _ = stop.send(true);
        let _ = h.await;
    }

    /// 他のプロセスを巻き込まないよう、一意な名前にコピーした実行ファイルを「犠牲者」として起動する。
    fn spawn_victim(tag: &str) -> (std::process::Child, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("memopt_victim_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(format!("memopt_victim_{tag}.exe"));
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
        let child = std::process::Command::new(&exe).args(["-n", "120", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(600));
        (child, exe)
    }

    async fn find_feature(c: &mut Client, exe: &std::path::Path) -> memopt_core::Feature {
        let want = format!("app:{}", exe.display().to_string().to_ascii_lowercase());
        for _ in 0..5 {
            if let Msg::Features { features } = c.call(Msg::GetFeatures, Duration::from_secs(10)).await.unwrap() {
                if let Some(f) = features.into_iter().find(|f| f.id == want) {
                    return f;
                }
            }
            tokio::time::sleep(Duration::from_millis(2200)).await; // 一覧のキャッシュ(2 秒)を待つ
        }
        panic!("犠牲者の機能が一覧に無い: {want}");
    }

    #[tokio::test]
    async fn stop_flow_graceful_request_then_force_kill_with_journal() {
        use memopt_core::stop::{Outcome, StopMethod};
        let (stop, h, pipe, _dir) = start("stop").await;
        let (mut child, exe) = spawn_victim("flow");
        let mut c = Client::connect(&pipe).unwrap();
        let t = Duration::from_secs(15);
        let f = find_feature(&mut c, &exe).await;

        // 計画: 一般アプリなので 正常終了 → 強制終了 → 優先度 の順に提示される。
        let Msg::Plan { plan } = c.call(Msg::PlanStop { feature_id: f.id.clone() }, t).await.unwrap() else { panic!() };
        let ms: Vec<_> = plan.options.iter().map(|o| o.method).collect();
        assert_eq!(&ms[..2], &[StopMethod::GracefulClose, StopMethod::ForceKill]);

        // 正常終了の依頼: GUI に渡す対象（PID, 起動時刻）が返り、結果は「依頼済み」で記録される。
        let Msg::StopResult { entry, graceful_targets } = c.call(Msg::StopFeature { feature_id: f.id.clone(), method: StopMethod::GracefulClose, target: None, confirmed: false }, t).await.unwrap() else { panic!() };
        assert_eq!(entry.outcome, Outcome::Requested);
        assert!(graceful_targets.iter().any(|(pid, st)| *pid == child.id() && *st != 0));
        // GUI がタイムアウトを報告 → 自動で強制終了にはならず、記録だけ TimedOut になる。
        assert_eq!(c.call(Msg::FinishGraceful { journal_id: entry.id, ok: false }, t).await.unwrap(), Msg::Ack);
        assert!(child.try_wait().unwrap().is_none(), "正常終了のタイムアウトで勝手に強制終了してはいけない");
        // 結果を二重に記録することはできない。
        assert!(matches!(c.call(Msg::FinishGraceful { journal_id: entry.id, ok: true }, t).await.unwrap(), Msg::Error { .. }));

        // 利用者が明示的に強制終了を選ぶ。
        let Msg::StopResult { entry: killed, .. } = c.call(Msg::StopFeature { feature_id: f.id.clone(), method: StopMethod::ForceKill, target: None, confirmed: false }, t).await.unwrap() else { panic!() };
        assert_eq!(killed.outcome, Outcome::Ok, "{killed:?}");
        assert!(killed.avail_before.is_some() && killed.avail_after.is_some());
        assert!(child.wait().is_ok());

        // 強制終了は元に戻せない。ジャーナルは新しい順に両方残る。
        let r = c.call(Msg::Undo { journal_id: killed.id }, t).await.unwrap();
        assert!(matches!(r, Msg::Error { ref code, .. } if code == "undo_failed"));
        let Msg::Journal { entries } = c.call(Msg::GetJournal { limit: 10 }, t).await.unwrap() else { panic!() };
        assert_eq!(entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![killed.id, entry.id]);
        assert_eq!(entries[1].outcome, Outcome::TimedOut);

        // 同じ機能はもう存在しない。
        let r = c.call(Msg::PlanStop { feature_id: f.id }, t).await.unwrap();
        assert!(matches!(r, Msg::Error { ref code, .. } if code == "not_found"));
        let _ = stop.send(true);
        let _ = h.await;
    }

    #[tokio::test]
    async fn protected_features_have_no_options_and_every_stop_is_denied() {
        use memopt_core::stop::StopMethod;
        use memopt_core::SafetyClass;
        let (stop, h, pipe, _dir) = start("prot").await;
        let mut c = Client::connect(&pipe).unwrap();
        let t = Duration::from_secs(15);
        let Msg::Features { features } = c.call(Msg::GetFeatures, t).await.unwrap() else { panic!() };
        let prot: Vec<_> = features.iter().filter(|f| f.safety == SafetyClass::Protected).collect();
        assert!(!prot.is_empty(), "システムには保護対象の機能がある");
        // 本ツール自身（テストプロセス名ではなくサービスの exe 名）は一覧に無いので、保護対象の先頭で確認する。
        let target = prot[0].id.clone();
        let Msg::Plan { plan } = c.call(Msg::PlanStop { feature_id: target.clone() }, t).await.unwrap() else { panic!() };
        assert!(plan.options.is_empty() && plan.safety == SafetyClass::Protected);
        for m in [StopMethod::ForceKill, StopMethod::GracefulClose, StopMethod::ServiceStop, StopMethod::DisableStartup] {
            let r = c.call(Msg::StopFeature { feature_id: target.clone(), method: m, target: None, confirmed: true }, t).await.unwrap();
            assert!(matches!(r, Msg::Error { ref code, .. } if code == "denied"), "{m:?}: {r:?}");
        }
        // 存在しない機能・存在しないジャーナル
        assert!(matches!(c.call(Msg::PlanStop { feature_id: "nope".into() }, t).await.unwrap(), Msg::Error { .. }));
        assert!(matches!(c.call(Msg::Undo { journal_id: 999 }, t).await.unwrap(), Msg::Error { .. }));
        let _ = stop.send(true);
        let _ = h.await;
    }
}
