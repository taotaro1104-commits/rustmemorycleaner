//! 評価ループ（設計書3章）: 既定 15 秒ごとにスナップショット → トリガー判定 → 実行/抑止の記録。
use crate::loadring::Sample;
use crate::runner::RunRequest;
use crate::state::{Shared, CPU_HISTORY};
use memopt_core::ipc::{Frame, Msg};
use memopt_policy::cron::{due_schedule, Moment};
use memopt_policy::engine::{Decision, Input, SkipReason};
use memopt_core::Trigger;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};

/// ブロッキング呼び出しを含む 1 回分の計測と入力の組み立て。
/// CPU 時間のサンプルを取る間隔（評価ループの何回に 1 回か）。
const CPU_SAMPLE_EVERY: u64 = 2;
/// 自分自身のワーキングセットを整理する間隔（評価ループの何回に 1 回か）。
const SELF_TRIM_EVERY: u64 = 4;

pub fn build_input(shared: &Shared, tick: u64) -> Option<Input> {
    let mem = memopt_probe::memory_snapshot()?;
    *shared.mem.lock().unwrap() = Some(mem);
    let now = shared.now_secs();

    // 負荷サンプル（ハードフォルト・ディスク）をリングに積む。
    {
        let pdh = shared.pdh.lock().unwrap();
        if let Some(p) = pdh.as_ref() {
            p.collect();
            if let Some(load) = p.load() {
                shared.loads.lock().unwrap().push(Sample { t: now, load, disk_busy_pct: p.disk_busy_pct() });
            }
        }
    }
    if tick % CPU_SAMPLE_EVERY == 0 {
        record_cpu_times(shared);
    }

    let cfg = shared.cfg.read().unwrap().clone();
    let session = shared.fresh_session();
    let (minute, hour, day, month, weekday) = memopt_probe::local_time_parts();
    let unix_min = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    let moment = Moment { minute, hour, day, month, weekday };

    Some(Input {
        now,
        mem,
        uptime_secs: memopt_probe::uptime_secs(),
        user_idle_secs: session.as_ref().map(|s| s.user_idle_secs),
        fullscreen: session.as_ref().is_some_and(|s| s.fullscreen),
        // 直近 30 秒（15 秒間隔で 2 点）の平均で判定する。
        disk_busy_pct: shared.loads.lock().unwrap().avg_disk_busy(now, 30),
        battery_saver: memopt_probe::battery_saver_on() || session.as_ref().is_some_and(|s| s.battery_saver),
        low_memory_signal: shared.low_mem.as_ref().is_some_and(|w| w.is_low()),
        schedule_due: due_schedule(&cfg.auto_clean.schedule, moment, unix_min),
        manual: None, // 手動は IPC から直接 runner へ送る
        paused: shared.is_paused(),
    })
}

fn record_cpu_times(shared: &Shared) {
    // 全プロセスの CPU 時間を 1 回のシステム呼び出しで取る（ハンドルを開かないので常駐でも軽い）。
    let times = memopt_probe::all_cpu_times();
    let mut h = shared.cpu_hist.lock().unwrap();
    h.push_back((Instant::now(), times));
    while h.len() > CPU_HISTORY {
        h.pop_front();
    }
}

/// 判定結果を処理する。`last_skip` は同じ理由の連続ログ/通知を避けるための前回値。
pub fn apply_decision(
    d: Decision,
    run_tx: &mpsc::Sender<RunRequest>,
    notify: &broadcast::Sender<Frame>,
    last_skip: &mut Option<(Trigger, SkipReason)>,
) {
    match d {
        Decision::None => *last_skip = None,
        Decision::Run { trigger, levels } => {
            *last_skip = None;
            if run_tx.try_send(RunRequest { trigger, levels }).is_err() {
                tracing::warn!("解放要求を積めませんでした（実行中）");
            }
        }
        Decision::Skip { trigger, reason } => {
            if *last_skip != Some((trigger, reason)) {
                tracing::info!("自動解放を見送り ({}): {}", trigger.as_str(), reason.text());
                let _ = notify.send(Frame::new(None, Msg::Skipped { trigger, reason: reason.text().to_string() }));
                *last_skip = Some((trigger, reason));
            }
        }
    }
}

fn pressure_now(shared: &Shared, input: &Input) -> bool {
    let p = &shared.cfg.read().unwrap().auto_clean.pressure;
    input.mem.avail_pct() < p.available_below_pct || input.mem.commit_pct() > p.commit_above_pct
}

pub async fn eval_loop(
    shared: Arc<Shared>,
    run_tx: mpsc::Sender<RunRequest>,
    notify: broadcast::Sender<Frame>,
    mut stop: watch::Receiver<bool>,
) {
    let mut last_skip = None;
    let mut tick: u64 = 0;
    loop {
        let secs = shared.cfg.read().unwrap().monitor.eval_interval_sec;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = stop.changed() => { if *stop.borrow() { return; } }
        }
        tick += 1;
        let s = shared.clone();
        let t = tick;
        let Ok(Some(input)) = tokio::task::spawn_blocking(move || build_input(&s, t)).await else { continue };

        let now_pressure = pressure_now(&shared, &input);
        if shared.pressure_active.swap(now_pressure, Ordering::SeqCst) != now_pressure {
            let _ = notify.send(Frame::new(None, Msg::PressureChanged { active: now_pressure }));
        }
        let decision = shared.engine.lock().unwrap().evaluate(&input);
        apply_decision(decision, &run_tx, &notify, &mut last_skip);
        // 自分自身のワーキングセットも整理する（設計書3章: 常駐メモリ目標のため、解放対象から除外しない）。
        if tick % SELF_TRIM_EVERY == 0 {
            let _ = memopt_cleaner::run_l1(&[std::process::id()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::Level;

    fn chans() -> (mpsc::Sender<RunRequest>, mpsc::Receiver<RunRequest>, broadcast::Sender<Frame>, broadcast::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(4);
        let (n, nr) = broadcast::channel(8);
        (tx, rx, n, nr)
    }

    #[test]
    fn run_decision_is_queued() {
        let (tx, mut rx, n, _nr) = chans();
        let mut last = None;
        apply_decision(Decision::Run { trigger: Trigger::Pressure, levels: vec![Level::L1] }, &tx, &n, &mut last);
        assert_eq!(rx.try_recv().unwrap(), RunRequest { trigger: Trigger::Pressure, levels: vec![Level::L1] });
    }

    #[test]
    fn repeated_same_skip_is_notified_once() {
        let (tx, _rx, n, mut nr) = chans();
        let mut last = None;
        let d = Decision::Skip { trigger: Trigger::Pressure, reason: SkipReason::Fullscreen };
        apply_decision(d.clone(), &tx, &n, &mut last);
        apply_decision(d, &tx, &n, &mut last);
        assert!(matches!(nr.try_recv().unwrap().msg, Msg::Skipped { .. }));
        assert!(nr.try_recv().is_err());
        // 条件が解けた後の同じスキップは再び通知される
        apply_decision(Decision::None, &tx, &n, &mut last);
        apply_decision(Decision::Skip { trigger: Trigger::Pressure, reason: SkipReason::Fullscreen }, &tx, &n, &mut last);
        assert!(nr.try_recv().is_ok());
    }

    #[test]
    fn different_skip_reason_is_notified_again() {
        let (tx, _rx, n, mut nr) = chans();
        let mut last = None;
        apply_decision(Decision::Skip { trigger: Trigger::Idle, reason: SkipReason::DiskBusy }, &tx, &n, &mut last);
        apply_decision(Decision::Skip { trigger: Trigger::Idle, reason: SkipReason::Cooldown }, &tx, &n, &mut last);
        assert!(nr.try_recv().is_ok() && nr.try_recv().is_ok());
    }

    /// 評価ループ 1 回分の各処理の所要時間（CPU 目標 0.1% 未満の調査用）。
    /// 実行: cargo test --release -p memopt-service -- --ignored --nocapture tick_cost
    #[test]
    #[ignore]
    fn tick_cost_breakdown() {
        fn time<T>(label: &str, n: u32, mut f: impl FnMut() -> T) {
            let t = Instant::now();
            for _ in 0..n {
                std::hint::black_box(f());
            }
            println!("{label:<28} {:>8.2} ms/回", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
        let pdh = memopt_probe::Pdh::new().unwrap();
        let low = memopt_probe::LowMemoryWatcher::new().unwrap();
        time("memory_snapshot", 50, memopt_probe::memory_snapshot);
        time("pdh.collect", 20, || pdh.collect());
        time("pdh.load+disk", 20, || (pdh.load(), pdh.disk_busy_pct()));
        time("all_cpu_times", 20, memopt_probe::all_cpu_times);
        time("battery_saver_on", 50, memopt_probe::battery_saver_on);
        time("low_memory is_low", 50, || low.is_low());
        time("local_time_parts", 50, memopt_probe::local_time_parts);
        time("self L1 trim", 20, || memopt_cleaner::run_l1(&[std::process::id()]));
    }
}
