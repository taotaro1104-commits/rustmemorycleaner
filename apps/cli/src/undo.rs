//! ジャーナルからの復元（サービスが動いていなくても、セーフモードでも使える。設計書6章）。
use memopt_actions::exec;
use memopt_actions::journal::Journal;
use memopt_core::stop::{JournalEntry, Outcome};
use std::path::PathBuf;

pub fn default_journal() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("MemOpt").join("journal.jsonl")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// 1 行の説明（一覧表示用）。
pub fn format_entry(e: &JournalEntry) -> String {
    let state = if e.undone_ts.is_some() {
        "復元済み".to_string()
    } else {
        match &e.outcome {
            Outcome::Ok if e.undoable() => "復元可".into(),
            Outcome::Ok => "完了（元に戻せない操作）".into(),
            Outcome::Failed(m) => format!("失敗: {m}"),
            Outcome::Requested => "依頼のみ".into(),
            Outcome::TimedOut => "タイムアウト".into(),
        }
    };
    let target = e.target.as_deref().map(|t| format!(" [{t}]")).unwrap_or_default();
    format!("#{:<4} {} / {}{target}  — {state}", e.id, e.feature_name, e.method.label())
}

pub fn list(path: Option<PathBuf>) -> Result<(), String> {
    let j = Journal::open(&path.unwrap_or_else(default_journal));
    let entries = j.load();
    if entries.is_empty() {
        println!("記録はありません（{}）", j.path().display());
        return Ok(());
    }
    for e in entries.iter().rev() {
        println!("{}", format_entry(e));
    }
    Ok(())
}

/// 確認ダイアログに出す本文（pure）。先頭 `max` 件だけ並べ、残りは件数で示す。
pub fn prompt_text(entries: &[JournalEntry], max: usize) -> String {
    let mut s = format!("MemOpt が加えた未復元の変更が {} 件あります:\n\n", entries.len());
    for e in entries.iter().take(max) {
        s += &format!("・{}\n", format_entry(e));
    }
    if entries.len() > max {
        s += &format!("…ほか {} 件\n", entries.len() - max);
    }
    s + "\nアンインストール前に、これらを元に戻しますか？\n（「いいえ」なら変更はそのまま残り、後から memopt-cli undo --all で戻せます）"
}

/// MSI のアンインストール前の確認。サイレント（ui_level < 3）では何も変更しない。
pub fn uninstall_prompt(ui_level: u32) -> Result<(), String> {
    let j = Journal::open(&default_journal());
    let pending = j.pending_persistent();
    if pending.is_empty() {
        return Ok(());
    }
    if ui_level < 3 {
        println!("未復元の永続変更が {} 件あります（サイレントのため変更しません）。`memopt-cli undo --all` で戻せます", pending.len());
        return Ok(());
    }
    if ask_yes_no("MemOpt のアンインストール", &prompt_text(&pending, 12)) {
        undo(true, None, None)?;
    }
    Ok(())
}

fn ask_yes_no(title: &str, body: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::*;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let (t, b) = (wide(title), wide(body));
    unsafe { MessageBoxW(None, PCWSTR(b.as_ptr()), PCWSTR(t.as_ptr()), MB_YESNO | MB_ICONQUESTION | MB_TOPMOST) == IDYES }
}

/// 未復元の永続変更を一覧表示する（アンインストール時の確認用）。
pub fn print_pending(j: &Journal) -> usize {
    let pending = j.pending_persistent();
    for e in &pending {
        println!("  {}", format_entry(e));
    }
    pending.len()
}

pub fn undo(all: bool, id: Option<u64>, path: Option<PathBuf>) -> Result<(), String> {
    let j = Journal::open(&path.unwrap_or_else(default_journal));
    if all {
        let r = exec::undo_all(&j, now());
        println!("復元 {} 件 / 失敗 {} 件", r.restored.len(), r.failed.len());
        for (i, m) in &r.failed {
            println!("  #{i}: {m}");
        }
        return if r.failed.is_empty() { Ok(()) } else { Err("一部を復元できませんでした".into()) };
    }
    let id = id.ok_or("--id が必要です")?;
    exec::undo(&j, id, now())?;
    println!("#{id} を元に戻しました");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::stop::{Change, StopMethod};

    fn entry(outcome: Outcome, change: Change, undone: Option<i64>) -> JournalEntry {
        JournalEntry {
            id: 3, ts_unix: 0, feature_id: "f".into(), feature_name: "Search".into(), method: StopMethod::ServiceStartupDisabled,
            target: Some("WSearch".into()), change, outcome, avail_before: None, avail_after: None, undone_ts: undone,
        }
    }

    fn restorable() -> Change {
        Change::ServiceStartType { name: "WSearch".into(), before: 2, delayed_before: false }
    }

    #[test]
    fn format_shows_state_and_target() {
        let s = format_entry(&entry(Outcome::Ok, restorable(), None));
        assert!(s.contains("#3") && s.contains("Search") && s.contains("[WSearch]") && s.contains("復元可"), "{s}");
        assert!(format_entry(&entry(Outcome::Ok, restorable(), Some(5))).contains("復元済み"));
        assert!(format_entry(&entry(Outcome::Failed("x".into()), Change::None, None)).contains("失敗: x"));
        assert!(format_entry(&entry(Outcome::Ok, Change::None, None)).contains("元に戻せない"));
    }

    #[test]
    fn missing_journal_lists_nothing_and_undo_id_is_an_error() {
        let p = std::env::temp_dir().join(format!("memopt_cli_undo_{}", std::process::id())).join("journal.jsonl");
        assert!(list(Some(p.clone())).is_ok());
        assert!(undo(false, Some(1), Some(p.clone())).is_err());
        assert!(undo(true, None, Some(p)).is_ok()); // 何も無ければ成功（0 件）
    }

    #[test]
    fn prompt_text_lists_entries_and_summarizes_the_rest() {
        let mk = |id: u64| JournalEntry { id, ..entry(Outcome::Ok, restorable(), None) };
        let es: Vec<_> = (1..=5).map(mk).collect();
        let t = prompt_text(&es, 3);
        assert!(t.contains("5 件") && t.contains("#1") && t.contains("#3") && !t.contains("#4"), "{t}");
        assert!(t.contains("ほか 2 件") && t.contains("元に戻しますか"));
        assert!(!prompt_text(&es[..2], 3).contains("ほか"));
    }

    #[test]
    fn silent_uninstall_prompt_never_changes_anything() {
        // ui_level < 3 はダイアログも復元もしない（journal が無ければ何もせず成功）。
        assert!(uninstall_prompt(0).is_ok());
    }
}
