//! サービスとの接続スレッド。つながらなければ読み取り専用の監視モードに落ち、3 秒ごとに再接続する。
use crate::stop_cmds::{self, StopCmd};
use memopt_core::ipc::{Msg, RunSummary, PIPE_NAME};
use memopt_core::stop::{JournalEntry, StopPlan};
use memopt_core::{Feature, Level, MemorySnapshot};
use memopt_ipc::Client;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT: Duration = Duration::from_secs(3);

#[derive(Clone, Default)]
pub struct View {
    pub connected: bool,
    pub mem: Option<MemorySnapshot>,
    pub features: Vec<Feature>,
    pub history: Vec<RunSummary>,
    pub auto_enabled: bool,
    pub paused: bool,
    pub emergency_stop: bool,
    pub pressure: bool,
    /// config.toml の ui 設定（接続時に取得）。
    pub ui: Option<memopt_core::config::Ui>,
    /// 直近の通知/エラー（画面下部に表示）。
    pub notice: Option<String>,
    /// 確認ダイアログに出す停止計画。
    pub plan: Option<StopPlan>,
    pub journal: Vec<JournalEntry>,
    /// 直近の停止結果（「元に戻す」ボタンを出す）。
    pub last_stop: Option<JournalEntry>,
    /// 正常終了に失敗した機能（強制終了を改めて確認する導線を出す）。
    pub force_hint: Option<String>,
    /// 現在有効な自動調整（控えめにしている設定）の説明。
    pub adaptive: Vec<String>,
}

#[derive(Debug)]
pub enum Cmd {
    RunClean(Level),
    Pause(u64),
    EmergencyStop(bool),
    ResetAdaptive,
    Stop(StopCmd),
}

#[derive(Clone, Copy)]
pub struct Options {
    /// 機能一覧を取得する間隔（None なら取得しない）。
    pub features_every: Option<Duration>,
    /// ユーザーセッションの状況（全画面/無操作/最前面/音声）を中継するか。
    pub report_session: bool,
    pub poll_every: Duration,
}

pub struct Link {
    pub view: Arc<Mutex<View>>,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Link {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
    pub fn snapshot(&self) -> View {
        self.view.lock().unwrap().clone()
    }
}

fn note(view: &Mutex<View>, s: impl Into<String>) {
    view.lock().unwrap().notice = Some(s.into());
}

fn describe(m: Msg) -> Option<String> {
    match m {
        Msg::CleanFinished { trigger, run } => Some(format!(
            "解放 L{} ({}) 空き {} → {} MB",
            run.level,
            trigger.as_str(),
            run.avail_before >> 20,
            run.avail_after >> 20
        )),
        Msg::Skipped { reason, .. } => Some(format!("自動解放を見送り: {reason}")),
        Msg::AdaptiveNotice { reason } => Some(format!("自動調整: {reason}")),
        Msg::PressureChanged { active } => Some(if active { "メモリ圧迫を検知".into() } else { "メモリ圧迫が解消".into() }),
        _ => None,
    }
}

async fn session_loop(c: &mut Client, view: &Mutex<View>, rx: &mut mpsc::UnboundedReceiver<Cmd>, o: Options) -> Result<(), String> {
    // 接続のたびに設定を取得（ショートカットキーやアイコン表示の設定を反映するため）。
    if let Msg::Config { toml, .. } = c.call(Msg::GetConfig, CALL_TIMEOUT).await.map_err(|e| e.to_string())? {
        view.lock().unwrap().ui = Some(memopt_core::Config::parse_and_validate(&toml).0.ui);
    }
    let (mut last_feat, mut last_hist, mut last_sess) = (None::<Instant>, None::<Instant>, None::<Instant>);
    loop {
        let due = |t: &Option<Instant>, every: Duration| t.is_none_or(|t| t.elapsed() >= every);
        if let Msg::Snapshot { mem, auto_enabled, paused, emergency_stop, adaptive } =
            c.call(Msg::GetSnapshot, CALL_TIMEOUT).await.map_err(|e| e.to_string())?
        {
            let mut v = view.lock().unwrap();
            (v.mem, v.auto_enabled, v.paused, v.emergency_stop) = (Some(mem), auto_enabled, paused, emergency_stop);
            v.adaptive = adaptive;
        }
        if let Some(every) = o.features_every.filter(|e| due(&last_feat, *e)) {
            let _ = every;
            if let Msg::Features { features } = c.call(Msg::GetFeatures, CALL_TIMEOUT).await.map_err(|e| e.to_string())? {
                view.lock().unwrap().features = features;
            }
            last_feat = Some(Instant::now());
        }
        if due(&last_hist, Duration::from_secs(10)) {
            if let Msg::History { runs } = c.call(Msg::GetHistory { limit: 30 }, CALL_TIMEOUT).await.map_err(|e| e.to_string())? {
                view.lock().unwrap().history = runs;
            }
            if let Msg::Journal { entries } = c.call(Msg::GetJournal { limit: 100 }, CALL_TIMEOUT).await.map_err(|e| e.to_string())? {
                view.lock().unwrap().journal = entries;
            }
            last_hist = Some(Instant::now());
        }
        if o.report_session && due(&last_sess, Duration::from_secs(5)) {
            // COM(オーディオ列挙)を含むためブロッキングスレッドで取る。
            let info = tokio::task::spawn_blocking(crate::session_info::collect).await.map_err(|e| e.to_string())?;
            c.call(Msg::ReportSession(info), CALL_TIMEOUT).await.map_err(|e| e.to_string())?;
            last_sess = Some(Instant::now());
        }
        while let Some(n) = c.notifications.pop_front() {
            if let Msg::PressureChanged { active } = &n {
                view.lock().unwrap().pressure = *active;
            }
            if let Some(s) = describe(n) {
                note(view, s);
            }
        }
        // 次のポーリングまで、コマンドを受けながら待つ（コマンドを処理したらすぐ状態を取り直す）。
        tokio::select! {
            _ = tokio::time::sleep(o.poll_every) => {}
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                let msg = match cmd {
                    Cmd::RunClean(level) => Msg::RunClean { level },
                    Cmd::Pause(secs) => Msg::SetPaused { secs },
                    Cmd::EmergencyStop(on) => Msg::EmergencyStop { on },
                    Cmd::ResetAdaptive => Msg::ResetAdaptive,
                    Cmd::Stop(s) => {
                        stop_cmds::handle(c, view, s).await?;
                        last_hist = None; // ジャーナルをすぐ取り直す
                        continue;
                    }
                };
                if let Msg::Error { message, .. } = c.call(msg, CALL_TIMEOUT).await.map_err(|e| e.to_string())? {
                    note(view, message);
                }
            }
        }
    }
}

pub fn spawn(o: Options) -> Link {
    let view = Arc::new(Mutex::new(View::default()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let v = view.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async move {
            loop {
                match Client::connect(PIPE_NAME) {
                    Ok(mut c) => {
                        v.lock().unwrap().connected = true;
                        let r = session_loop(&mut c, &v, &mut rx, o).await;
                        v.lock().unwrap().connected = false;
                        match r {
                            Ok(()) => return, // Link が破棄された
                            Err(e) => note(&v, format!("サービスとの接続が切れました: {e}")),
                        }
                    }
                    Err(_) => v.lock().unwrap().connected = false,
                }
                tokio::time::sleep(RECONNECT).await;
            }
        });
    });
    Link { view, tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::Trigger;

    #[test]
    fn notifications_are_described_for_the_ui() {
        let run = RunSummary {
            id: 1, ts_unix: 0, level: 1, trigger: "pressure".into(), avail_before: 100 << 20, avail_after: 200 << 20,
            standby_before: None, standby_after: None, avail_after60: None, note: None,
        };
        assert!(describe(Msg::CleanFinished { trigger: Trigger::Pressure, run }).unwrap().contains("100 → 200 MB"));
        assert!(describe(Msg::Skipped { trigger: Trigger::Idle, reason: "全画面".into() }).unwrap().contains("全画面"));
        assert!(describe(Msg::PressureChanged { active: true }).unwrap().contains("圧迫"));
        assert_eq!(describe(Msg::Ack), None);
    }

    #[test]
    fn without_a_service_the_view_stays_disconnected() {
        let l = spawn(Options { features_every: None, report_session: false, poll_every: Duration::from_millis(100) });
        std::thread::sleep(Duration::from_millis(300));
        assert!(!l.snapshot().connected);
    }
}
