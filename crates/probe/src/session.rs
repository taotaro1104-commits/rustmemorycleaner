//! ユーザーセッション側の状況: 最前面プロセス・オーディオ再生中・CPU 使用中。
use std::collections::{HashMap, HashSet};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

pub fn foreground_pid() -> Option<u32> {
    unsafe {
        let h = GetForegroundWindow();
        if h.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

/// 再生中（Active）のオーディオセッションを持つプロセス。取得失敗時は空集合（安全側にはならない点に注意）。
pub fn audio_pids() -> HashSet<u32> {
    let mut out = HashSet::new();
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = (|| -> windows::core::Result<()> {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let sessions = mgr.GetSessionEnumerator()?;
            for i in 0..sessions.GetCount()? {
                let c = sessions.GetSession(i)?;
                if c.GetState()? == AudioSessionStateActive {
                    let c2: IAudioSessionControl2 = windows::core::Interface::cast(&c)?;
                    let pid = c2.GetProcessId()?;
                    if pid != 0 {
                        out.insert(pid);
                    }
                }
            }
            Ok(())
        })();
        if init.is_ok() {
            CoUninitialize();
        }
    }
    out
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// PID -> カーネル+ユーザーの累積 CPU 時間（100ns）。2 回取って差分を取る用。
pub fn cpu_times(pids: &[u32]) -> HashMap<u32, u64> {
    let mut m = HashMap::new();
    for &pid in pids {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { continue };
            let (mut c, mut e, mut k, mut u) = Default::default();
            if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok() {
                m.insert(pid, ft(k) + ft(u));
            }
            let _ = CloseHandle(h);
        }
    }
    m
}

/// 2 回の `cpu_times` の差から、窓内に `threshold_100ns` を超えて CPU を使ったプロセスを返す。
pub fn busy_pids(before: &HashMap<u32, u64>, after: &HashMap<u32, u64>, threshold_100ns: u64) -> HashSet<u32> {
    after
        .iter()
        .filter(|(pid, a)| a.saturating_sub(*before.get(pid).unwrap_or(a)) > threshold_100ns)
        .map(|(pid, _)| *pid)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_pids_uses_delta_over_threshold() {
        let b = HashMap::from([(1, 100), (2, 100), (3, 100)]);
        let a = HashMap::from([(1, 100), (2, 5_000), (3, 150), (4, 9_999)]);
        let busy = busy_pids(&b, &a, 1_000);
        assert!(busy.contains(&2));
        assert!(!busy.contains(&1) && !busy.contains(&3));
        assert!(!busy.contains(&4)); // 新規プロセスは差分が取れない
    }

    #[test]
    fn cpu_times_includes_self() {
        let me = std::process::id();
        assert!(cpu_times(&[me]).contains_key(&me));
    }

    #[test]
    fn audio_and_foreground_calls_do_not_panic() {
        let _ = audio_pids();
        let _ = foreground_pid();
    }
}
