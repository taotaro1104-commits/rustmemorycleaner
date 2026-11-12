//! 5 フィールド cron（分 時 日 月 曜日）。`*`、数値、`a,b`、`a-b`、`*/n`、`a-b/n` に対応。
//! 曜日は 0=日〜6=土（7 も日曜）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    pub minute: u32,
    pub hour: u32,
    pub day: u32,
    pub month: u32,
    /// 0=日曜
    pub weekday: u32,
}

fn field_matches(spec: &str, value: u32, lo: u32, hi: u32) -> Option<bool> {
    let mut any = false;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|n| *n > 0)?),
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let n: u32 = range.parse().ok()?;
            if part.contains('/') { (n, hi) } else { (n, n) }
        };
        if a < lo || b > hi || a > b {
            return None;
        }
        if value >= a && value <= b && (value - a) % step == 0 {
            any = true;
        }
    }
    Some(any)
}

/// 構文が正しければ Some(一致したか)、不正なら None。
pub fn cron_matches(expr: &str, m: Moment) -> Option<bool> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return None;
    }
    let wd = if m.weekday == 7 { 0 } else { m.weekday };
    // 曜日の 7 は日曜として許容するため、範囲 0..=7 で評価
    let dow = field_matches(f[4], wd, 0, 7).or(None)? || (wd == 0 && field_matches(f[4], 7, 0, 7)?);
    Some(
        field_matches(f[0], m.minute, 0, 59)?
            && field_matches(f[1], m.hour, 0, 23)?
            && field_matches(f[2], m.day, 1, 31)?
            && field_matches(f[3], m.month, 1, 12)?
            && dow,
    )
}

/// 設定のスケジュール一覧から、この分に一致する最初のものを (重複排除キー, レベル) で返す。
/// 構文不正のエントリは無視（config 検証でも落とすが二重に守る）。
pub fn due_schedule(entries: &[memopt_core::config::Schedule], m: Moment, minute_key: u64) -> Option<(u64, memopt_core::Level)> {
    entries.iter().find(|e| cron_matches(&e.cron, m) == Some(true)).map(|e| {
        let level = match e.level {
            1 => memopt_core::Level::L1,
            2 => memopt_core::Level::L2,
            _ => memopt_core::Level::L3,
        };
        (minute_key, level)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: u32, hour: u32, day: u32, month: u32, weekday: u32) -> Moment {
        Moment { minute, hour, day, month, weekday }
    }

    #[test]
    fn daily_3am() {
        assert_eq!(cron_matches("0 3 * * *", at(0, 3, 15, 6, 2)), Some(true));
        assert_eq!(cron_matches("0 3 * * *", at(1, 3, 15, 6, 2)), Some(false));
        assert_eq!(cron_matches("0 3 * * *", at(0, 4, 15, 6, 2)), Some(false));
    }

    #[test]
    fn lists_ranges_and_steps() {
        assert_eq!(cron_matches("*/15 * * * *", at(45, 1, 1, 1, 1)), Some(true));
        assert_eq!(cron_matches("*/15 * * * *", at(40, 1, 1, 1, 1)), Some(false));
        assert_eq!(cron_matches("0,30 9-17 * * *", at(30, 12, 1, 1, 1)), Some(true));
        assert_eq!(cron_matches("0,30 9-17 * * *", at(30, 18, 1, 1, 1)), Some(false));
        assert_eq!(cron_matches("10-40/10 * * * *", at(30, 0, 1, 1, 1)), Some(true));
        assert_eq!(cron_matches("10-40/10 * * * *", at(35, 0, 1, 1, 1)), Some(false));
    }

    #[test]
    fn weekday_and_month_fields() {
        assert_eq!(cron_matches("0 3 * * 1-5", at(0, 3, 1, 1, 3)), Some(true)); // 水
        assert_eq!(cron_matches("0 3 * * 1-5", at(0, 3, 1, 1, 0)), Some(false)); // 日
        assert_eq!(cron_matches("0 3 * * 7", at(0, 3, 1, 1, 0)), Some(true)); // 7=日
        assert_eq!(cron_matches("0 3 1 12 *", at(0, 3, 1, 12, 4)), Some(true));
        assert_eq!(cron_matches("0 3 1 12 *", at(0, 3, 1, 11, 4)), Some(false));
    }

    #[test]
    fn invalid_syntax_returns_none() {
        for bad in ["", "0 3 * *", "60 3 * * *", "0 24 * * *", "a b c d e", "*/0 * * * *", "5-1 * * * *", "0 3 0 * *"] {
            assert_eq!(cron_matches(bad, at(0, 3, 1, 1, 1)), None, "{bad}");
        }
    }

    #[test]
    fn due_schedule_picks_first_match_and_ignores_invalid() {
        use memopt_core::config::Schedule;
        let es = vec![
            Schedule { cron: "bad".into(), level: 3 },
            Schedule { cron: "0 3 * * *".into(), level: 2 },
            Schedule { cron: "0 3 * * *".into(), level: 1 },
        ];
        assert_eq!(due_schedule(&es, at(0, 3, 1, 1, 1), 42), Some((42, memopt_core::Level::L2)));
        assert_eq!(due_schedule(&es, at(1, 3, 1, 1, 1), 43), None);
    }
}
