//! memopt-cli: 手動解放（P2）。実行前/直後/60 秒後の値を history.db に記録する。
mod args;
mod diag;
mod install;
mod undo;

use args::{CleanOpts, Command};
use memopt_core::{Level, LoadSample, MemSample};
use memopt_probe::Pdh;
use memopt_store::{RunRecord, Store};
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "memopt-cli
  clean [--level 1|2|3] [--dry-run] [--post-secs N] [--pre-secs N] [--idle-window N]
        [--max-processes N] [--protect <exe フルパス>]... [--db <path>]
  history [--limit N] [--db <path>]
  install     サービス登録＋トレイ自動起動（管理者）
  uninstall [--restore|--keep]   サービス削除＋自動起動解除（管理者）。未復元の変更があれば確認
  undo --all | --id <N> [--journal <path>]   ジャーナルから復元（サービス停止中・セーフモードでも可）
  journal [--journal <path>]                 変更履歴の一覧
  uninstall-prompt [--ui-level N]            MSI のアンインストール前確認（N<3 はサイレント: 何も変更しない）
  diag [--out <zip>] [--no-redact]            不具合報告用の診断 ZIP（既定で個人情報を伏せる）
L2/L3 は管理者権限が必要です（権限が無い場合は「利用不可」と表示して終了します）。";

fn mb(b: u64) -> f64 { b as f64 / (1u64 << 20) as f64 }

fn default_db() -> PathBuf {
    let base = if memopt_cleaner::is_elevated() {
        std::env::var_os("ProgramData")
    } else {
        std::env::var_os("LOCALAPPDATA")
    };
    PathBuf::from(base.unwrap_or_else(|| ".".into())).join("MemOpt").join("history.db")
}

fn open_store(db: Option<PathBuf>) -> Result<Store, String> {
    let p = db.unwrap_or_else(default_db);
    if let Some(dir) = p.parent() { std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?; }
    Store::open(&p).map_err(|e| format!("{}: {e}", p.display()))
}

fn sample(pdh: Option<&Pdh>) -> MemSample {
    let m = memopt_probe::memory_snapshot().unwrap_or_default();
    MemSample {
        avail_phys: m.avail_phys,
        standby: pdh.and_then(Pdh::standby_bytes),
        modified: pdh.and_then(Pdh::modified_bytes),
        commit_pct: m.commit_pct(),
    }
}

fn filetime_now() -> u64 {
    // 1601-01-01 からの 100ns。UNIX 時刻から換算。
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + (d.subsec_nanos() / 100) as u64
}

fn collect_load(pdh: Option<&Pdh>, secs: u64) -> Option<LoadSample> {
    let p = pdh?;
    p.collect();
    std::thread::sleep(Duration::from_secs(secs.max(1)));
    p.collect();
    p.load()
}

fn l1_targets(o: &CleanOpts, procs: &[memopt_core::ProcessInfo]) -> Vec<u32> {
    let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    let before = memopt_probe::cpu_times(&pids);
    std::thread::sleep(Duration::from_secs(o.idle_window_secs));
    let after = memopt_probe::cpu_times(&pids);
    // 窓の 0.5% 以上 CPU を使ったプロセスは「使用中」とみなして除外。
    let threshold = o.idle_window_secs * 10_000_000 / 200;
    let ctx = memopt_policy::l1::L1Context {
        now_filetime: filetime_now(),
        self_pid: std::process::id(),
        foreground_pid: memopt_probe::foreground_pid(),
        audio_pids: memopt_probe::audio_pids(),
        busy_pids: memopt_probe::busy_pids(&before, &after, threshold),
        user_protected: &o.protect,
        max_processes: o.max_processes,
        skip_foreground: true,
        skip_audio: true,
    };
    memopt_policy::l1::select_l1_targets(procs, &ctx)
}

fn clean(o: CleanOpts) -> Result<(), String> {
    println!("管理者権限: {}", if memopt_cleaner::is_elevated() { "あり" } else { "なし" });
    if let memopt_cleaner::Availability::Unavailable(why) = memopt_cleaner::availability(o.level) {
        return Err(format!("L{} は利用不可: {why}", o.level as u8));
    }
    let store = if o.dry_run { None } else { Some(open_store(o.db.clone())?) };
    let pdh = Pdh::new();
    if pdh.is_none() { eprintln!("警告: PDH を開けない。スタンバイ/ハードフォルトは記録されません"); }

    let procs = memopt_probe::processes();
    let targets = if o.level == Level::L1 { l1_targets(&o, &procs) } else { vec![] };
    if o.level == Level::L1 {
        println!("L1 対象: {} プロセス（上限 {}）", targets.len(), o.max_processes);
        if o.dry_run {
            for pid in &targets {
                if let Some(p) = procs.iter().find(|p| p.pid == *pid) {
                    println!("  {:>6} {:>8.0} MB  {}", pid, mb(p.working_set.unwrap_or(0)), p.exe_name);
                }
            }
        }
    }
    if o.dry_run { println!("--dry-run: 実行しません"); return Ok(()); }

    let load_before = collect_load(pdh.as_ref(), o.pre_secs);
    let before = sample(pdh.as_ref());
    let report = if o.level == Level::L1 { memopt_cleaner::run_l1(&targets) } else { memopt_cleaner::run_level(o.level) };
    std::thread::sleep(Duration::from_millis(500));
    if let Some(p) = &pdh { p.collect(); }
    let after = sample(pdh.as_ref());

    let freed = after.avail_phys as i64 - before.avail_phys as i64;
    println!("実行 {} ms: 成功 {}/{} 失敗 {}", report.elapsed_ms, report.succeeded, report.attempted, report.failures.len());
    for f in report.failures.iter().take(5) { println!("  失敗 {}: {}", f.target, f.reason); }
    println!("空きメモリ: {:.0} MB -> {:.0} MB（{:+.0} MB）", mb(before.avail_phys), mb(after.avail_phys), freed as f64 / (1u64 << 20) as f64);

    // 利用可能メモリはスタンバイを含むため、L2/L3 の効果はスタンバイ/変更ページの増減で見る。
    if let (Some(b), Some(a)) = (before.standby, after.standby) {
        println!("スタンバイ: {:.0} MB -> {:.0} MB（{:+.0} MB）", mb(b), mb(a), (a as i64 - b as i64) as f64 / (1u64 << 20) as f64);
    }
    if let (Some(b), Some(a)) = (before.modified, after.modified) {
        println!("変更ページ: {:.0} MB -> {:.0} MB", mb(b), mb(a));
    }
    let store = store.expect("dry-run は上で return 済み");
    let id = store.insert_run(&RunRecord {
        id: 0, ts_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        level: o.level as u8, trigger: "manual".into(), before, after, load_before,
        after60: None, load_after60: None, note: None,
        report_json: serde_json::to_string(&report).map_err(|e| e.to_string())?,
    }).map_err(|e| e.to_string())?;

    if o.post_secs > 0 {
        println!("{} 秒後の測定を待機中...", o.post_secs);
        let load = collect_load(pdh.as_ref(), o.post_secs).unwrap_or_default();
        let after60 = sample(pdh.as_ref());
        store.update_after60(id, &after60, &load).map_err(|e| e.to_string())?;
        println!("{} 秒後: 空き {:.0} MB / ハードフォルト {:.0}/s（実行前 {}）",
            o.post_secs, mb(after60.avail_phys), load.pages_input_per_sec,
            load_before.map(|l| format!("{:.0}/s", l.pages_input_per_sec)).unwrap_or("不明".into()));
    }
    println!("履歴 id={id} を保存しました");
    Ok(())
}

fn history(limit: usize, db: Option<PathBuf>) -> Result<(), String> {
    for r in open_store(db)?.list_runs(limit).map_err(|e| e.to_string())? {
        let a60 = r.after60.map(|m| format!("{:.0}MB", mb(m.avail_phys))).unwrap_or("-".into());
        println!("#{} ts={} L{} {} 空き {:.0}->{:.0}MB 60秒後 {}", r.id, r.ts_unix, r.level, r.trigger,
            mb(r.before.avail_phys), mb(r.after.avail_phys), a60);
    }
    Ok(())
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let res = match args::parse(&argv) {
        Ok(Command::Help) => { println!("{HELP}"); Ok(()) }
        Ok(Command::Clean(o)) => clean(o),
        Ok(Command::History { limit, db }) => history(limit, db),
        Ok(Command::Install) => install::install(),
        Ok(Command::Uninstall { restore }) => install::uninstall(restore),
        Ok(Command::Undo { all, id, journal }) => undo::undo(all, id, journal),
        Ok(Command::Journal { journal }) => undo::list(journal),
        Ok(Command::Diag { out, data_dir, redact }) => diag::run(out, data_dir, redact),
        Ok(Command::UninstallPrompt { ui_level }) => undo::uninstall_prompt(ui_level),
        Err(e) => Err(e),
    };
    if let Err(e) = res {
        eprintln!("エラー: {e}");
        std::process::exit(1);
    }
}
