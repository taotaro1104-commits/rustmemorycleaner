/// Run キー値のコマンド文字列から実行ファイルのパスを取り出す。
/// 引用符付き / 引用符なし（".exe" で終わる最短一致）に対応し、環境変数は展開済みの前提。
pub fn extract_exe_path(cmd: &str) -> Option<String> {
    let c = cmd.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return rest.find('"').map(|i| rest[..i].to_string()).filter(|s| !s.is_empty());
    }
    let lower = c.to_ascii_lowercase();
    lower.find(".exe").map(|i| c[..i + 4].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_with_args() {
        assert_eq!(extract_exe_path(r#""C:\Program Files\A\a.exe" --min"#).as_deref(), Some(r"C:\Program Files\A\a.exe"));
    }
    #[test]
    fn unquoted_with_args() {
        assert_eq!(extract_exe_path(r"C:\x\b.exe /silent").as_deref(), Some(r"C:\x\b.exe"));
    }
    #[test]
    fn no_exe_or_empty_quote_is_none() {
        assert_eq!(extract_exe_path("rundll32 foo"), None);
        assert_eq!(extract_exe_path(r#""" x"#), None);
    }
}
