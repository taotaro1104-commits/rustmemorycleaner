//! トリガー判定・抑止・クールダウン・段階実行（設計書3章・6章）。純粋ロジック。
//! 時刻はすべて呼び出し側が渡す単調増加の秒（実機なしでテストできる）。
use crate::adaptive::{Adjuster, Adjustment, Evaluation};
use memopt_core::{Config, Level, MemorySnapshot, Trigger};
use std::collections::VecDeque;

const VERIFY_AFTER_SECS: u64 = 60;
const HOUR_SECS: u64 = 3600;
/// コミット率の解除側マージン（コミット側にもヒステリシスを付ける）。
const COMMIT_RELEASE_MARGIN: f64 = 5.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub now: u64,
    pub mem: MemorySnapshot,
    pub uptime_secs: u64,
    pub user_idle_secs: Option<u64>,
    pub fullscreen: bool,
    pub disk_busy_pct: Option<f64>,
    pub battery_saver: bool,
    pub low_memory_signal: bool,
    /// (分単位の重複排除キー, レベル)。cron 一致を呼び出し側が計算する。
    pub schedule_due: Option<(u64, Level)>,
    pub manual: Option<Level>,
    pub paused: bool,
}

impl Input {
    pub fn quiet(now: u64, mem: MemorySnapshot) -> Self {
        Self {
            now, mem, uptime_secs: 100_000, user_idle_secs: Some(0), fullscreen: false, disk_busy_pct: None,
            battery_saver: false, low_memory_signal: false, schedule_due: None, manual: None, paused: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Startup,
    Fullscreen,
    DiskBusy,
    BatterySaver,
    Paused,
    EmergencyStop,
    Cooldown,
    HourlyCap,
    /// 自動調整（効果が小さかった）による 1 回の見送り。
    AdaptiveSkip,
}

impl SkipReason {
    pub fn text(self) -> &'static str {
        match self {
            Self::Startup => "起動直後のため",
            Self::Fullscreen => "全画面/プレゼン中のため",
            Self::DiskBusy => "ディスク使用率が高いため",
            Self::BatterySaver => "バッテリー駆動の省電力モードのため",
            Self::Paused => "一時停止中のため",
            Self::EmergencyStop => "緊急停止中のため",
            Self::Cooldown => "クールダウン中のため",
            Self::HourlyCap => "1 時間あたりの自動解放の上限に達したため",
            Self::AdaptiveSkip => "前回の効果が小さかったため（自動調整）",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// 何も起きていない（ログも不要）。
    None,
    /// 実行する。`levels` を順に実行する（例: 低メモリ通知 = [L1, L2]）。
    Run { trigger: Trigger, levels: Vec<Level> },
    /// 発動条件は成立したが抑止された。履歴に理由を残す。
    Skip { trigger: Trigger, reason: SkipReason },
}

#[derive(Debug, Clone, Copy)]
struct Chain {
    trigger: Trigger,
    last_level: Level,
    verify_at: u64,
}

pub struct Engine {
    cfg: Config,
    armed: bool,
    cond_since: Option<u64>,
    chain: Option<Chain>,
    cooldown_until: u64,
    runs: VecDeque<u64>,
    idle_fired: bool,
    last_schedule_key: Option<u64>,
    emergency_stop: bool,
    adj: Adjuster,
}

fn next_level(l: Level) -> Option<Level> {
    match l {
        Level::L1 => Some(Level::L2),
        Level::L2 => Some(Level::L3),
        Level::L3 => None,
    }
}

fn to_level(n: u8) -> Level {
    match n {
        1 => Level::L1,
        2 => Level::L2,
        _ => Level::L3,
    }
}

impl Engine {
    pub fn new(cfg: &Config) -> Self {
        Self {
            cfg: cfg.clone(), armed: true, cond_since: None, chain: None, cooldown_until: 0,
            runs: VecDeque::new(), idle_fired: false, last_schedule_key: None, emergency_stop: false,
            adj: Adjuster::default(),
        }
    }

    pub fn update_config(&mut self, cfg: &Config) {
        self.cfg = cfg.clone();
    }

    pub fn set_emergency_stop(&mut self, on: bool) {
        self.emergency_stop = on;
    }

    pub fn emergency_stopped(&self) -> bool {
        self.emergency_stop
    }

    /// 直近 1 時間の自動実行回数。
    pub fn runs_last_hour(&mut self, now: u64) -> usize {
        self.trim(now);
        self.runs.len()
    }

    fn trim(&mut self, now: u64) {
        while self.runs.front().is_some_and(|t| now.saturating_sub(*t) >= HOUR_SECS) {
            self.runs.pop_front();
        }
    }

    fn raw_pressure(&self, m: &MemorySnapshot) -> bool {
        let p = &self.cfg.auto_clean.pressure;
        m.avail_pct() < p.available_below_pct || m.commit_pct() > p.commit_above_pct
    }

    fn released(&self, m: &MemorySnapshot) -> bool {
        let p = &self.cfg.auto_clean.pressure;
        m.avail_pct() >= p.release_above_pct && m.commit_pct() <= p.commit_above_pct - COMMIT_RELEASE_MARGIN
    }

    fn suppression(&self, i: &Input) -> Option<SkipReason> {
        let s = &self.cfg.suppress;
        if self.emergency_stop {
            return Some(SkipReason::EmergencyStop);
        }
        if i.paused {
            return Some(SkipReason::Paused);
        }
        if i.uptime_secs < self.cfg.monitor.startup_grace_sec {
            return Some(SkipReason::Startup);
        }
        if s.fullscreen && i.fullscreen {
            return Some(SkipReason::Fullscreen);
        }
        if i.disk_busy_pct.is_some_and(|d| d > s.disk_busy_pct) {
            return Some(SkipReason::DiskBusy);
        }
        if s.on_battery_saver && i.battery_saver {
            return Some(SkipReason::BatterySaver);
        }
        None
    }

    /// 逆効果の履歴があれば延長したクールダウンを開始する。
    fn start_cooldown(&mut self, now: u64, trigger: Trigger) {
        let base = self.cfg.auto_clean.cooldown_sec;
        self.cooldown_until = now + self.adj.cooldown_secs(trigger, base, now, &self.cfg.adaptive);
        self.chain = None;
    }

    /// 自動実行の上限レベル: 設定の上限と、自動調整による制限のうち低い方。
    fn auto_cap(&mut self, trigger: Trigger, now: u64) -> Level {
        let max = to_level(self.cfg.auto_clean.max_auto_level);
        self.adj.level_cap(trigger, now, &self.cfg.adaptive).map_or(max, |c| c.min(max))
    }

    /// 60 秒後の測定結果を渡す。調整が入った場合は「なぜ控えめにしたか」を返す（履歴に残す）。
    pub fn record_result(&mut self, now: u64, e: &Evaluation) -> Vec<Adjustment> {
        let base = self.cfg.auto_clean.cooldown_sec;
        self.adj.record(now, &self.cfg.adaptive, base, e)
    }

    /// 利用者が自動調整を手動で解除する。
    pub fn reset_adaptive(&mut self) {
        self.adj.reset();
    }

    /// 現在有効な自動調整の説明（画面表示用）。
    pub fn adaptive_notes(&mut self, now: u64) -> Vec<String> {
        let base = self.cfg.auto_clean.cooldown_sec;
        self.adj.describe(now, &self.cfg.adaptive, base)
    }

    pub fn evaluate(&mut self, i: &Input) -> Decision {
        // 再武装（ヒステリシス）と継続時間の追跡は抑止中も進める。
        if !self.armed && self.released(&i.mem) {
            self.armed = true;
            self.cond_since = None;
        }
        if self.armed {
            if self.raw_pressure(&i.mem) {
                self.cond_since.get_or_insert(i.now);
            } else {
                self.cond_since = None;
            }
        }
        if i.user_idle_secs.is_some_and(|s| s < 60) {
            self.idle_fired = false; // ユーザーが戻ったら次のアイドル期間に備える
        }

        // 手動は抑止・クールダウン・上限の対象外（ユーザーの明示操作）。
        if let Some(l) = i.manual {
            return Decision::Run { trigger: Trigger::Manual, levels: vec![l] };
        }
        if !self.cfg.auto_clean.enabled && !self.emergency_stop {
            return Decision::None;
        }

        self.trim(i.now);
        let sustain = self.cfg.auto_clean.pressure.sustain_sec;

        // 1) 実行中のチェーンの検証待ち / 段階上げ
        if let Some(ch) = self.chain {
            if i.now < ch.verify_at {
                return Decision::None; // 60 秒後の評価待ち
            }
            let still = self.raw_pressure(&i.mem);
            let cap = self.auto_cap(ch.trigger, i.now);
            match next_level(ch.last_level).filter(|n| still && *n <= cap) {
                Some(next) => return self.guarded(i, ch.trigger, vec![next], Some(next), true),
                None => self.start_cooldown(i.now, ch.trigger),
            }
        }

        // 2) 低メモリ通知 → L1 + L2
        if i.low_memory_signal {
            let cap = self.auto_cap(Trigger::LowMemory, i.now);
            let levels = [Level::L1, Level::L2].into_iter().filter(|l| *l <= cap).collect::<Vec<_>>();
            if !levels.is_empty() {
                let last = *levels.last().unwrap();
                return self.guarded(i, Trigger::LowMemory, levels, Some(last), true);
            }
        }

        // 3) メモリ圧迫（継続時間を満たしたときだけ、武装中に 1 回）
        if self.armed && self.cond_since.is_some_and(|t| i.now - t >= sustain) {
            return self.guarded(i, Trigger::Pressure, vec![Level::L1], Some(Level::L1), true);
        }

        // 4) スケジュール（設定されたレベルをそのまま実行。クールダウンの対象外）
        if let Some((key, level)) = i.schedule_due {
            if self.last_schedule_key != Some(key) {
                let d = self.guarded(i, Trigger::Schedule, vec![level], None, false);
                if matches!(d, Decision::Run { .. }) {
                    self.last_schedule_key = Some(key);
                }
                return d;
            }
        }

        // 5) アイドル（無操作が続いている間に 1 回）
        let idle = &self.cfg.auto_clean.idle;
        if idle.enabled && !self.idle_fired && i.user_idle_secs.is_some_and(|s| s >= idle.idle_min * 60) {
            let level = to_level(idle.level).min(self.auto_cap(Trigger::Idle, i.now));
            let d = self.guarded(i, Trigger::Idle, vec![level], None, true);
            if matches!(d, Decision::Run { .. }) {
                self.idle_fired = true;
            }
            return d;
        }
        Decision::None
    }

    /// 抑止・クールダウン・1 時間上限を確認して Run/Skip を返す。
    fn guarded(&mut self, i: &Input, trigger: Trigger, levels: Vec<Level>, chain_level: Option<Level>, use_cooldown: bool) -> Decision {
        if let Some(reason) = self.suppression(i) {
            return Decision::Skip { trigger, reason };
        }
        if use_cooldown && i.now < self.cooldown_until {
            return Decision::Skip { trigger, reason: SkipReason::Cooldown };
        }
        if self.runs.len() >= self.cfg.auto_clean.max_runs_per_hour as usize {
            return Decision::Skip { trigger, reason: SkipReason::HourlyCap };
        }
        // 前回の効果が小さかったトリガーは 1 回見送る。見送った分も「発動した」扱いにして、
        // 15 秒後にすぐ再発動しないようにする（クールダウンを開始し、圧迫/アイドルは再武装まで待つ）。
        if use_cooldown && self.adj.take_skip(trigger) {
            match trigger {
                Trigger::Pressure | Trigger::LowMemory => {
                    self.armed = false;
                    self.cond_since = None;
                }
                Trigger::Idle => self.idle_fired = true,
                _ => {}
            }
            self.start_cooldown(i.now, trigger);
            return Decision::Skip { trigger, reason: SkipReason::AdaptiveSkip };
        }
        self.runs.push_back(i.now);
        if trigger == Trigger::Pressure || trigger == Trigger::LowMemory {
            self.armed = false;
            self.cond_since = None;
        }
        if let Some(l) = chain_level {
            self.chain = Some(Chain { trigger, last_level: l, verify_at: i.now + VERIFY_AFTER_SECS });
        } else if use_cooldown {
            self.start_cooldown(i.now, trigger);
        }
        Decision::Run { trigger, levels }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
