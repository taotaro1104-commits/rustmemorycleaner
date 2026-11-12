//! 表示用の整形。
pub fn gb(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.2} GB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", b as f64 / (1u64 << 20) as f64)
    }
}

/// 「3 分前」形式（履歴表示用。ローカル時刻の変換を避ける）。
pub fn ago(now_unix: i64, ts_unix: i64) -> String {
    let d = (now_unix - ts_unix).max(0);
    match d {
        0..=59 => format!("{d} 秒前"),
        60..=3599 => format!("{} 分前", d / 60),
        3600..=86_399 => format!("{} 時間前", d / 3600),
        _ => format!("{} 日前", d / 86_400),
    }
}

pub fn trigger_label(s: &str) -> &str {
    match s {
        "pressure" => "メモリ圧迫",
        "low_memory" => "低メモリ通知",
        "idle" => "アイドル",
        "schedule" => "スケジュール",
        "manual" => "手動",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gb_switches_units() {
        assert_eq!(gb(512 << 20), "512 MB");
        assert_eq!(gb(3 << 30), "3.00 GB");
    }

    #[test]
    fn ago_buckets_and_future_timestamps() {
        assert_eq!(ago(100, 70), "30 秒前");
        assert_eq!(ago(1000, 700), "5 分前");
        assert_eq!(ago(10_000, 0), "2 時間前");
        assert_eq!(ago(200_000, 0), "2 日前");
        assert_eq!(ago(10, 50), "0 秒前"); // 時計ずれでも負にならない
    }

    #[test]
    fn trigger_labels_fall_back_to_raw() {
        assert_eq!(trigger_label("manual"), "手動");
        assert_eq!(trigger_label("zzz"), "zzz");
    }
}
