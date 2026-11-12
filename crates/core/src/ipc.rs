//! IPC メッセージ（設計書2章）。1 行 1 メッセージの JSON、全メッセージに protocol_version。
use crate::stop::{JournalEntry, StopMethod, StopPlan};
use crate::{Feature, Level, MemorySnapshot, Trigger};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const PIPE_NAME: &str = r"\\.\pipe\memopt-v1";
/// 1 メッセージ（1 行）の上限。これを超える入力は切断する。
pub const MAX_LINE_BYTES: usize = 4 << 20;

/// ユーザーセッション（トレイ）がサービスへ中継する状況。サービスはセッション 0 で
/// GetForegroundWindow / SHQueryUserNotificationState / GetLastInputInfo を使えないため。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub fullscreen: bool,
    pub user_idle_secs: u64,
    pub foreground_pid: Option<u32>,
    pub audio_pids: Vec<u32>,
    pub battery_saver: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: i64,
    pub ts_unix: i64,
    pub level: u8,
    pub trigger: String,
    pub avail_before: u64,
    pub avail_after: u64,
    pub standby_before: Option<u64>,
    pub standby_after: Option<u64>,
    pub avail_after60: Option<u64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Msg {
    // ---- GUI → service ----
    GetSnapshot,
    GetFeatures,
    GetHistory { limit: u32 },
    RunClean { level: Level },
    ReportSession(SessionInfo),
    SetPaused { secs: u64 },
    EmergencyStop { on: bool },
    GetConfig,
    UpdateConfig { toml: String },
    /// 停止の計画（使える手段・影響・安全分類）を問い合わせる。
    PlanStop { feature_id: String },
    /// 1 機能を 1 手段で停止する。永続変更は `confirmed`（2 段階確認済み）が必須。
    StopFeature { feature_id: String, method: StopMethod, target: Option<String>, confirmed: bool },
    /// GUI が行った正常終了の結果（成功/タイムアウト）を記録する。
    FinishGraceful { journal_id: u64, ok: bool },
    Undo { journal_id: u64 },
    UndoAll,
    GetJournal { limit: u32 },
    /// 自動調整（控えめにした設定）を手動で解除する。
    ResetAdaptive,
    // ---- service → GUI（応答）----
    Snapshot { mem: MemorySnapshot, auto_enabled: bool, paused: bool, emergency_stop: bool, #[serde(default)] adaptive: Vec<String> },
    Features { features: Vec<Feature> },
    History { runs: Vec<RunSummary> },
    Config { toml: String, warnings: Vec<String> },
    Plan { plan: StopPlan },
    /// `graceful_targets` は正常終了のときだけ入る（GUI が WM_CLOSE を送る対象 = (PID, 起動時刻)）。
    StopResult { entry: JournalEntry, graceful_targets: Vec<(u32, u64)> },
    Journal { entries: Vec<JournalEntry> },
    UndoDone { restored: Vec<u64>, failed: Vec<(u64, String)> },
    Ack,
    Error { code: String, message: String },
    // ---- service → GUI（通知）----
    CleanFinished { trigger: Trigger, run: RunSummary },
    Skipped { trigger: Trigger, reason: String },
    PressureChanged { active: bool },
    StopNotice { entry: JournalEntry },
    /// 自動調整が入った（なぜ控えめにしたか）。
    AdaptiveNotice { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub protocol_version: u32,
    /// 要求と応答を対応付ける。通知は None。
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(flatten)]
    pub msg: Msg,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DecodeError {
    TooLong,
    Malformed(String),
    /// 版ずれ。相手の版を返す（未知のメッセージ型でも検出できる）。
    VersionMismatch { theirs: u32 },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => write!(f, "メッセージが長すぎる"),
            Self::Malformed(e) => write!(f, "不正なメッセージ: {e}"),
            Self::VersionMismatch { theirs } => write!(f, "プロトコル版が違う（相手 v{theirs} / 自分 v{PROTOCOL_VERSION}）"),
        }
    }
}

impl Frame {
    pub fn new(id: Option<u64>, msg: Msg) -> Self {
        Self { protocol_version: PROTOCOL_VERSION, id, msg }
    }

    /// 末尾に改行を付けた 1 行の JSON。
    pub fn encode(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Frame は常にシリアライズできる");
        s.push('\n');
        s
    }

    pub fn decode(line: &str) -> Result<Frame, DecodeError> {
        if line.len() > MAX_LINE_BYTES {
            return Err(DecodeError::TooLong);
        }
        #[derive(Deserialize)]
        struct Ver {
            protocol_version: u32,
        }
        let v: Ver = serde_json::from_str(line).map_err(|e| DecodeError::Malformed(e.to_string()))?;
        if v.protocol_version != PROTOCOL_VERSION {
            return Err(DecodeError::VersionMismatch { theirs: v.protocol_version });
        }
        serde_json::from_str(line).map_err(|e| DecodeError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request_and_response() {
        for msg in [
            Msg::GetSnapshot,
            Msg::RunClean { level: Level::L2 },
            Msg::ReportSession(SessionInfo { fullscreen: true, user_idle_secs: 5, foreground_pid: Some(9), audio_pids: vec![1, 2], battery_saver: false }),
            Msg::Error { code: "denied".into(), message: "x".into() },
            Msg::Ack,
        ] {
            let f = Frame::new(Some(7), msg);
            let line = f.encode();
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
            assert_eq!(Frame::decode(line.trim_end()).unwrap(), f);
        }
    }

    #[test]
    fn wire_format_is_flat_with_version_and_type() {
        let line = Frame::new(Some(1), Msg::SetPaused { secs: 3600 }).encode();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(v["type"], "SetPaused");
        assert_eq!(v["secs"], 3600);
    }

    #[test]
    fn version_mismatch_detected_even_for_unknown_message_types() {
        let line = r#"{"protocol_version":99,"id":1,"type":"FutureThing","x":1}"#;
        assert_eq!(Frame::decode(line), Err(DecodeError::VersionMismatch { theirs: 99 }));
    }

    #[test]
    fn malformed_and_missing_version_are_rejected() {
        assert!(matches!(Frame::decode("not json"), Err(DecodeError::Malformed(_))));
        assert!(matches!(Frame::decode(r#"{"type":"GetSnapshot"}"#), Err(DecodeError::Malformed(_))));
        let bad_level = r#"{"protocol_version":1,"type":"RunClean","level":"L9"}"#;
        assert!(matches!(Frame::decode(bad_level), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn oversized_line_is_rejected() {
        let big = "x".repeat(MAX_LINE_BYTES + 1);
        assert_eq!(Frame::decode(&big), Err(DecodeError::TooLong));
    }

    #[test]
    fn notification_has_no_id() {
        let line = Frame::new(None, Msg::PressureChanged { active: true }).encode();
        assert_eq!(Frame::decode(line.trim_end()).unwrap().id, None);
    }
}
