//! config.toml の読み書き。検証してから保存し、保存は一時ファイル経由で原子的に行う。
use memopt_core::Config;
use std::path::Path;

/// ファイルが無ければ既定値（警告なし）。読めない/不正なら既定値＋警告。
pub fn load(path: &Path) -> (Config, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse_and_validate(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), vec![]),
        Err(e) => (Config::default(), vec![format!("{}: 読み込めません ({e})。既定値を使います", path.display())]),
    }
}

/// 検証済みの設定を保存（途中で落ちても壊れた config.toml が残らない）。
pub fn save(path: &Path, cfg: &Config) -> Result<(), String> {
    let text = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// GUI からの更新: 解析・検証し、補正済みの内容を保存して返す。解析不能なら保存しない。
pub fn apply_update(path: &Path, toml_text: &str) -> Result<(Config, Vec<String>), String> {
    // 解析不能（構文エラー）の場合は既定値で上書きしてしまわない。
    if toml::from_str::<toml::Table>(toml_text).is_err() {
        return Err("TOML として解析できません。保存しませんでした".into());
    }
    let (cfg, warnings) = Config::parse_and_validate(toml_text);
    save(path, &cfg)?;
    Ok((cfg, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一時フォルダー。テスト終了時に削除する。
    struct Tmp(std::path::PathBuf);
    impl std::ops::Deref for Tmp {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("memopt_cfg_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    #[test]
    fn missing_file_gives_defaults_without_warning() {
        let d = tmp("missing");
        let (c, w) = load(&d.join("config.toml"));
        assert_eq!(c, Config::default());
        assert!(w.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let d = tmp("rt");
        let p = d.join("config.toml");
        let mut c = Config::default();
        c.auto_clean.cooldown_sec = 600;
        c.protect.user_paths = vec![r"C:\a\b.exe".into()];
        save(&p, &c).unwrap();
        assert!(!d.join("config.toml.tmp").exists());
        assert_eq!(load(&p).0, c);
    }

    #[test]
    fn update_sanitizes_and_persists_corrected_values() {
        let d = tmp("upd");
        let p = d.join("config.toml");
        let (c, w) = apply_update(&p, "[auto_clean]\nmax_auto_level = 9").unwrap();
        assert_eq!(c.auto_clean.max_auto_level, 2);
        assert_eq!(w.len(), 1);
        assert_eq!(load(&p).0.auto_clean.max_auto_level, 2);
    }

    #[test]
    fn syntactically_broken_update_is_rejected_and_keeps_old_file() {
        let d = tmp("bad");
        let p = d.join("config.toml");
        let mut c = Config::default();
        c.l1.max_processes = 7;
        save(&p, &c).unwrap();
        assert!(apply_update(&p, "= = [").is_err());
        assert_eq!(load(&p).0.l1.max_processes, 7);
    }
}
