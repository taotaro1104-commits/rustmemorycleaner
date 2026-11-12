//! config.toml（設計書8章）。不正な値は既定値に戻し、警告を返す。
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Monitor {
    pub eval_interval_sec: u64,
    pub process_scan_sec: u64,
    pub startup_grace_sec: u64,
}
impl Default for Monitor {
    fn default() -> Self { Self { eval_interval_sec: 15, process_scan_sec: 5, startup_grace_sec: 300 } }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pressure {
    pub available_below_pct: f64,
    pub release_above_pct: f64,
    pub commit_above_pct: f64,
    pub sustain_sec: u64,
}
impl Default for Pressure {
    fn default() -> Self {
        Self { available_below_pct: 15.0, release_above_pct: 25.0, commit_above_pct: 85.0, sustain_sec: 30 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Idle {
    pub enabled: bool,
    pub idle_min: u64,
    pub level: u8,
}
impl Default for Idle {
    fn default() -> Self { Self { enabled: true, idle_min: 10, level: 1 } }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub cron: String,
    pub level: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoClean {
    pub enabled: bool,
    pub max_auto_level: u8,
    pub cooldown_sec: u64,
    pub max_runs_per_hour: u32,
    pub pressure: Pressure,
    pub idle: Idle,
    pub schedule: Vec<Schedule>,
}
impl Default for AutoClean {
    fn default() -> Self {
        Self {
            enabled: true, max_auto_level: 2, cooldown_sec: 300, max_runs_per_hour: 6,
            pressure: Pressure::default(), idle: Idle::default(),
            schedule: vec![Schedule { cron: "0 3 * * *".into(), level: 2 }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Suppress {
    pub fullscreen: bool,
    pub disk_busy_pct: f64,
    pub on_battery_saver: bool,
}
impl Default for Suppress {
    fn default() -> Self { Self { fullscreen: true, disk_busy_pct: 70.0, on_battery_saver: true } }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Adaptive {
    pub enabled: bool,
    pub hard_fault_ratio: f64,
    /// 「逆効果」とみなすのに必要な、実行後のハードフォルト率の下限（回/秒）。ノイズ程度の増減で判定しない。
    pub min_hard_faults_per_sec: f64,
    pub max_cooldown_min: u64,
    /// 効果が小さい（解放量が物理メモリのこの割合未満）と判定する閾値（%）。
    pub min_effect_pct: f64,
    /// 自動調整の有効期間（時間）。これを過ぎたら通常の動作に戻す。
    pub expire_hours: u64,
}
impl Default for Adaptive {
    fn default() -> Self { Self { enabled: true, hard_fault_ratio: 2.0, min_hard_faults_per_sec: 100.0, max_cooldown_min: 60, min_effect_pct: 1.0, expire_hours: 24 } }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct L1Cfg {
    pub max_processes: usize,
    pub skip_foreground: bool,
    pub skip_audio: bool,
    pub min_idle_cpu_sec: u64,
}
impl Default for L1Cfg {
    fn default() -> Self { Self { max_processes: 50, skip_foreground: true, skip_audio: true, min_idle_cpu_sec: 30 } }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Protect {
    pub user_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub tray_shows_percent: bool,
    pub hotkey_clean: String,
    pub notify_on_auto: bool,
}
impl Default for Ui {
    fn default() -> Self { Self { tray_shows_percent: true, hotkey_clean: "Ctrl+Alt+M".into(), notify_on_auto: false } }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub monitor: Monitor,
    pub auto_clean: AutoClean,
    pub suppress: Suppress,
    pub adaptive: Adaptive,
    pub l1: L1Cfg,
    pub protect: Protect,
    pub ui: Ui,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION, monitor: Monitor::default(), auto_clean: AutoClean::default(),
            suppress: Suppress::default(), adaptive: Adaptive::default(), l1: L1Cfg::default(),
            protect: Protect::default(), ui: Ui::default(),
        }
    }
}

/// 範囲外なら既定値に戻して警告を積む。
fn fix<T: PartialOrd + Copy + std::fmt::Display>(v: &mut T, lo: T, hi: T, def: T, name: &str, w: &mut Vec<String>) {
    if !(*v >= lo && *v <= hi) {
        w.push(format!("{name}={v} は範囲外（{lo}〜{hi}）。既定値 {def} に戻しました"));
        *v = def;
    }
}

impl Config {
    /// 解析して検証。解析不能ならまるごと既定値＋警告。
    pub fn parse_and_validate(text: &str) -> (Config, Vec<String>) {
        match toml::from_str::<Config>(text) {
            Ok(mut c) => {
                let w = c.validate();
                (c, w)
            }
            Err(e) => (Config::default(), vec![format!("config.toml を解析できません: {e}。既定値を使います")]),
        }
    }

    pub fn validate(&mut self) -> Vec<String> {
        let (d, mut w) = (Config::default(), Vec::new());
        fix(&mut self.monitor.eval_interval_sec, 5, 3600, d.monitor.eval_interval_sec, "monitor.eval_interval_sec", &mut w);
        fix(&mut self.monitor.process_scan_sec, 2, 3600, d.monitor.process_scan_sec, "monitor.process_scan_sec", &mut w);
        fix(&mut self.monitor.startup_grace_sec, 0, 3600, d.monitor.startup_grace_sec, "monitor.startup_grace_sec", &mut w);
        let a = &mut self.auto_clean;
        fix(&mut a.max_auto_level, 1, 3, d.auto_clean.max_auto_level, "auto_clean.max_auto_level", &mut w);
        fix(&mut a.cooldown_sec, 30, 86_400, d.auto_clean.cooldown_sec, "auto_clean.cooldown_sec", &mut w);
        fix(&mut a.max_runs_per_hour, 1, 60, d.auto_clean.max_runs_per_hour, "auto_clean.max_runs_per_hour", &mut w);
        let p = &mut a.pressure;
        fix(&mut p.available_below_pct, 1.0, 90.0, 15.0, "pressure.available_below_pct", &mut w);
        fix(&mut p.commit_above_pct, 30.0, 100.0, 85.0, "pressure.commit_above_pct", &mut w);
        fix(&mut p.sustain_sec, 0, 3600, 30, "pressure.sustain_sec", &mut w);
        fix(&mut p.release_above_pct, 2.0, 95.0, 25.0, "pressure.release_above_pct", &mut w);
        // ヒステリシス: 解除は発動より大きくなければ境界で連続発動する。
        if p.release_above_pct <= p.available_below_pct {
            w.push(format!("release_above_pct({}) は available_below_pct({}) より大きくする必要があります。既定値に戻しました",
                p.release_above_pct, p.available_below_pct));
            p.available_below_pct = 15.0;
            p.release_above_pct = 25.0;
        }
        fix(&mut a.idle.level, 1, 3, 1, "idle.level", &mut w);
        fix(&mut a.idle.idle_min, 1, 1440, 10, "idle.idle_min", &mut w);
        a.schedule.retain(|s| {
            let ok = (1..=3).contains(&s.level) && s.cron.split_whitespace().count() == 5;
            if !ok { w.push(format!("schedule '{}' level {} は不正なので無視しました", s.cron, s.level)); }
            ok
        });
        fix(&mut self.suppress.disk_busy_pct, 10.0, 100.0, 70.0, "suppress.disk_busy_pct", &mut w);
        fix(&mut self.adaptive.hard_fault_ratio, 1.1, 100.0, 2.0, "adaptive.hard_fault_ratio", &mut w);
        fix(&mut self.adaptive.max_cooldown_min, 1, 1440, 60, "adaptive.max_cooldown_min", &mut w);
        fix(&mut self.adaptive.min_hard_faults_per_sec, 0.0, 1e6, 100.0, "adaptive.min_hard_faults_per_sec", &mut w);
        fix(&mut self.adaptive.min_effect_pct, 0.0, 50.0, 1.0, "adaptive.min_effect_pct", &mut w);
        fix(&mut self.adaptive.expire_hours, 1, 24 * 30, 24, "adaptive.expire_hours", &mut w);
        fix(&mut self.l1.max_processes, 1, 500, 50, "l1.max_processes", &mut w);
        w
    }

    /// 自動実行の上限レベルを Level に（検証済み前提）。
    pub fn max_auto_level(&self) -> crate::Level {
        match self.auto_clean.max_auto_level { 1 => crate::Level::L1, 2 => crate::Level::L2, _ => crate::Level::L3 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
schema_version = 1
[monitor]
eval_interval_sec = 15
[auto_clean]
enabled = true
max_auto_level = 2
[auto_clean.pressure]
available_below_pct = 15
release_above_pct = 25
commit_above_pct = 85
sustain_sec = 30
[[auto_clean.schedule]]
cron = "0 3 * * *"
level = 2
[protect]
user_paths = ['C:\Program Files\OBS Studio\bin\64bit\obs64.exe']
"#;

    #[test]
    fn spec_example_parses_without_warnings() {
        let (c, w) = Config::parse_and_validate(SPEC);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.protect.user_paths.len(), 1);
        assert_eq!(c.auto_clean.pressure.sustain_sec, 30);
    }

    #[test]
    fn empty_file_gives_defaults() {
        let (c, w) = Config::parse_and_validate("");
        assert!(w.is_empty());
        assert_eq!(c, Config::default());
    }

    #[test]
    fn out_of_range_values_reset_to_default_with_warning() {
        let (c, w) = Config::parse_and_validate("[auto_clean]\nmax_auto_level = 9\ncooldown_sec = 1\n[l1]\nmax_processes = 0");
        assert_eq!(c.auto_clean.max_auto_level, 2);
        assert_eq!(c.auto_clean.cooldown_sec, 300);
        assert_eq!(c.l1.max_processes, 50);
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn hysteresis_must_have_release_above_trigger() {
        let (c, w) = Config::parse_and_validate("[auto_clean.pressure]\navailable_below_pct = 30\nrelease_above_pct = 20");
        assert_eq!((c.auto_clean.pressure.available_below_pct, c.auto_clean.pressure.release_above_pct), (15.0, 25.0));
        assert!(w.iter().any(|m| m.contains("release_above_pct")));
    }

    #[test]
    fn garbage_text_falls_back_to_defaults() {
        let (c, w) = Config::parse_and_validate("this is = = not toml [");
        assert_eq!(c, Config::default());
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn invalid_schedule_entries_are_dropped() {
        let (c, w) = Config::parse_and_validate("[[auto_clean.schedule]]\ncron = \"bad\"\nlevel = 2\n[[auto_clean.schedule]]\ncron = \"0 4 * * *\"\nlevel = 1");
        assert_eq!(c.auto_clean.schedule.len(), 1);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        let (_, w) = Config::parse_and_validate("future_key = 1\n[monitor]\nnew_thing = true");
        assert!(w.is_empty());
    }

    #[test]
    fn adaptive_thresholds_are_validated() {
        let (c, w) = Config::parse_and_validate("[adaptive]
min_effect_pct = 99
expire_hours = 0
min_hard_faults_per_sec = -5");
        assert_eq!((c.adaptive.min_effect_pct, c.adaptive.expire_hours, c.adaptive.min_hard_faults_per_sec), (1.0, 24, 100.0));
        assert_eq!(w.len(), 3);
    }
}
