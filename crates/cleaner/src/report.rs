use memopt_core::Level;
use serde::Serialize;

/// 失敗は記録して続行する（リトライはしない）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub target: String,
    pub code: i32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CleanReport {
    pub level: u8,
    pub attempted: usize,
    pub succeeded: usize,
    pub failures: Vec<Failure>,
    pub elapsed_ms: u128,
    /// 特権不足などでレベル自体が実行不可だった場合の理由。
    pub unavailable: Option<String>,
}

impl CleanReport {
    pub fn new(level: Level) -> Self {
        Self { level: level as u8, attempted: 0, succeeded: 0, failures: vec![], elapsed_ms: 0, unavailable: None }
    }
    pub fn unavailable(level: Level, why: impl Into<String>) -> Self {
        Self { unavailable: Some(why.into()), ..Self::new(level) }
    }
    pub fn record_ok(&mut self) {
        self.attempted += 1;
        self.succeeded += 1;
    }
    pub fn record_fail(&mut self, target: impl Into<String>, code: i32, reason: impl Into<String>) {
        self.attempted += 1;
        self.failures.push(Failure { target: target.into(), code, reason: reason.into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_recorded_and_counted_without_aborting() {
        let mut r = CleanReport::new(Level::L1);
        r.record_ok();
        r.record_fail("42", 5, "access denied");
        r.record_ok();
        assert_eq!((r.attempted, r.succeeded, r.failures.len()), (3, 2, 1));
    }

    #[test]
    fn unavailable_carries_reason() {
        let r = CleanReport::unavailable(Level::L3, "no privilege");
        assert_eq!(r.unavailable.as_deref(), Some("no privilege"));
        assert_eq!(r.attempted, 0);
    }
}
