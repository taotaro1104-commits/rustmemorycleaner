use serde::{Deserialize, Serialize};

/// 解放レベル（設計書3章）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Level {
    L1 = 1,
    L2 = 2,
    L3 = 3,
}

/// 安全分類。順序は厳しさ順（Protected が最も厳しい）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SafetyClass {
    Stoppable,
    Caution,
    Protected,
}

impl SafetyClass {
    /// 「最も厳しいものを採用」。
    pub fn strictest(self, other: Self) -> Self {
        self.max(other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeatureKind {
    DesktopApp,
    Browser,
    StoreApp,
    Service,
    Startup,
    ScheduledTask,
    Wsl,
    System,
}

/// probe が返す生のプロセス情報。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    /// 起動時刻 (FILETIME 100ns)。PID 再利用の照合用。
    pub start_time: u64,
    pub exe_name: String,
    pub exe_path: Option<String>,
    pub command_line: Option<String>,
    pub package_family: Option<String>,
    /// このプロセスが所有するサービス名（svchost 等）。
    pub services: Vec<String>,
    /// None = 取得不可（保護プロセス等）
    pub private_bytes: Option<u64>,
    pub working_set: Option<u64>,
    pub private_working_set: Option<u64>,
    /// セッション ID（0 = サービス/システム）。
    #[serde(default)]
    pub session_id: u32,
    /// 保護プロセス（PPL 等）か。実行時の安全判定に使う。
    #[serde(default)]
    pub is_ppl: bool,
}

/// 機能単位の集約結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    pub id: String,
    pub name: String,
    pub kind: FeatureKind,
    pub pids: Vec<u32>,
    pub private_bytes: u64,
    pub working_set: u64,
    pub private_working_set: u64,
    pub safety: SafetyClass,
    /// スタートアップ登録（Run キー等）に紐づくか。
    #[serde(default)]
    pub startup: bool,
    /// 停止の対象（サービス/スタートアップ/タスク）。サービス側の再スキャンで埋める。
    #[serde(default)]
    pub targets: crate::stop::StopTargets,
}

/// システム全体のメモリ状態（バイト）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub total_phys: u64,
    pub avail_phys: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub system_cache: u64,
}

impl MemorySnapshot {
    pub fn avail_pct(&self) -> f64 {
        if self.total_phys == 0 { 0.0 } else { self.avail_phys as f64 * 100.0 / self.total_phys as f64 }
    }
    pub fn commit_pct(&self) -> f64 {
        if self.commit_limit == 0 { 0.0 } else { self.commit_total as f64 * 100.0 / self.commit_limit as f64 }
    }
}

/// Run キー等の自動起動エントリ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartupEntry {
    pub name: String,
    /// 登録コマンドから取り出した実行ファイルのフルパス。
    pub exe_path: String,
    pub machine_wide: bool,
    /// マシン全体でなければ、そのユーザーの SID（HKU 配下）。
    #[serde(default)]
    pub user_sid: Option<String>,
    /// StartupApproved 上で有効か。
    pub enabled: bool,
}

/// 解放前後の測定値（設計書7章）。PDH 取得不可の値は None。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MemSample {
    pub avail_phys: u64,
    pub standby: Option<u64>,
    pub modified: Option<u64>,
    pub commit_pct: f64,
}

/// 実行後（および実行前）60 秒間の負荷指標。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadSample {
    /// Pages Input/sec の平均（ハードフォルト相当）
    pub pages_input_per_sec: f64,
    pub disk_read_bytes_per_sec: f64,
}

/// 解放の契機（設計書3章のトリガー）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Trigger {
    Pressure,
    LowMemory,
    Idle,
    Schedule,
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Pressure => "pressure",
            Trigger::LowMemory => "low_memory",
            Trigger::Idle => "idle",
            Trigger::Schedule => "schedule",
            Trigger::Manual => "manual",
        }
    }
}
