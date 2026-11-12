//! サービスの共有状態。ロックは短く保ち、await をまたいで保持しない。
use crate::loadring::LoadRing;
use memopt_actions::journal::Journal;
use memopt_actions::task::TaskInfo;
use memopt_core::ipc::SessionInfo;
use memopt_core::{Config, Feature, MemorySnapshot};
use memopt_policy::engine::Engine;
use memopt_probe::{LowMemoryWatcher, Pdh};
use memopt_store::Store;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

/// トレイからの状況報告がこの秒数より古ければ「不明」として扱う。
pub const SESSION_STALE_SECS: u64 = 30;
pub const FEATURES_TTL_SECS: u64 = 2;
pub const MAX_PAUSE_SECS: u64 = 24 * 3600;
/// busy 判定に使う CPU 時間サンプルの数（30 秒間隔 × 2 ≒ 30 秒の窓。サンプル取得は 1 回 約 9ms と重いため間引く）。
pub const CPU_HISTORY: usize = 2;
/// タスク一覧（COM で数百件を列挙するので重い）のキャッシュ時間。
pub const TASKS_TTL_SECS: u64 = 600;

pub struct Shared {
    pub journal: Journal,
    /// 停止操作は 1 件ずつ（複数機能の一括停止は提供しない）。
    pub stop_lock: Mutex<()>,
    pub tasks: Mutex<Option<(Instant, Vec<TaskInfo>)>>,
    pub started: Instant,
    pub config_path: PathBuf,
    pub cfg: RwLock<Config>,
    pub engine: Mutex<Engine>,
    pub store: Mutex<Store>,
    pub pdh: Mutex<Option<Pdh>>,
    pub low_mem: Option<LowMemoryWatcher>,
    pub session: Mutex<Option<(SessionInfo, Instant)>>,
    pub features: Mutex<Option<(Instant, Vec<Feature>)>>,
    pub mem: Mutex<Option<MemorySnapshot>>,
    pub paused_until: Mutex<Option<Instant>>,
    pub loads: Mutex<LoadRing>,
    pub cpu_hist: Mutex<VecDeque<(Instant, HashMap<u32, u64>)>>,
    pub running: AtomicBool,
    pub pressure_active: AtomicBool,
}

// LowMemoryWatcher は HANDLE を保持するが、Query のみで読み取り専用に使う。
unsafe impl Sync for Shared {}
unsafe impl Send for Shared {}

impl Shared {
    pub fn new(config_path: PathBuf, cfg: Config, store: Store, journal: Journal) -> Self {
        Self {
            journal,
            stop_lock: Mutex::new(()),
            tasks: Mutex::new(None),
            started: Instant::now(),
            engine: Mutex::new(Engine::new(&cfg)),
            cfg: RwLock::new(cfg),
            config_path,
            store: Mutex::new(store),
            pdh: Mutex::new(Pdh::new()),
            low_mem: LowMemoryWatcher::new(),
            session: Mutex::new(None),
            features: Mutex::new(None),
            mem: Mutex::new(None),
            paused_until: Mutex::new(None),
            loads: Mutex::new(LoadRing::default()),
            cpu_hist: Mutex::new(VecDeque::new()),
            running: AtomicBool::new(false),
            pressure_active: AtomicBool::new(false),
        }
    }

    pub fn now_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// 新しい報告があるときだけ返す。
    pub fn fresh_session(&self) -> Option<SessionInfo> {
        let g = self.session.lock().unwrap();
        g.as_ref()
            .filter(|(_, at)| at.elapsed().as_secs() <= SESSION_STALE_SECS)
            .map(|(s, _)| s.clone())
    }

    pub fn is_paused(&self) -> bool {
        let mut g = self.paused_until.lock().unwrap();
        match *g {
            Some(t) if Instant::now() < t => true,
            Some(_) => {
                *g = None;
                false
            }
            None => false,
        }
    }

    pub fn set_paused(&self, secs: u64) {
        *self.paused_until.lock().unwrap() =
            (secs > 0).then(|| Instant::now() + std::time::Duration::from_secs(secs.min(MAX_PAUSE_SECS)));
    }
}
