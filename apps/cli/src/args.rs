//! 引数解析（依存を増やさないため手書き）。
use memopt_core::Level;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Command {
    Clean(CleanOpts),
    History { limit: usize, db: Option<PathBuf> },
    Install,
    /// `restore`: None = 未復元の永続変更があれば対話で確認、Some = 指定どおり。
    Uninstall { restore: Option<bool> },
    Undo { all: bool, id: Option<u64>, journal: Option<PathBuf> },
    Journal { journal: Option<PathBuf> },
    /// 診断 ZIP。個人情報は既定で伏せる（--no-redact で無効）。
    Diag { out: Option<PathBuf>, data_dir: Option<PathBuf>, redact: bool },
    /// MSI のアンインストール前に呼ばれる。未復元の永続変更があれば確認ダイアログで復元を提案する。
    /// `ui_level` が 3 未満（サイレント）ならダイアログを出さず、一覧だけ表示して何も変更しない。
    UninstallPrompt { ui_level: u32 },
    Help,
}

#[derive(Debug, PartialEq)]
pub struct CleanOpts {
    pub level: Level,
    pub db: Option<PathBuf>,
    /// 実行後に追加測定する秒数（設計書は 60）。
    pub post_secs: u64,
    /// 実行前の負荷測定秒数。常時記録のリングバッファが無い CLI では短い窓で代用。
    pub pre_secs: u64,
    /// L1: CPU 使用中判定の観測窓（秒）。
    pub idle_window_secs: u64,
    pub max_processes: usize,
    pub protect: Vec<String>,
    pub dry_run: bool,
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => Ok(Command::Help),
        Some("install") => Ok(Command::Install),
        Some("uninstall") => {
            let mut restore = None;
            for a in it.by_ref() {
                match a.as_str() {
                    "--restore" => restore = Some(true),
                    "--keep" => restore = Some(false),
                    o => return Err(format!("不明なオプション: {o}")),
                }
            }
            Ok(Command::Uninstall { restore })
        }
        Some("undo") => {
            let (mut all, mut id, mut journal) = (false, None, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--all" => all = true,
                    "--id" => id = Some(num(it.next(), "--id")?),
                    "--journal" => journal = Some(PathBuf::from(it.next().ok_or("--journal に値がない")?)),
                    o => return Err(format!("不明なオプション: {o}")),
                }
            }
            if all == id.is_some() {
                return Err("undo は --all か --id <N> のどちらか一方を指定する".into());
            }
            Ok(Command::Undo { all, id, journal })
        }
        Some("uninstall-prompt") => {
            let mut ui_level = 0;
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--ui-level" => ui_level = num(it.next(), "--ui-level")?,
                    o => return Err(format!("不明なオプション: {o}")),
                }
            }
            Ok(Command::UninstallPrompt { ui_level })
        }
        Some("diag") => {
            let (mut out, mut data_dir, mut redact) = (None, None, true);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--out" => out = Some(PathBuf::from(it.next().ok_or("--out に値がない")?)),
                    "--data-dir" => data_dir = Some(PathBuf::from(it.next().ok_or("--data-dir に値がない")?)),
                    "--no-redact" => redact = false,
                    o => return Err(format!("不明なオプション: {o}")),
                }
            }
            Ok(Command::Diag { out, data_dir, redact })
        }
        Some("journal") => {
            let mut journal = None;
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--journal" => journal = Some(PathBuf::from(it.next().ok_or("--journal に値がない")?)),
                    o => return Err(format!("不明なオプション: {o}")),
                }
            }
            Ok(Command::Journal { journal })
        }
        Some("history") => {
            let (mut limit, mut db) = (20usize, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--limit" => limit = num(it.next(), "--limit")?,
                    "--db" => db = Some(PathBuf::from(it.next().ok_or("--db に値がない")?)),
                    o => return Err(format!("不明なオプション: {o}")),
                }
            }
            Ok(Command::History { limit, db })
        }
        Some("clean") => {
            let mut o = CleanOpts {
                level: Level::L1, db: None, post_secs: 60, pre_secs: 5, idle_window_secs: 3,
                max_processes: 50, protect: vec![], dry_run: false,
            };
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--level" => o.level = level(it.next())?,
                    "--db" => o.db = Some(PathBuf::from(it.next().ok_or("--db に値がない")?)),
                    "--post-secs" => o.post_secs = num(it.next(), "--post-secs")?,
                    "--pre-secs" => o.pre_secs = num(it.next(), "--pre-secs")?,
                    "--idle-window" => o.idle_window_secs = num(it.next(), "--idle-window")?,
                    "--max-processes" => o.max_processes = num(it.next(), "--max-processes")?,
                    "--protect" => o.protect.push(it.next().ok_or("--protect に値がない")?.clone()),
                    "--dry-run" => o.dry_run = true,
                    x => return Err(format!("不明なオプション: {x}")),
                }
            }
            Ok(Command::Clean(o))
        }
        Some(x) => Err(format!("不明なコマンド: {x}")),
    }
}

fn num<T: std::str::FromStr>(v: Option<&String>, name: &str) -> Result<T, String> {
    v.ok_or(format!("{name} に値がない"))?.parse().map_err(|_| format!("{name} は数値で指定する"))
}

fn level(v: Option<&String>) -> Result<Level, String> {
    match v.map(String::as_str) {
        Some("1") => Ok(Level::L1),
        Some("2") => Ok(Level::L2),
        Some("3") => Ok(Level::L3),
        _ => Err("--level は 1, 2, 3 のいずれか".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn a(s: &str) -> Vec<String> { s.split_whitespace().map(String::from).collect() }

    #[test]
    fn clean_defaults_to_l1_with_60s_post() {
        let Command::Clean(o) = parse(&a("clean")).unwrap() else { panic!() };
        assert_eq!((o.level, o.post_secs, o.max_processes, o.dry_run), (Level::L1, 60, 50, false));
    }
    #[test]
    fn clean_options_parse() {
        let Command::Clean(o) = parse(&a("clean --level 3 --post-secs 5 --protect C:\\a.exe --dry-run")).unwrap() else { panic!() };
        assert_eq!((o.level, o.post_secs, o.dry_run), (Level::L3, 5, true));
        assert_eq!(o.protect, vec!["C:\\a.exe".to_string()]);
    }
    #[test]
    fn invalid_input_is_rejected_with_message() {
        assert!(parse(&a("clean --level 9")).is_err());
        assert!(parse(&a("clean --post-secs x")).is_err());
        assert!(parse(&a("clean --bogus")).is_err());
        assert!(parse(&a("frobnicate")).is_err());
        assert!(parse(&a("clean --level")).is_err());
    }
    #[test]
    fn history_and_help() {
        assert_eq!(parse(&a("history --limit 5")).unwrap(), Command::History { limit: 5, db: None });
        assert_eq!(parse(&[]).unwrap(), Command::Help);
    }

    #[test]
    fn install_and_uninstall_parse() {
        assert_eq!(parse(&a("install")).unwrap(), Command::Install);
        assert_eq!(parse(&a("uninstall")).unwrap(), Command::Uninstall { restore: None });
        assert_eq!(parse(&a("uninstall --restore")).unwrap(), Command::Uninstall { restore: Some(true) });
        assert_eq!(parse(&a("uninstall --keep")).unwrap(), Command::Uninstall { restore: Some(false) });
    }

    #[test]
    fn undo_needs_exactly_one_of_all_or_id() {
        assert_eq!(parse(&a("undo --all")).unwrap(), Command::Undo { all: true, id: None, journal: None });
        assert_eq!(parse(&a("undo --id 7 --journal x.jsonl")).unwrap(), Command::Undo { all: false, id: Some(7), journal: Some("x.jsonl".into()) });
        assert!(parse(&a("undo")).is_err());
        assert!(parse(&a("undo --all --id 3")).is_err());
        assert!(parse(&a("undo --id x")).is_err());
        assert_eq!(parse(&a("journal")).unwrap(), Command::Journal { journal: None });
    }

    #[test]
    fn diag_redacts_by_default() {
        assert_eq!(parse(&a("diag")).unwrap(), Command::Diag { out: None, data_dir: None, redact: true });
        assert_eq!(parse(&a("diag --out x.zip --no-redact")).unwrap(), Command::Diag { out: Some("x.zip".into()), data_dir: None, redact: false });
        assert!(parse(&a("diag --out")).is_err());
    }

    #[test]
    fn uninstall_prompt_takes_the_msi_ui_level() {
        assert_eq!(parse(&a("uninstall-prompt")).unwrap(), Command::UninstallPrompt { ui_level: 0 });
        assert_eq!(parse(&a("uninstall-prompt --ui-level 5")).unwrap(), Command::UninstallPrompt { ui_level: 5 });
        assert!(parse(&a("uninstall-prompt --ui-level x")).is_err());
    }
}
