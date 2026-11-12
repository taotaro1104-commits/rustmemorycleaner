//! ユーザーセッション側でしか取れない状況（設計書3章: 判定は GUI が行いサービスへ通知）。
use memopt_core::ipc::SessionInfo;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::SHQueryUserNotificationState;

// QUERY_USER_NOTIFICATION_STATE の値
const QUNS_BUSY: i32 = 2;
const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
const QUNS_PRESENTATION_MODE: i32 = 4;

/// 全画面アプリ/ゲーム/プレゼン中は自動解放を止める対象。
pub fn is_suppressing_state(state: i32) -> bool {
    matches!(state, QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
}

fn fullscreen() -> bool {
    unsafe { SHQueryUserNotificationState().map(|s| is_suppressing_state(s.0)).unwrap_or(false) }
}

/// 最後の入力からの秒数。
fn idle_secs() -> u64 {
    unsafe {
        let mut li = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if !GetLastInputInfo(&mut li).as_bool() {
            return 0;
        }
        // GetTickCount64 の下位 32 ビットと比較する（dwTime は 32 ビットで周回する）。
        let now = GetTickCount64() as u32;
        now.wrapping_sub(li.dwTime) as u64 / 1000
    }
}

pub fn collect() -> SessionInfo {
    SessionInfo {
        fullscreen: fullscreen(),
        user_idle_secs: idle_secs(),
        foreground_pid: memopt_probe::foreground_pid(),
        audio_pids: memopt_probe::audio_pids().into_iter().collect(),
        battery_saver: memopt_probe::battery_saver_on(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fullscreen_busy_and_presentation_suppress() {
        assert!(is_suppressing_state(QUNS_BUSY));
        assert!(is_suppressing_state(QUNS_RUNNING_D3D_FULL_SCREEN));
        assert!(is_suppressing_state(QUNS_PRESENTATION_MODE));
        for ok in [1, 5, 6, 7] {
            assert!(!is_suppressing_state(ok), "{ok}");
        }
    }

    #[test]
    fn collect_does_not_panic_and_idle_is_sane() {
        let s = collect();
        assert!(s.user_idle_secs < 60 * 60 * 24 * 49); // 49 日で GetTickCount が周回する
    }
}
