//! 15 秒ごとの負荷サンプルのリングバッファ（設計書7章「常時記録しているリングバッファ」）。
//! 実行前 60 秒間と実行後 60 秒間の平均を、これから取り出す。
use memopt_core::LoadSample;
use std::collections::VecDeque;

const CAPACITY: usize = 40; // 15 秒間隔で 10 分

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// サービス起動からの秒。
    pub t: u64,
    pub load: LoadSample,
    pub disk_busy_pct: Option<f64>,
}

#[derive(Default)]
pub struct LoadRing {
    v: VecDeque<Sample>,
}

impl LoadRing {
    pub fn push(&mut self, s: Sample) {
        self.v.push_back(s);
        while self.v.len() > CAPACITY {
            self.v.pop_front();
        }
    }

    /// (from, to] の平均。該当サンプルが無ければ None。
    pub fn avg_load(&self, from: u64, to: u64) -> Option<LoadSample> {
        let xs: Vec<&Sample> = self.v.iter().filter(|s| s.t > from && s.t <= to).collect();
        if xs.is_empty() {
            return None;
        }
        let n = xs.len() as f64;
        Some(LoadSample {
            pages_input_per_sec: xs.iter().map(|s| s.load.pages_input_per_sec).sum::<f64>() / n,
            disk_read_bytes_per_sec: xs.iter().map(|s| s.load.disk_read_bytes_per_sec).sum::<f64>() / n,
        })
    }

    /// 直近 `window` 秒のディスク使用率の平均（抑止判定用）。値が取れていなければ None。
    pub fn avg_disk_busy(&self, now: u64, window: u64) -> Option<f64> {
        let xs: Vec<f64> = self
            .v
            .iter()
            .filter(|s| s.t + window >= now)
            .filter_map(|s| s.disk_busy_pct)
            .collect();
        (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t: u64, pages: f64, disk: Option<f64>) -> Sample {
        Sample { t, load: LoadSample { pages_input_per_sec: pages, disk_read_bytes_per_sec: pages * 10.0 }, disk_busy_pct: disk }
    }

    #[test]
    fn avg_load_uses_half_open_window() {
        let mut r = LoadRing::default();
        for (t, p) in [(15, 100.0), (30, 200.0), (45, 300.0), (60, 400.0)] {
            r.push(s(t, p, None));
        }
        let a = r.avg_load(15, 60).unwrap(); // 30,45,60 を含み 15 は含まない
        assert_eq!(a.pages_input_per_sec, 300.0);
        assert_eq!(a.disk_read_bytes_per_sec, 3000.0);
        assert!(r.avg_load(60, 120).is_none());
    }

    #[test]
    fn avg_disk_busy_ignores_old_and_missing_values() {
        let mut r = LoadRing::default();
        r.push(s(0, 0.0, Some(99.0))); // 古い
        r.push(s(70, 0.0, None));
        r.push(s(85, 0.0, Some(60.0)));
        r.push(s(100, 0.0, Some(80.0)));
        assert_eq!(r.avg_disk_busy(100, 30), Some(70.0));
        assert_eq!(r.avg_disk_busy(1000, 30), None);
    }

    #[test]
    fn capacity_is_bounded() {
        let mut r = LoadRing::default();
        for t in 0..100 {
            r.push(s(t, 1.0, None));
        }
        assert!(r.avg_load(0, 1000).is_some());
        assert!(r.avg_load(0, 59).is_none()); // 古い分は捨てられている
    }
}
