//! プロセス操作（強制終了・メモリ優先度）。PID の再利用に備え、必ず起動時刻を照合してから操作する。
use memopt_core::ProcessInfo;
use std::collections::HashSet;

/// `roots` の子孫プロセス（プロセスツリー）。親子関係は PID 再利用で偽装され得るので、
/// 「子の起動時刻が親より後」の場合だけたどる。
pub fn descendants(procs: &[ProcessInfo], roots: &[u32]) -> Vec<u32> {
    let mut found: Vec<u32> = Vec::new();
    let mut seen: HashSet<u32> = roots.iter().copied().collect();
    let mut frontier: Vec<u32> = roots.to_vec();
    while let Some(parent) = frontier.pop() {
        let Some(pp) = procs.iter().find(|p| p.pid == parent) else { continue };
        for c in procs {
            let linked = c.parent_pid == parent && c.pid != parent && c.start_time != 0 && c.start_time >= pp.start_time;
            if linked && seen.insert(c.pid) {
                found.push(c.pid);
                frontier.push(c.pid);
            }
        }
    }
    found
}

#[cfg(windows)]
pub use win::{kill_verified, lower_priority, restore_priority};

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::System::Threading::*;

    fn ft(f: FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
    }

    /// PID の起動時刻が期待値と一致するハンドルだけ返す（別プロセスに再利用された PID を弾く）。
    unsafe fn open_verified(pid: u32, start_time: u64, access: PROCESS_ACCESS_RIGHTS) -> Result<HANDLE, String> {
        let h = OpenProcess(access | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(|e| format!("OpenProcess({pid}): {e}"))?;
        let (mut c, mut e, mut k, mut u) = Default::default();
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_err() || ft(c) != start_time {
            let _ = CloseHandle(h);
            return Err(format!("PID {pid} は別のプロセスに置き換わっています"));
        }
        Ok(h)
    }

    /// 1 プロセスを強制終了。成功/失敗の詳細を返す。
    pub fn kill_verified(pid: u32, start_time: u64) -> Result<(), String> {
        unsafe {
            let h = open_verified(pid, start_time, PROCESS_TERMINATE)?;
            let r = TerminateProcess(h, 1).map_err(|e| format!("TerminateProcess({pid}): {e}"));
            let _ = CloseHandle(h);
            r
        }
    }

    /// メモリ優先度を LOW(2) にして、変更前の値を返す。
    pub fn lower_priority(pid: u32, start_time: u64) -> Result<u32, String> {
        unsafe {
            let h = open_verified(pid, start_time, PROCESS_SET_INFORMATION)?;
            let mut cur = MEMORY_PRIORITY_INFORMATION::default();
            let size = size_of::<MEMORY_PRIORITY_INFORMATION>() as u32;
            let before = match GetProcessInformation(h, ProcessMemoryPriority, &mut cur as *mut _ as *mut _, size) {
                Ok(()) => cur.MemoryPriority.0,
                Err(_) => 5, // 取得できなければ NORMAL とみなす
            };
            let new = MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY(2) };
            let r = SetProcessInformation(h, ProcessMemoryPriority, &new as *const _ as *const _, size)
                .map(|_| before)
                .map_err(|e| format!("SetProcessInformation({pid}): {e}"));
            let _ = CloseHandle(h);
            r
        }
    }

    pub fn restore_priority(pid: u32, start_time: u64, before: u32) -> Result<(), String> {
        unsafe {
            let h = open_verified(pid, start_time, PROCESS_SET_INFORMATION)?;
            let v = MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY(before) };
            let r = SetProcessInformation(h, ProcessMemoryPriority, &v as *const _ as *const _, size_of::<MEMORY_PRIORITY_INFORMATION>() as u32)
                .map_err(|e| format!("SetProcessInformation({pid}): {e}"));
            let _ = CloseHandle(h);
            r
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::{Command, Stdio};

        fn start_time_of(pid: u32) -> u64 {
            memopt_probe::processes().into_iter().find(|p| p.pid == pid).map(|p| p.start_time).expect("process")
        }

        fn sleeper() -> std::process::Child {
            Command::new("ping").args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null()).spawn().expect("ping")
        }

        #[test]
        fn kill_verified_terminates_a_child_and_rejects_wrong_start_time() {
            let mut c = sleeper();
            let st = start_time_of(c.id());
            assert!(kill_verified(c.id(), st + 1).unwrap_err().contains("置き換わって"));
            assert!(c.try_wait().unwrap().is_none(), "起動時刻が違うときは終了させない");
            kill_verified(c.id(), st).unwrap();
            assert!(c.wait().is_ok());
        }

        #[test]
        fn lower_then_restore_memory_priority_roundtrips() {
            let mut c = sleeper();
            let st = start_time_of(c.id());
            let before = lower_priority(c.id(), st).unwrap();
            assert!(before >= 1);
            restore_priority(c.id(), st, before).unwrap();
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn p(pid: u32, ppid: u32, start: u64) -> ProcessInfo {
        ProcessInfo { pid, parent_pid: ppid, start_time: start, ..Default::default() }
    }

    #[test]
    fn collects_children_and_grandchildren_but_not_siblings() {
        let ps = vec![p(1, 0, 10), p(2, 1, 20), p(3, 2, 30), p(4, 1, 25), p(9, 8, 5)];
        let mut d = descendants(&ps, &[2]);
        d.sort();
        assert_eq!(d, vec![3]);
        let mut d = descendants(&ps, &[1]);
        d.sort();
        assert_eq!(d, vec![2, 3, 4]);
    }

    #[test]
    fn child_started_before_parent_is_a_reused_pid_and_is_not_followed() {
        // PID 2 は親 1 より前に起動している → 親子関係は偽り（PID 再利用）
        let ps = vec![p(1, 0, 100), p(2, 1, 50)];
        assert!(descendants(&ps, &[1]).is_empty());
    }

    #[test]
    fn cycles_and_unknown_start_times_terminate() {
        let ps = vec![p(1, 2, 10), p(2, 1, 20), p(3, 1, 0)];
        let d = descendants(&ps, &[1]);
        assert!(!d.contains(&3)); // 起動時刻不明は対象外
        assert!(d.len() <= 1);
    }
}
