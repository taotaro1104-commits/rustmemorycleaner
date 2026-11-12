//! journal.jsonl: 永続変更の操作ジャーナル（追記のみ・削除しない。設計書5章）。
//! 1 行 1 イベント。状態は先頭から畳み込んで得る（途中で壊れた行は読み飛ばす）。
use memopt_core::stop::{JournalEntry, Outcome};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(tag = "ev")]
enum Event {
    #[serde(rename = "entry")]
    Entry { entry: JournalEntry },
    #[serde(rename = "undone")]
    Undone { id: u64, ts: i64 },
    #[serde(rename = "outcome")]
    Outcome { id: u64, outcome: Outcome, avail_after: Option<u64> },
}

pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn open(path: &Path) -> Self {
        Self { path: path.to_path_buf() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, ev: &Event) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut line = serde_json::to_string(ev).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.sync_all() // 復元に必要な記録なので、電源断でも残るようにする
    }

    pub fn append_entry(&self, e: &JournalEntry) -> std::io::Result<()> {
        self.append(&Event::Entry { entry: e.clone() })
    }

    pub fn mark_undone(&self, id: u64, ts: i64) -> std::io::Result<()> {
        self.append(&Event::Undone { id, ts })
    }

    pub fn set_outcome(&self, id: u64, outcome: Outcome, avail_after: Option<u64>) -> std::io::Result<()> {
        self.append(&Event::Outcome { id, outcome, avail_after })
    }

    /// 畳み込んだ現在の状態（id 昇順）。ファイルが無ければ空。
    pub fn load(&self) -> Vec<JournalEntry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return vec![] };
        let mut entries: Vec<JournalEntry> = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Event>(line) {
                Ok(Event::Entry { entry }) => entries.push(entry),
                Ok(Event::Undone { id, ts }) => {
                    if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                        e.undone_ts = Some(ts);
                    }
                }
                Ok(Event::Outcome { id, outcome, avail_after }) => {
                    if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                        e.outcome = outcome;
                        e.avail_after = avail_after.or(e.avail_after);
                    }
                }
                Err(_) => continue, // 壊れた行（書き込み途中の電源断など）は無視して続行
            }
        }
        entries
    }

    pub fn next_id(&self) -> u64 {
        self.load().iter().map(|e| e.id).max().unwrap_or(0) + 1
    }

    /// 未復元の永続変更（アンインストール時の一覧表示・一括復元の対象）。
    pub fn pending_persistent(&self) -> Vec<JournalEntry> {
        self.load().into_iter().filter(JournalEntry::pending_persistent).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::stop::{Change, StopMethod};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("memopt_jr_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("journal.jsonl")
    }

    fn entry(id: u64, method: StopMethod, change: Change) -> JournalEntry {
        JournalEntry {
            id, ts_unix: 100 + id as i64, feature_id: format!("f{id}"), feature_name: "F".into(), method, target: None,
            change, outcome: Outcome::Ok, avail_before: Some(10), avail_after: None, undone_ts: None, actor_sid: None,
        }
    }

    fn task(id: u64) -> JournalEntry {
        entry(id, StopMethod::DisableTask, Change::TaskEnabled { path: r"\T".into(), before_enabled: true })
    }

    #[test]
    fn missing_file_is_empty_and_ids_start_at_one() {
        let j = Journal::open(&tmp("none"));
        assert!(j.load().is_empty());
        assert_eq!(j.next_id(), 1);
    }

    #[test]
    fn append_and_load_preserves_order_and_next_id_increments() {
        let j = Journal::open(&tmp("order"));
        j.append_entry(&task(1)).unwrap();
        j.append_entry(&task(2)).unwrap();
        assert_eq!(j.load().iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(j.next_id(), 3);
    }

    #[test]
    fn undone_and_outcome_events_fold_into_entries_without_rewriting_the_file() {
        let j = Journal::open(&tmp("fold"));
        let mut e = task(1);
        e.outcome = Outcome::Requested;
        j.append_entry(&e).unwrap();
        j.set_outcome(1, Outcome::Ok, Some(99)).unwrap();
        j.mark_undone(1, 500).unwrap();
        let before_size = std::fs::metadata(j.path()).unwrap().len();
        let got = &j.load()[0];
        assert_eq!((got.outcome.clone(), got.avail_after, got.undone_ts), (Outcome::Ok, Some(99), Some(500)));
        assert_eq!(std::fs::metadata(j.path()).unwrap().len(), before_size); // 読み取りでファイルは変わらない
    }

    #[test]
    fn corrupt_lines_are_skipped_but_valid_ones_survive() {
        let j = Journal::open(&tmp("corrupt"));
        j.append_entry(&task(1)).unwrap();
        std::fs::OpenOptions::new().append(true).open(j.path()).unwrap().write_all(b"{\"ev\":\"entry\",\"entr\n garbage\n").unwrap();
        j.append_entry(&task(2)).unwrap();
        assert_eq!(j.load().len(), 2);
    }

    #[test]
    fn events_for_unknown_ids_are_ignored() {
        let j = Journal::open(&tmp("unknown"));
        j.append_entry(&task(1)).unwrap();
        j.mark_undone(42, 1).unwrap();
        assert_eq!(j.load()[0].undone_ts, None);
    }

    #[test]
    fn pending_persistent_lists_only_unrestored_persistent_changes() {
        let j = Journal::open(&tmp("pending"));
        j.append_entry(&task(1)).unwrap();
        j.append_entry(&task(2)).unwrap();
        j.append_entry(&entry(3, StopMethod::ServiceStop, Change::ServiceStopped { name: "s".into(), dependents: vec![] })).unwrap();
        j.mark_undone(1, 9).unwrap();
        assert_eq!(j.pending_persistent().iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);
    }
}
