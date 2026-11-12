//! 直近30分の使用量推移と「増加中」判定（メモリリーク候補, 設計書4章）。
use std::collections::{HashMap, VecDeque};

pub const WINDOW_SECS: u64 = 30 * 60;
const MIN_SAMPLES: usize = 6;
const MIN_SPAN_SECS: u64 = 5 * 60;
const MIN_GROWTH_BYTES: u64 = 50 << 20;
const MIN_GROWTH_RATIO: f64 = 1.10;
const MONOTONE_RATIO: f64 = 0.8;

#[derive(Default)]
pub struct TrendTracker {
    series: HashMap<String, VecDeque<(u64, u64)>>,
}

impl TrendTracker {
    pub fn new() -> Self { Self::default() }

    /// `samples`: (feature id, private bytes)。消えた機能の履歴は破棄、古い点は窓外で捨てる。
    pub fn record<'a>(&mut self, now_secs: u64, samples: impl IntoIterator<Item = (&'a str, u64)>) {
        let mut seen = Vec::new();
        for (id, bytes) in samples {
            seen.push(id.to_string());
            let q = self.series.entry(id.to_string()).or_default();
            q.push_back((now_secs, bytes));
            while q.front().is_some_and(|(t, _)| now_secs.saturating_sub(*t) > WINDOW_SECS) {
                q.pop_front();
            }
        }
        self.series.retain(|k, _| seen.contains(k));
    }

    pub fn series(&self, id: &str) -> Vec<u64> {
        self.series.get(id).map(|q| q.iter().map(|(_, b)| *b).collect()).unwrap_or_default()
    }

    pub fn is_rising(&self, id: &str) -> bool {
        let Some(q) = self.series.get(id) else { return false };
        if q.len() < MIN_SAMPLES { return false; }
        let (t0, first) = *q.front().unwrap();
        let (t1, last) = *q.back().unwrap();
        if t1 - t0 < MIN_SPAN_SECS { return false; }
        if last < first + MIN_GROWTH_BYTES || (last as f64) < first as f64 * MIN_GROWTH_RATIO { return false; }
        let steps = q.len() - 1;
        let up = q.iter().zip(q.iter().skip(1)).filter(|(a, b)| b.1 >= a.1).count();
        up as f64 / steps as f64 >= MONOTONE_RATIO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MB: u64 = 1 << 20;

    fn feed(t: &mut TrendTracker, vals: &[u64], step: u64) {
        for (i, v) in vals.iter().enumerate() { t.record(i as u64 * step, [("a", *v * MB)]); }
    }

    #[test]
    fn steady_growth_is_rising() {
        let mut t = TrendTracker::new();
        feed(&mut t, &[100, 120, 140, 160, 180, 200, 220], 60);
        assert!(t.is_rising("a"));
    }
    #[test]
    fn flat_or_noisy_is_not_rising() {
        let mut t = TrendTracker::new();
        feed(&mut t, &[100, 102, 99, 101, 100, 103, 101], 60);
        assert!(!t.is_rising("a"));
    }
    #[test]
    fn too_few_samples_or_short_span_is_not_rising() {
        let mut t = TrendTracker::new();
        feed(&mut t, &[100, 200, 300], 60);
        assert!(!t.is_rising("a"));
        let mut t2 = TrendTracker::new();
        feed(&mut t2, &[100, 120, 140, 160, 180, 200, 220], 10); // 60秒しかない
        assert!(!t2.is_rising("a"));
    }
    #[test]
    fn growth_below_threshold_is_not_rising() {
        let mut t = TrendTracker::new();
        feed(&mut t, &[100, 101, 102, 103, 104, 105, 106], 60); // +6MB
        assert!(!t.is_rising("a"));
    }
    #[test]
    fn old_samples_drop_out_of_window_and_vanished_ids_are_forgotten() {
        let mut t = TrendTracker::new();
        t.record(0, [("a", 1)]);
        t.record(WINDOW_SECS + 100, [("a", 2)]);
        assert_eq!(t.series("a"), vec![2]);
        t.record(WINDOW_SECS + 200, [("b", 3)]);
        assert!(t.series("a").is_empty());
    }
}
