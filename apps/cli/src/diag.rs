//! 診断 ZIP（設計書7章）: 不具合報告用に直近の状態をまとめる。個人のファイルパスなどは既定で伏せる。
use memopt_actions::journal::Journal;
use memopt_store::Store;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_LOG_BYTES: usize = 512 * 1024;
const MAX_LOG_FILES: usize = 3;
const HISTORY_ROWS: usize = 50;

/// 個人を特定し得る文字列（ユーザー名・PC 名・SID・ユーザープロファイルのパス）を伏せる。
pub fn redact(text: &str, user: &str, host: &str) -> String {
    let mut out = text.to_string();
    // ユーザープロファイルのパス（区切りが / や \\ の表記も）
    for sep in ["\\", "/", "\\\\"] {
        let needle = format!("Users{sep}{user}");
        if !user.is_empty() {
            out = replace_ci(&out, &needle, &format!("Users{sep}<user>"));
        }
    }
    if user.len() >= 3 {
        out = replace_ci(&out, user, "<user>");
    }
    if host.len() >= 3 {
        out = replace_ci(&out, host, "<host>");
    }
    redact_sids(&out)
}

/// 大文字小文字を無視した置換（ASCII のみ照合、非 ASCII はそのまま）。
fn replace_ci(s: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return s.to_string();
    }
    let (ls, lf) = (s.to_ascii_lowercase(), from.to_ascii_lowercase());
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(pos) = ls[i..].find(&lf) {
        out.push_str(&s[i..i + pos]);
        out.push_str(to);
        i += pos + from.len();
    }
    out.push_str(&s[i..]);
    out
}

/// `S-1-5-21-…` 形式のユーザー SID（機器/ドメイン固有部分）を伏せる。
fn redact_sids(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("S-1-5-21-") {
        out.push_str(&rest[..p]);
        let tail = &rest[p + "S-1-5-21-".len()..];
        let n = tail.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(tail.len());
        out.push_str("S-1-5-21-<redacted>");
        rest = &tail[n..];
    }
    out.push_str(rest);
    out
}

pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
}

fn entry(name: &str, text: String) -> Entry {
    Entry { name: name.to_string(), data: text.into_bytes() }
}

/// 末尾 `max` バイトだけ残す（ログが巨大でも ZIP を小さく保つ）。UTF-8 の境界を壊さない。
pub fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

pub fn write_zip(path: &Path, entries: &[Entry]) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut z = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for e in entries {
        z.start_file(&e.name, opts).map_err(|er| er.to_string())?;
        z.write_all(&e.data).map_err(|er| er.to_string())?;
    }
    z.finish().map(|_| ()).map_err(|e| e.to_string())
}

fn read_or(path: &Path, what: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("({what} を読めません: {e})\n"))
}

fn system_info() -> String {
    let m = memopt_probe::memory_snapshot();
    let os = std::process::Command::new("cmd").args(["/c", "ver"]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let mut s = format!("memopt {}\nOS: {os}\n管理者権限: {}\n", env!("CARGO_PKG_VERSION"), memopt_cleaner::is_elevated());
    if let Some(m) = m {
        s += &format!(
            "物理メモリ: 合計 {} MB / 空き {} MB ({:.0}%)\nコミット: {:.0}%\n",
            m.total_phys >> 20,
            m.avail_phys >> 20,
            m.avail_pct(),
            m.commit_pct()
        );
    }
    s
}

fn top_features() -> String {
    let procs = memopt_probe::processes();
    let mut out = String::from("種類\t名前\tプライベート MB\tプロセス数\n");
    for f in memopt_grouping::group(&procs).iter().take(30) {
        out += &format!("{:?}\t{}\t{}\t{}\n", f.kind, f.name, f.private_bytes >> 20, f.pids.len());
    }
    out
}

/// 診断 ZIP の中身を集める。データフォルダーに無いファイルは「読めません」と書いて続行する。
pub fn collect(data_dir: &Path, redact_on: bool, user: &str, host: &str) -> Vec<Entry> {
    let mut files = vec![
        entry("system.txt", system_info()),
        entry("config.toml", read_or(&data_dir.join("config.toml"), "config.toml")),
        entry("journal.jsonl", {
            let p = data_dir.join("journal.jsonl");
            if Journal::open(&p).load().is_empty() { "(記録なし)\n".into() } else { read_or(&p, "journal.jsonl") }
        }),
        entry("features_top30.tsv", top_features()),
    ];
    let history = Store::open(&data_dir.join("history.db"))
        .and_then(|s| s.list_runs(HISTORY_ROWS))
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        .unwrap_or_else(|e| format!("(履歴を読めません: {e})"));
    files.push(entry("history.json", history));

    // 直近のログ（新しい順に数件、末尾だけ）
    let mut logs: Vec<PathBuf> = std::fs::read_dir(data_dir.join("logs"))
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    logs.sort();
    for p in logs.iter().rev().take(MAX_LOG_FILES) {
        let text = read_or(p, "ログ");
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "log".into());
        files.push(entry(&format!("logs/{name}"), tail_bytes(&text, MAX_LOG_BYTES).to_string()));
    }

    if redact_on {
        for f in files.iter_mut() {
            f.data = redact(&String::from_utf8_lossy(&f.data), user, host).into_bytes();
        }
    }
    files
}

pub fn run(out: Option<PathBuf>, data_dir: Option<PathBuf>, redact_on: bool) -> Result<(), String> {
    let dir = data_dir.unwrap_or_else(|| PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("MemOpt"));
    let out = out.unwrap_or_else(|| {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        std::env::temp_dir().join(format!("memopt-diag-{ts}.zip"))
    });
    let user = std::env::var("USERNAME").unwrap_or_default();
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    let entries = collect(&dir, redact_on, &user, &host);
    write_zip(&out, &entries)?;
    println!("診断 ZIP を作成しました: {}（{} ファイル、個人情報の伏せ字: {}）", out.display(), entries.len(), if redact_on { "あり" } else { "なし" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn redacts_profile_paths_user_host_and_sids() {
        let t = r"C:\Users\Taro\AppData\x.exe by taro on DESKTOP-ABC (S-1-5-21-111-222-333-1001) C:/users/TARO/y";
        let r = redact(t, "Taro", "DESKTOP-ABC");
        assert!(!r.to_ascii_lowercase().contains("taro") && !r.contains("DESKTOP-ABC"), "{r}");
        assert!(r.contains(r"C:\Users\<user>\AppData") && r.contains("S-1-5-21-<redacted>"), "{r}");
        assert!(r.contains("C:/Users/<user>/y"), "{r}");
        // 非個人情報は変えない
        assert_eq!(redact(r"C:\Windows\System32\svchost.exe", "Taro", "PC-1"), r"C:\Windows\System32\svchost.exe");
    }

    #[test]
    fn short_or_empty_names_do_not_mangle_unrelated_text() {
        // 2 文字以下のユーザー名/ホスト名で全文を壊さない（"a" を全置換しない）。
        assert_eq!(redact("abc ab a", "a", "ab"), "abc ab a");
        assert_eq!(redact("hello", "", ""), "hello");
    }

    #[test]
    fn tail_bytes_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail_bytes("abcdef", 3), "def");
        assert_eq!(tail_bytes("abc", 10), "abc");
        let s = "あいうえお"; // 各 3 バイト
        let t = tail_bytes(s, 7);
        assert!(s.ends_with(t) && t.len() <= 7);
    }

    #[test]
    fn zip_roundtrip_with_redaction_and_missing_inputs() {
        let dir = std::env::temp_dir().join(format!("memopt_diag_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("config.toml"), "path = 'C:\\Users\\Taro\\x.exe'").unwrap();
        std::fs::write(dir.join("logs").join("memopt.log.2026-11-12"), "user Taro did X").unwrap();
        let entries = collect(&dir, true, "Taro", "PC-XYZ");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for want in ["system.txt", "config.toml", "journal.jsonl", "history.json", "features_top30.tsv", "logs/memopt.log.2026-11-12"] {
            assert!(names.contains(&want), "{want} が無い: {names:?}");
        }
        let out = dir.join("out.zip");
        write_zip(&out, &entries).unwrap();
        let mut z = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut cfg = String::new();
        z.by_name("config.toml").unwrap().read_to_string(&mut cfg).unwrap();
        assert!(cfg.contains("<user>") && !cfg.contains("Taro"), "{cfg}");
        let mut log = String::new();
        z.by_name("logs/memopt.log.2026-11-12").unwrap().read_to_string(&mut log).unwrap();
        assert_eq!(log, "user <user> did X");
        // 履歴 DB が無くても失敗せず、その旨が書かれる
        let mut h = String::new();
        z.by_name("history.json").unwrap().read_to_string(&mut h).unwrap();
        assert!(!h.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redaction_can_be_disabled() {
        let dir = std::env::temp_dir().join(format!("memopt_diag_off_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "Taro").unwrap();
        let e = collect(&dir, false, "Taro", "PC");
        assert_eq!(e.iter().find(|x| x.name == "config.toml").unwrap().data, b"Taro");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
