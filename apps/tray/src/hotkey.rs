//! "Ctrl+Alt+M" 形式のショートカット文字列の解析（config.toml の ui.hotkey_clean）。
use global_hotkey::hotkey::{Code, HotKey, Modifiers};

fn key_code(s: &str) -> Option<Code> {
    let u = s.to_ascii_uppercase();
    if u.len() == 1 {
        let c = u.chars().next()?;
        return match c {
            'A' => Some(Code::KeyA), 'B' => Some(Code::KeyB), 'C' => Some(Code::KeyC), 'D' => Some(Code::KeyD),
            'E' => Some(Code::KeyE), 'F' => Some(Code::KeyF), 'G' => Some(Code::KeyG), 'H' => Some(Code::KeyH),
            'I' => Some(Code::KeyI), 'J' => Some(Code::KeyJ), 'K' => Some(Code::KeyK), 'L' => Some(Code::KeyL),
            'M' => Some(Code::KeyM), 'N' => Some(Code::KeyN), 'O' => Some(Code::KeyO), 'P' => Some(Code::KeyP),
            'Q' => Some(Code::KeyQ), 'R' => Some(Code::KeyR), 'S' => Some(Code::KeyS), 'T' => Some(Code::KeyT),
            'U' => Some(Code::KeyU), 'V' => Some(Code::KeyV), 'W' => Some(Code::KeyW), 'X' => Some(Code::KeyX),
            'Y' => Some(Code::KeyY), 'Z' => Some(Code::KeyZ),
            '0' => Some(Code::Digit0), '1' => Some(Code::Digit1), '2' => Some(Code::Digit2), '3' => Some(Code::Digit3),
            '4' => Some(Code::Digit4), '5' => Some(Code::Digit5), '6' => Some(Code::Digit6), '7' => Some(Code::Digit7),
            '8' => Some(Code::Digit8), '9' => Some(Code::Digit9),
            _ => None,
        };
    }
    match u.as_str() {
        "F1" => Some(Code::F1), "F2" => Some(Code::F2), "F3" => Some(Code::F3), "F4" => Some(Code::F4),
        "F5" => Some(Code::F5), "F6" => Some(Code::F6), "F7" => Some(Code::F7), "F8" => Some(Code::F8),
        "F9" => Some(Code::F9), "F10" => Some(Code::F10), "F11" => Some(Code::F11), "F12" => Some(Code::F12),
        _ => None,
    }
}

/// 修飾キーが 1 つも無いショートカットは通常の入力を奪うため拒否する。
pub fn parse(s: &str) -> Option<(Modifiers, Code)> {
    let mut mods = Modifiers::empty();
    let mut key = None;
    for part in s.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            "win" | "super" | "meta" => mods |= Modifiers::SUPER,
            _ => {
                if key.is_some() {
                    return None;
                }
                key = Some(key_code(part)?);
            }
        }
    }
    (!mods.is_empty()).then_some(())?;
    key.map(|k| (mods, k))
}

pub fn to_hotkey(s: &str) -> Option<HotKey> {
    parse(s).map(|(m, c)| HotKey::new(Some(m), c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkey_parses() {
        assert_eq!(parse("Ctrl+Alt+M"), Some((Modifiers::CONTROL | Modifiers::ALT, Code::KeyM)));
    }

    #[test]
    fn case_spacing_and_function_keys() {
        assert_eq!(parse("ctrl + shift + f9"), Some((Modifiers::CONTROL | Modifiers::SHIFT, Code::F9)));
        assert_eq!(parse("Win+7"), Some((Modifiers::SUPER, Code::Digit7)));
    }

    #[test]
    fn invalid_shortcuts_are_rejected() {
        for bad in ["", "M", "Ctrl+Alt", "Ctrl+Alt+MM", "Ctrl+M+K", "Ctrl+Alt+?", "Banana+M"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
