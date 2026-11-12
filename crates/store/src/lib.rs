//! history.db（SQLite）: 解放履歴（設計書7章）。
use memopt_core::{LoadSample, MemSample};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: i64,
    pub ts_unix: i64,
    pub level: u8,
    pub trigger: String,
    pub before: MemSample,
    pub after: MemSample,
    pub load_before: Option<LoadSample>,
    pub after60: Option<MemSample>,
    pub load_after60: Option<LoadSample>,
    /// CleanReport の JSON（対象数・失敗内訳）
    pub report_json: String,
    /// 自動調整で控えめにした理由（「なぜ控えめにしたか」）。
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug)]
pub struct StoreError(pub String);
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError { fn from(e: rusqlite::Error) -> Self { Self(e.to_string()) } }
impl From<serde_json::Error> for StoreError { fn from(e: serde_json::Error) -> Self { Self(e.to_string()) } }

pub struct Store { conn: Connection }

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> { Self::init(Connection::open(path)?) }
    pub fn open_in_memory() -> Result<Self, StoreError> { Self::init(Connection::open_in_memory()?) }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clean_runs (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               ts_unix INTEGER NOT NULL,
               level INTEGER NOT NULL,
               trigger TEXT NOT NULL,
               before_json TEXT NOT NULL,
               after_json TEXT NOT NULL,
               load_before_json TEXT,
               after60_json TEXT,
               load_after60_json TEXT,
               report_json TEXT NOT NULL,
               note TEXT
             );",
        )?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// 古いスキーマ（note 列なし）の DB を引き継ぐ。
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        let has_note = conn
            .prepare("SELECT 1 FROM pragma_table_info('clean_runs') WHERE name = 'note'")?
            .exists([])?;
        if !has_note {
            conn.execute("ALTER TABLE clean_runs ADD COLUMN note TEXT", [])?;
        }
        Ok(())
    }

    /// 自動調整の理由を履歴に残す。
    pub fn set_note(&self, id: i64, note: &str) -> Result<(), StoreError> {
        let n = self.conn.execute("UPDATE clean_runs SET note = ?2 WHERE id = ?1", params![id, note])?;
        if n == 0 {
            return Err(StoreError(format!("run {id} not found")));
        }
        Ok(())
    }

    /// 新規実行を記録し、id を返す。
    pub fn insert_run(&self, r: &RunRecord) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO clean_runs (ts_unix, level, trigger, before_json, after_json, load_before_json, report_json, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.ts_unix, r.level, r.trigger,
                serde_json::to_string(&r.before)?, serde_json::to_string(&r.after)?,
                r.load_before.map(|l| serde_json::to_string(&l)).transpose()?,
                r.report_json,
                r.note
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 実行 60 秒後の測定を追記する。存在しない id は Err。
    pub fn update_after60(&self, id: i64, mem: &MemSample, load: &LoadSample) -> Result<(), StoreError> {
        let n = self.conn.execute(
            "UPDATE clean_runs SET after60_json = ?2, load_after60_json = ?3 WHERE id = ?1",
            params![id, serde_json::to_string(mem)?, serde_json::to_string(load)?],
        )?;
        if n == 0 { return Err(StoreError(format!("run {id} not found"))); }
        Ok(())
    }

    /// 新しい順に最大 `limit` 件。
    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunRecord>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT id, ts_unix, level, trigger, before_json, after_json, load_before_json,
                    after60_json, load_after60_json, report_json, note
             FROM clean_runs ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, u8>(2)?, row.get::<_, String>(3)?,
                row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?, row.get::<_, Option<String>>(8)?, row.get::<_, String>(9)?, row.get::<_, Option<String>>(10)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, ts, level, trig, b, a, lb, a60, la60, rep, note) = r?;
            out.push(RunRecord {
                id, ts_unix: ts, level, trigger: trig,
                before: serde_json::from_str(&b)?, after: serde_json::from_str(&a)?,
                load_before: lb.map(|s| serde_json::from_str(&s)).transpose()?,
                after60: a60.map(|s| serde_json::from_str(&s)).transpose()?,
                load_after60: la60.map(|s| serde_json::from_str(&s)).transpose()?,
                report_json: rep,
                note,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: u8) -> RunRecord {
        RunRecord {
            id: 0, ts_unix: 1_700_000_000, level, trigger: "manual".into(),
            before: MemSample { avail_phys: 100, standby: Some(50), modified: Some(5), commit_pct: 40.0 },
            after: MemSample { avail_phys: 180, standby: Some(10), modified: Some(5), commit_pct: 40.0 },
            load_before: Some(LoadSample { pages_input_per_sec: 10.0, disk_read_bytes_per_sec: 1000.0 }),
            after60: None, load_after60: None, report_json: "{}".into(), note: None,
        }
    }

    #[test]
    fn insert_then_list_roundtrips_and_orders_newest_first() {
        let s = Store::open_in_memory().unwrap();
        let a = s.insert_run(&rec(1)).unwrap();
        let b = s.insert_run(&rec(2)).unwrap();
        let v = s.list_runs(10).unwrap();
        assert_eq!(v.iter().map(|r| r.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(v[1].before.avail_phys, 100);
        assert_eq!(v[1].after60, None);
    }

    #[test]
    fn update_after60_is_persisted() {
        let s = Store::open_in_memory().unwrap();
        let id = s.insert_run(&rec(1)).unwrap();
        let mem = MemSample { avail_phys: 150, ..Default::default() };
        let load = LoadSample { pages_input_per_sec: 99.0, disk_read_bytes_per_sec: 5.0 };
        s.update_after60(id, &mem, &load).unwrap();
        let r = &s.list_runs(1).unwrap()[0];
        assert_eq!(r.after60.unwrap().avail_phys, 150);
        assert_eq!(r.load_after60.unwrap().pages_input_per_sec, 99.0);
    }

    #[test]
    fn update_missing_id_errors_and_limit_applies() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.update_after60(42, &MemSample::default(), &LoadSample::default()).is_err());
        for _ in 0..3 { s.insert_run(&rec(1)).unwrap(); }
        assert_eq!(s.list_runs(2).unwrap().len(), 2);
    }

    #[test]
    fn file_db_persists_across_open() {
        let dir = std::env::temp_dir().join(format!("memopt_store_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("history.db");
        { Store::open(&p).unwrap().insert_run(&rec(3)).unwrap(); }
        assert_eq!(Store::open(&p).unwrap().list_runs(5).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn note_is_stored_and_listed() {
        let s = Store::open_in_memory().unwrap();
        let id = s.insert_run(&rec(1)).unwrap();
        assert_eq!(s.list_runs(1).unwrap()[0].note, None);
        s.set_note(id, "ハードフォルトが増えたため延長").unwrap();
        assert_eq!(s.list_runs(1).unwrap()[0].note.as_deref(), Some("ハードフォルトが増えたため延長"));
        assert!(s.set_note(999, "x").is_err());
    }

    #[test]
    fn old_database_without_note_column_is_migrated_in_place() {
        let dir = std::env::temp_dir().join(format!("memopt_store_mig_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("history.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "CREATE TABLE clean_runs (id INTEGER PRIMARY KEY AUTOINCREMENT, ts_unix INTEGER NOT NULL, level INTEGER NOT NULL,
                 trigger TEXT NOT NULL, before_json TEXT NOT NULL, after_json TEXT NOT NULL, load_before_json TEXT,
                 after60_json TEXT, load_after60_json TEXT, report_json TEXT NOT NULL);
                 INSERT INTO clean_runs (ts_unix, level, trigger, before_json, after_json, report_json)
                 VALUES (1, 1, 'manual', '{\"avail_phys\":1,\"standby\":null,\"modified\":null,\"commit_pct\":1.0}',
                         '{\"avail_phys\":2,\"standby\":null,\"modified\":null,\"commit_pct\":1.0}', '[]');",
            )
            .unwrap();
        }
        let s = Store::open(&p).unwrap();
        let runs = s.list_runs(5).unwrap();
        assert_eq!((runs.len(), runs[0].note.clone()), (1, None)); // 既存の記録は残る
        s.set_note(runs[0].id, "n").unwrap();
        assert_eq!(Store::open(&p).unwrap().list_runs(1).unwrap()[0].note.as_deref(), Some("n")); // 再オープンでも壊れない
        let _ = std::fs::remove_dir_all(&dir);
    }
}
