//! データ置き場（設計書6・7章）: %ProgramData%\MemOpt\。開発/テスト用に MEMOPT_DATA_DIR で上書きできる。
use std::path::PathBuf;

pub const DATA_DIR_ENV: &str = "MEMOPT_DATA_DIR";

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("MemOpt")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.db")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}
