//! セッションに依存しないシステム状態（サービスから呼べる）。
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::*;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{GetLocalTime, GetTickCount64};

/// 起動後の経過秒。
pub fn uptime_secs() -> u64 {
    unsafe { GetTickCount64() / 1000 }
}

/// ローカル時刻（cron 判定用）。(分, 時, 日, 月, 曜日0=日)
pub fn local_time_parts() -> (u32, u32, u32, u32, u32) {
    let t = unsafe { GetLocalTime() };
    (t.wMinute as u32, t.wHour as u32, t.wDay as u32, t.wMonth as u32, t.wDayOfWeek as u32)
}

/// 「バッテリー駆動 かつ 省電力モード」のときだけ true（設計書3章の抑止条件）。
pub fn battery_saver_on() -> bool {
    unsafe {
        let mut s = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut s).is_err() {
            return false;
        }
        let on_battery = s.ACLineStatus == 0;
        on_battery && s.SystemStatusFlag == 1
    }
}

/// 低メモリ通知（CreateMemoryResourceNotification）。
pub struct LowMemoryWatcher(HANDLE);

impl LowMemoryWatcher {
    pub fn new() -> Option<Self> {
        unsafe { CreateMemoryResourceNotification(LowMemoryResourceNotification).ok().map(Self) }
    }

    /// 現在シグナル状態（低メモリ）か。
    pub fn is_low(&self) -> bool {
        let mut state = false.into();
        unsafe { QueryMemoryResourceNotification(self.0, &mut state).is_ok() && state.as_bool() }
    }
}

impl Drop for LowMemoryWatcher {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_positive() {
        assert!(uptime_secs() > 0);
    }

    #[test]
    fn low_memory_watcher_can_be_created_and_queried() {
        let w = LowMemoryWatcher::new().expect("watcher");
        let _ = w.is_low(); // 値は環境依存。呼べることを確認する
    }

    #[test]
    fn battery_saver_query_does_not_panic() {
        let _ = battery_saver_on();
    }
}
