//! 解放の実行（直列に 1 件ずつ）。実行前/直後/60 秒後を history.db に記録する。
use crate::state::Shared;
use memopt_cleaner::CleanReport;
use memopt_core::ipc::{Frame, Msg, RunSummary};
use memopt_core::{Level, LoadSample, MemSample, Trigger};
use memopt_policy::adaptive::Evaluation;
use memopt_policy::l1::{select_l1_targets, L1Context};
use memopt_probe::Pdh;
use memopt_store::RunRecord;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

pub const POST_MEASURE_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct RunRequest {
    pub trigger: Trigger,
    pub levels: Vec<Level>,
}

fn mem_sample() -> MemSample {
    let m = memopt_probe::memory_snapshot().unwrap_or_default();
    // 瞬時値（スタンバイ/変更ページ）用に、毎回新しい PDH クエリを使う。
    // 共有の PDH はレート系（ハードフォルト）の間隔を保つため触らない。
    let p = Pdh::new();
    MemSample {
        avail_phys: m.avail_phys,
        standby: p.as_ref().and_then(Pdh::standby_bytes),
        modified: p.as_ref().and_then(Pdh::modified_bytes),
        commit_pct: m.commit_pct(),
    }
}

/// 解放量: 空きメモリの増加か、スタンバイの減少の大きい方。
/// （スタンバイは「利用可能」に含まれるため、L2/L3 の効果は空きメモリの増加だけでは測れない）
pub fn freed_bytes(before: &MemSample, after: &MemSample) -> u64 {
    let avail = after.avail_phys.saturating_sub(before.avail_phys);
    let standby = match (before.standby, after.standby) {
        (Some(b), Some(a)) => b.saturating_sub(a),
        _ => 0,
    };
    avail.max(standby)
}

/// execute の結果。60 秒後の評価（自動調整）に必要なものを持ち回る。
pub struct Executed {
    pub summary: RunSummary,
    pub end_t: u64,
    pub top_level: Level,
    pub before: MemSample,
    pub after: MemSample,
    pub load_before: Option<LoadSample>,
}

fn filetime_now() -> u64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + (d.subsec_nanos() / 100) as u64
}

/// L1 の対象を選ぶ。トレイの報告が新しければ最前面/オーディオを除外する。
fn l1_targets(shared: &Shared, procs: &[memopt_core::ProcessInfo]) -> (Vec<u32>, Option<String>) {
    let cfg = shared.cfg.read().unwrap().clone();
    let session = shared.fresh_session();
    let busy: HashSet<u32> = {
        let h = shared.cpu_hist.lock().unwrap();
        match (h.front(), h.back()) {
            (Some(a), Some(b)) if h.len() >= 2 => {
                let window = b.0.duration_since(a.0).as_secs().max(1);
                // 窓の 0.5% 以上 CPU を使ったプロセスは「使用中」。
                memopt_probe::busy_pids(&a.1, &b.1, window * 10_000_000 / 200)
            }
            _ => HashSet::new(),
        }
    };
    let ctx = L1Context {
        now_filetime: filetime_now(),
        self_pid: std::process::id(),
        foreground_pid: session.as_ref().and_then(|s| s.foreground_pid),
        audio_pids: session.as_ref().map(|s| s.audio_pids.iter().copied().collect()).unwrap_or_default(),
        busy_pids: busy,
        user_protected: &cfg.protect.user_paths,
        max_processes: cfg.l1.max_processes,
        skip_foreground: cfg.l1.skip_foreground,
        skip_audio: cfg.l1.skip_audio,
    };
    let note = session.is_none().then(|| "ユーザーセッション情報が無いため最前面/音声の除外は未適用".to_string());
    (select_l1_targets(procs, &ctx), note)
}

/// ブロッキング実行（spawn_blocking から呼ぶ）。
pub fn execute(shared: &Shared, req: &RunRequest) -> Result<Executed, String> {
    let start_t = shared.now_secs();
    let load_before = shared.loads.lock().unwrap().avg_load(start_t.saturating_sub(60), start_t);
    let before = mem_sample();

    let mut reports: Vec<CleanReport> = Vec::new();
    let mut note = None;
    for level in &req.levels {
        let r = if *level == Level::L1 {
            let procs = memopt_probe::processes();
            let (targets, n) = l1_targets(shared, &procs);
            note = n;
            memopt_cleaner::run_l1(&targets)
        } else {
            memopt_cleaner::run_level(*level)
        };
        if let Some(why) = &r.unavailable {
            note = Some(format!("L{} は利用不可: {why}", *level as u8));
        }
        reports.push(r);
    }
    std::thread::sleep(Duration::from_millis(500));
    let after = mem_sample();
    let end_t = shared.now_secs();

    let ts_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let top_level = req.levels.iter().max().copied().unwrap_or(Level::L1) as u8;
    let id = shared
        .store
        .lock()
        .unwrap()
        .insert_run(&RunRecord {
            id: 0,
            ts_unix,
            level: top_level,
            trigger: req.trigger.as_str().into(),
            before,
            after,
            load_before,
            after60: None,
            load_after60: None,
            report_json: serde_json::to_string(&reports).map_err(|e| e.to_string())?,
            note: None,
        })
        .map_err(|e| e.to_string())?;
    let summary = RunSummary {
            id,
            ts_unix,
            level: top_level,
            trigger: req.trigger.as_str().into(),
            avail_before: before.avail_phys,
            avail_after: after.avail_phys,
            standby_before: before.standby,
            standby_after: after.standby,
            avail_after60: None,
            note,
    };
    Ok(Executed { summary, end_t, top_level: req.levels.iter().max().copied().unwrap_or(Level::L1), before, after, load_before })
}

/// 60 秒後の測定を履歴に追記し、自動調整（逆効果・効果小の判定）に反映する。
async fn post_measure(shared: Arc<Shared>, notify: broadcast::Sender<Frame>, trigger: Trigger, ex: Executed) {
    post_measure_after(shared, notify, trigger, ex, Duration::from_secs(POST_MEASURE_SECS + 5)).await;
}

/// `wait` 後に測定する（テストでは短くできるよう分けてある）。
async fn post_measure_after(shared: Arc<Shared>, notify: broadcast::Sender<Frame>, trigger: Trigger, ex: Executed, wait: Duration) {
    tokio::time::sleep(wait).await;
    let load = shared.loads.lock().unwrap().avg_load(ex.end_t, ex.end_t + POST_MEASURE_SECS + 5).unwrap_or_default();
    let mem = tokio::task::spawn_blocking(mem_sample).await.unwrap_or_default();
    let id = ex.summary.id;
    if let Err(e) = shared.store.lock().unwrap().update_after60(id, &mem, &load) {
        tracing::warn!("60 秒後の測定を保存できません: {e}");
    }
    // ハードフォルトが測れていないとき（PDH 不可）は、逆効果の判定をしない（効果の大小だけ判定する）。
    let measured = shared.loads.lock().unwrap().avg_load(ex.end_t, ex.end_t + POST_MEASURE_SECS + 5).is_some();
    let total_phys = memopt_probe::memory_snapshot().map_or(0, |m| m.total_phys);
    let eval = Evaluation {
        trigger,
        top_level: ex.top_level,
        freed_bytes: freed_bytes(&ex.before, &ex.after),
        total_phys,
        load_before: ex.load_before,
        load_after: measured.then_some(load),
    };
    let adjustments = shared.engine.lock().unwrap().record_result(shared.now_secs(), &eval);
    if adjustments.is_empty() {
        return;
    }
    // 「なぜ控えめにしたか」を履歴に残し、画面へ通知する。
    let reason = adjustments.iter().map(|a| a.reason.clone()).collect::<Vec<_>>().join(" / ");
    tracing::warn!("自動調整: {reason}");
    if let Err(e) = shared.store.lock().unwrap().set_note(id, &reason) {
        tracing::warn!("自動調整の理由を保存できません: {e}");
    }
    let _ = notify.send(Frame::new(None, Msg::AdaptiveNotice { reason }));
}

/// 解放要求を 1 件ずつ処理する。実行中は `running` を立て、完了を通知する。
pub async fn run_loop(shared: Arc<Shared>, mut rx: mpsc::Receiver<RunRequest>, notify: broadcast::Sender<Frame>) {
    while let Some(req) = rx.recv().await {
        shared.running.store(true, Ordering::SeqCst);
        let s = shared.clone();
        let r = req.clone();
        let res = tokio::task::spawn_blocking(move || execute(&s, &r)).await;
        shared.running.store(false, Ordering::SeqCst);
        match res {
            Ok(Ok(ex)) => {
                let summary = ex.summary.clone();
                tracing::info!(
                    "解放 L{} ({}) 空き {} -> {} MB",
                    summary.level,
                    summary.trigger,
                    summary.avail_before >> 20,
                    summary.avail_after >> 20
                );
                let _ = notify.send(Frame::new(None, Msg::CleanFinished { trigger: req.trigger, run: summary }));
                tokio::spawn(post_measure(shared.clone(), notify.clone(), req.trigger, ex));
            }
            Ok(Err(e)) => tracing::error!("解放の記録に失敗: {e}"),
            Err(e) => tracing::error!("解放タスクが異常終了: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(avail_mb: u64, standby_mb: Option<u64>) -> MemSample {
        MemSample { avail_phys: avail_mb << 20, standby: standby_mb.map(|v| v << 20), modified: None, commit_pct: 0.0 }
    }

    #[test]
    fn freed_is_the_larger_of_free_gain_and_standby_drop() {
        assert_eq!(freed_bytes(&m(100, Some(500)), &m(300, Some(480))), 200 << 20); // L1: 空きの増加
        assert_eq!(freed_bytes(&m(100, Some(500)), &m(101, Some(50))), 450 << 20); // L2/L3: スタンバイの減少
    }

    #[test]
    fn freed_never_goes_negative_and_tolerates_missing_standby() {
        assert_eq!(freed_bytes(&m(300, Some(100)), &m(100, Some(400))), 0);
        assert_eq!(freed_bytes(&m(100, None), &m(250, Some(10))), 150 << 20);
    }

    use crate::loadring::Sample;
    use memopt_core::config::Adaptive;
    use memopt_core::Config;

    fn shared_for_test() -> Arc<Shared> {
        let dir = std::env::temp_dir().join(format!("memopt_runner_{}", std::process::id()));
        let journal = memopt_actions::journal::Journal::open(&dir.join("journal.jsonl"));
        let store = memopt_store::Store::open_in_memory().unwrap();
        let mut cfg = Config::default();
        cfg.adaptive = Adaptive::default();
        Arc::new(Shared::new(dir.join("config.toml"), cfg, store, journal))
    }

    fn executed(shared: &Shared, trigger: Trigger, level: Level, load_before: f64) -> Executed {
        let rec = RunRecord {
            id: 0, ts_unix: 0, level: level as u8, trigger: trigger.as_str().into(),
            before: m(100, Some(500)), after: m(2000, Some(400)), load_before: None, after60: None, load_after60: None,
            report_json: "[]".into(), note: None,
        };
        let id = shared.store.lock().unwrap().insert_run(&rec).unwrap();
        let summary = RunSummary {
            id, ts_unix: 0, level: level as u8, trigger: trigger.as_str().into(), avail_before: 0, avail_after: 0,
            standby_before: None, standby_after: None, avail_after60: None, note: None,
        };
        Executed {
            summary, end_t: 0, top_level: level, before: m(100, Some(500)), after: m(2000, Some(400)),
            load_before: Some(LoadSample { pages_input_per_sec: load_before, disk_read_bytes_per_sec: 0.0 }),
        }
    }

    fn push_load(shared: &Shared, t: u64, pages: f64) {
        shared.loads.lock().unwrap().push(Sample {
            t, load: LoadSample { pages_input_per_sec: pages, disk_read_bytes_per_sec: 0.0 }, disk_busy_pct: None,
        });
    }

    #[tokio::test]
    async fn counterproductive_run_is_noted_in_history_broadcast_and_applied_to_the_engine() {
        let shared = shared_for_test();
        let (tx, mut rx) = broadcast::channel(8);
        let ex = executed(&shared, Trigger::LowMemory, Level::L2, 100.0);
        // 実行後 60 秒間のハードフォルトが 100/s -> 900/s に悪化
        push_load(&shared, 10, 900.0);
        push_load(&shared, 25, 900.0);
        post_measure_after(shared.clone(), tx, Trigger::LowMemory, ex, Duration::from_millis(5)).await;

        let note = shared.store.lock().unwrap().list_runs(1).unwrap()[0].note.clone().expect("理由が履歴に残る");
        assert!(note.contains("ハードフォルト") && note.contains("L1 までに制限"), "{note}");
        let Msg::AdaptiveNotice { reason } = rx.try_recv().unwrap().msg else { panic!("通知が来る") };
        assert_eq!(reason, note);
        let notes = shared.engine.lock().unwrap().adaptive_notes(shared.now_secs());
        assert!(notes.iter().any(|n| n.contains("低メモリ通知") && n.contains("L1 まで")), "{notes:?}");
        // 60 秒後の測定値も保存されている
        assert!(shared.store.lock().unwrap().list_runs(1).unwrap()[0].load_after60.is_some());
    }

    #[tokio::test]
    async fn healthy_run_leaves_no_note_and_no_adjustment() {
        let shared = shared_for_test();
        let (tx, mut rx) = broadcast::channel(8);
        let ex = executed(&shared, Trigger::Pressure, Level::L1, 100.0);
        push_load(&shared, 10, 90.0);
        post_measure_after(shared.clone(), tx, Trigger::Pressure, ex, Duration::from_millis(5)).await;
        assert_eq!(shared.store.lock().unwrap().list_runs(1).unwrap()[0].note, None);
        assert!(rx.try_recv().is_err());
        assert!(shared.engine.lock().unwrap().adaptive_notes(shared.now_secs()).is_empty());
    }

    #[tokio::test]
    async fn manual_runs_never_trigger_adjustments_even_if_they_hurt() {
        let shared = shared_for_test();
        let (tx, mut rx) = broadcast::channel(8);
        let ex = executed(&shared, Trigger::Manual, Level::L3, 10.0);
        push_load(&shared, 10, 5000.0);
        post_measure_after(shared.clone(), tx, Trigger::Manual, ex, Duration::from_millis(5)).await;
        assert!(rx.try_recv().is_err());
        assert!(shared.engine.lock().unwrap().adaptive_notes(shared.now_secs()).is_empty());
    }

    #[tokio::test]
    async fn without_pdh_samples_only_effect_size_is_judged() {
        // ハードフォルトが測れていない（リングが空）場合は逆効果とは判定しない。解放量は十分なので調整なし。
        let shared = shared_for_test();
        let (tx, mut rx) = broadcast::channel(8);
        let ex = executed(&shared, Trigger::Pressure, Level::L1, 100.0);
        post_measure_after(shared.clone(), tx, Trigger::Pressure, ex, Duration::from_millis(5)).await;
        assert!(rx.try_recv().is_err());
    }
}
