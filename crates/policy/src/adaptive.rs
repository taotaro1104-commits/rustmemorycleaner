//! 効果測定に基づく自動調整（設計書7章）。単純なメモリクリーナーとの最大の違い:
//! 解放後にかえって遅くなった（ハードフォルトが増えた）なら、自動で控えめにする。純粋ロジック。
use memopt_core::config::Adaptive as Cfg;
use memopt_core::{Level, LoadSample, Trigger};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// 実行後 60 秒のハードフォルト率が実行前の `hard_fault_ratio` 倍以上かつ一定量以上。
    Counterproductive,
    /// 解放量が物理メモリの `min_effect_pct`% 未満。
    Ineffective,
}

/// 1 回の自動解放の結果。`freed_bytes` は「空きメモリの増加」か「スタンバイの減少」の大きい方。
/// （スタンバイは利用可能メモリに含まれるため、L2/L3 の効果は空きメモリの増加だけでは測れない）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Evaluation {
    pub trigger: Trigger,
    pub top_level: Level,
    pub freed_bytes: u64,
    pub total_phys: u64,
    pub load_before: Option<LoadSample>,
    pub load_after: Option<LoadSample>,
}

/// 調整の内容と、履歴に残す「なぜ控えめにしたか」。
#[derive(Debug, Clone, PartialEq)]
pub struct Adjustment {
    pub trigger: Trigger,
    pub reason: String,
}

pub fn judge(cfg: &Cfg, e: &Evaluation) -> Verdict {
    if let (Some(b), Some(a)) = (e.load_before, e.load_after) {
        let (pre, post) = (b.pages_input_per_sec, a.pages_input_per_sec);
        if post >= cfg.min_hard_faults_per_sec && post >= pre * cfg.hard_fault_ratio {
            return Verdict::Counterproductive;
        }
    }
    if e.total_phys > 0 && (e.freed_bytes as f64) * 100.0 < e.total_phys as f64 * cfg.min_effect_pct {
        return Verdict::Ineffective;
    }
    Verdict::Fine
}

#[derive(Debug, Clone, Default)]
struct State {
    mult: u32,
    cap: Option<Level>,
    skip_next: bool,
    since: u64,
}

const MAX_MULT: u32 = 1024;

#[derive(Default)]
pub struct Adjuster {
    states: HashMap<Trigger, State>,
}

fn label(t: Trigger) -> &'static str {
    match t {
        Trigger::Pressure => "メモリ圧迫",
        Trigger::LowMemory => "低メモリ通知",
        Trigger::Idle => "アイドル",
        Trigger::Schedule => "スケジュール",
        Trigger::Manual => "手動",
    }
}

impl Adjuster {
    fn expire(&mut self, now: u64, cfg: &Cfg) {
        let ttl = cfg.expire_hours * 3600;
        self.states.retain(|_, s| now.saturating_sub(s.since) < ttl);
    }

    /// 基本のクールダウンを、逆効果の履歴に応じて延長した秒数（上限 `max_cooldown_min`）。
    pub fn cooldown_secs(&mut self, t: Trigger, base: u64, now: u64, cfg: &Cfg) -> u64 {
        if !cfg.enabled {
            return base;
        }
        self.expire(now, cfg);
        let mult = self.states.get(&t).map_or(1, |s| s.mult.max(1)) as u64;
        (base * mult).min((cfg.max_cooldown_min * 60).max(base))
    }

    /// そのトリガーの自動実行を制限しているレベル（無ければ制限なし）。
    pub fn level_cap(&mut self, t: Trigger, now: u64, cfg: &Cfg) -> Option<Level> {
        if !cfg.enabled {
            return None;
        }
        self.expire(now, cfg);
        self.states.get(&t).and_then(|s| s.cap)
    }

    /// 「次回 1 回見送り」が立っていれば消費して true。
    pub fn take_skip(&mut self, t: Trigger) -> bool {
        self.states.get_mut(&t).is_some_and(|s| std::mem::take(&mut s.skip_next))
    }

    /// 結果を評価して調整を更新する。手動実行は利用者の明示操作なので調整しない。
    pub fn record(&mut self, now: u64, cfg: &Cfg, base_cooldown: u64, e: &Evaluation) -> Vec<Adjustment> {
        if !cfg.enabled || e.trigger == Trigger::Manual {
            return vec![];
        }
        self.expire(now, cfg);
        let t = e.trigger;
        match judge(cfg, e) {
            Verdict::Counterproductive => {
                let s = self.states.entry(t).or_default();
                s.mult = (s.mult.max(1) * 2).min(MAX_MULT);
                s.since = now;
                let mut reason = format!(
                    "L{} 実行後 60 秒のハードフォルトが {:.0}/s から {:.0}/s に増えたため、{}のクールダウンを延長しました",
                    e.top_level as u8,
                    e.load_before.map_or(0.0, |l| l.pages_input_per_sec),
                    e.load_after.map_or(0.0, |l| l.pages_input_per_sec),
                    label(t)
                );
                if e.top_level >= Level::L2 {
                    s.cap = Some(Level::L1);
                    reason.push_str("。自動実行は L1 までに制限します");
                }
                let mins = self.cooldown_secs(t, base_cooldown, now, cfg) / 60;
                reason.push_str(&format!("（現在 {mins} 分）"));
                vec![Adjustment { trigger: t, reason }]
            }
            Verdict::Ineffective => {
                let s = self.states.entry(t).or_default();
                s.skip_next = true;
                s.since = now;
                vec![Adjustment {
                    trigger: t,
                    reason: format!("解放量が物理メモリの {}% 未満だったため、次回の{}を 1 回見送ります", cfg.min_effect_pct, label(t)),
                }]
            }
            Verdict::Fine => {
                // 効果があり悪化もなければ、延長を段階的に緩める。
                if let Some(s) = self.states.get_mut(&t) {
                    s.mult = (s.mult / 2).max(1);
                    if s.mult == 1 && s.cap.is_none() && !s.skip_next {
                        self.states.remove(&t);
                    }
                }
                vec![]
            }
        }
    }

    /// 利用者が手動で通常の動作に戻す。
    pub fn reset(&mut self) {
        self.states.clear();
    }

    /// 現在有効な調整の説明（GUI 表示用）。
    pub fn describe(&mut self, now: u64, cfg: &Cfg, base_cooldown: u64) -> Vec<String> {
        self.expire(now, cfg);
        let mut keys: Vec<Trigger> = self.states.keys().copied().collect();
        keys.sort_by_key(|k| k.as_str());
        keys.into_iter()
            .map(|t| {
                let s = self.states[&t].clone();
                let mut parts = Vec::new();
                if s.mult > 1 {
                    parts.push(format!("クールダウン {} 分", self.cooldown_secs(t, base_cooldown, now, cfg) / 60));
                }
                if let Some(c) = s.cap {
                    parts.push(format!("L{} まで", c as u8));
                }
                if s.skip_next {
                    parts.push("次回 1 回見送り".into());
                }
                format!("{}: {}", label(t), parts.join("、"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Cfg {
        Cfg::default() // ratio 2.0, min 100/s, max cooldown 60 分, 効果 1%, 有効期間 24h
    }

    fn load(p: f64) -> Option<LoadSample> {
        Some(LoadSample { pages_input_per_sec: p, disk_read_bytes_per_sec: 0.0 })
    }

    fn ev(trigger: Trigger, level: Level, freed_mb: u64, before: f64, after: f64) -> Evaluation {
        Evaluation { trigger, top_level: level, freed_bytes: freed_mb << 20, total_phys: 16 << 30, load_before: load(before), load_after: load(after) }
    }

    #[test]
    fn counterproductive_needs_both_ratio_and_absolute_floor() {
        let c = cfg();
        assert_eq!(judge(&c, &ev(Trigger::Pressure, Level::L1, 500, 100.0, 200.0)), Verdict::Counterproductive); // ちょうど 2 倍・100/s 以上
        assert_eq!(judge(&c, &ev(Trigger::Pressure, Level::L1, 500, 100.0, 199.0)), Verdict::Fine); // 2 倍未満
        assert_eq!(judge(&c, &ev(Trigger::Pressure, Level::L1, 500, 10.0, 60.0)), Verdict::Fine); // 6 倍でも 100/s 未満はノイズ
        assert_eq!(judge(&c, &ev(Trigger::Pressure, Level::L1, 500, 0.0, 150.0)), Verdict::Counterproductive); // 実行前 0 でも下限超えなら
    }

    #[test]
    fn ineffective_when_freed_below_one_percent_and_counterproductive_wins() {
        let c = cfg(); // 16GB の 1% = 163.84MB
        assert_eq!(judge(&c, &ev(Trigger::Idle, Level::L1, 100, 10.0, 10.0)), Verdict::Ineffective);
        assert_eq!(judge(&c, &ev(Trigger::Idle, Level::L1, 164, 10.0, 10.0)), Verdict::Fine);
        assert_eq!(judge(&c, &ev(Trigger::Idle, Level::L1, 1, 50.0, 500.0)), Verdict::Counterproductive);
        // 負荷が測れていなくても効果の判定はできる
        let e = Evaluation { load_before: None, load_after: None, ..ev(Trigger::Idle, Level::L1, 1, 0.0, 0.0) };
        assert_eq!(judge(&c, &e), Verdict::Ineffective);
    }

    #[test]
    fn repeated_counterproductive_doubles_cooldown_up_to_the_cap() {
        let (c, mut a) = (cfg(), Adjuster::default());
        let bad = ev(Trigger::Pressure, Level::L1, 500, 100.0, 400.0);
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 0, &c), 300);
        let adj = a.record(10, &c, 300, &bad);
        assert_eq!(adj.len(), 1);
        assert!(adj[0].reason.contains("10 分"), "{}", adj[0].reason); // 300s×2 = 10 分
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 10, &c), 600);
        for _ in 0..10 {
            a.record(20, &c, 300, &bad);
        }
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 20, &c), 3600); // 上限 60 分
        assert_eq!(a.cooldown_secs(Trigger::Idle, 300, 20, &c), 300); // 他のトリガーには影響しない
    }

    #[test]
    fn counterproductive_at_l2_caps_that_trigger_at_l1() {
        let (c, mut a) = (cfg(), Adjuster::default());
        let adj = a.record(5, &c, 300, &ev(Trigger::LowMemory, Level::L2, 500, 100.0, 400.0));
        assert!(adj[0].reason.contains("L1 までに制限"));
        assert_eq!(a.level_cap(Trigger::LowMemory, 6, &c), Some(Level::L1));
        assert_eq!(a.level_cap(Trigger::Pressure, 6, &c), None);
        // L1 での逆効果では上限は付かない
        a.reset();
        a.record(5, &c, 300, &ev(Trigger::Pressure, Level::L1, 500, 100.0, 400.0));
        assert_eq!(a.level_cap(Trigger::Pressure, 6, &c), None);
    }

    #[test]
    fn ineffective_sets_a_single_skip_that_is_consumed_once() {
        let (c, mut a) = (cfg(), Adjuster::default());
        let adj = a.record(1, &c, 300, &ev(Trigger::Idle, Level::L1, 10, 5.0, 5.0));
        assert!(adj[0].reason.contains("1 回見送ります"));
        assert!(a.take_skip(Trigger::Idle));
        assert!(!a.take_skip(Trigger::Idle));
        assert!(!a.take_skip(Trigger::Pressure));
    }

    #[test]
    fn manual_runs_and_disabled_config_never_adjust() {
        let (mut c, mut a) = (cfg(), Adjuster::default());
        assert!(a.record(1, &c, 300, &ev(Trigger::Manual, Level::L3, 0, 1.0, 999.0)).is_empty());
        c.enabled = false;
        assert!(a.record(1, &c, 300, &ev(Trigger::Pressure, Level::L2, 0, 1.0, 999.0)).is_empty());
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 1, &c), 300);
    }

    #[test]
    fn fine_results_gradually_relax_the_extension() {
        let (c, mut a) = (cfg(), Adjuster::default());
        let bad = ev(Trigger::Pressure, Level::L1, 500, 100.0, 400.0);
        a.record(1, &c, 300, &bad);
        a.record(2, &c, 300, &bad); // ×4
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 3, &c), 1200);
        let fine = ev(Trigger::Pressure, Level::L1, 900, 100.0, 100.0);
        a.record(4, &c, 300, &fine);
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 5, &c), 600);
        a.record(6, &c, 300, &fine);
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 7, &c), 300);
        assert!(a.describe(7, &c, 300).is_empty());
    }

    #[test]
    fn adjustments_expire_and_can_be_reset_manually() {
        let (c, mut a) = (cfg(), Adjuster::default());
        a.record(0, &c, 300, &ev(Trigger::Pressure, Level::L2, 500, 100.0, 400.0));
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 24 * 3600 - 1, &c), 600);
        assert_eq!(a.cooldown_secs(Trigger::Pressure, 300, 24 * 3600, &c), 300); // 24 時間で元に戻る
        assert_eq!(a.level_cap(Trigger::Pressure, 24 * 3600, &c), None);
        a.record(100_000, &c, 300, &ev(Trigger::Idle, Level::L1, 1, 1.0, 1.0));
        a.reset();
        assert!(!a.take_skip(Trigger::Idle));
        assert!(a.describe(100_001, &c, 300).is_empty());
    }

    #[test]
    fn describe_lists_active_adjustments_for_the_ui() {
        let (c, mut a) = (cfg(), Adjuster::default());
        a.record(1, &c, 300, &ev(Trigger::Pressure, Level::L2, 500, 100.0, 400.0));
        a.record(1, &c, 300, &ev(Trigger::Idle, Level::L1, 1, 1.0, 1.0));
        let d = a.describe(2, &c, 300);
        assert_eq!(d.len(), 2);
        assert!(d.iter().any(|s| s.contains("メモリ圧迫") && s.contains("クールダウン 10 分") && s.contains("L1 まで")));
        assert!(d.iter().any(|s| s.contains("アイドル") && s.contains("次回 1 回見送り")));
    }
}
