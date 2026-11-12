use crate::memlist::{memory_list, MemoryListCommand};
use crate::priv_token::{enable_privilege, Privilege};
use crate::report::CleanReport;
use memopt_core::Level;
use std::time::Instant;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Memory::{SetProcessWorkingSetSizeEx, SETPROCESSWORKINGSETSIZEEX_FLAGS};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA};

/// そのレベルが今の権限で実行可能か（GUI の「利用不可」表示用）。
#[derive(Debug, Clone, PartialEq)]
pub enum Availability {
    Ready,
    Unavailable(String),
}

fn needs(level: Level) -> Option<Privilege> {
    match level {
        Level::L1 => None,
        Level::L2 | Level::L3 => Some(Privilege::ProfileSingleProcess),
    }
}

pub fn availability(level: Level) -> Availability {
    match needs(level) {
        None => Availability::Ready,
        Some(p) => match enable_privilege(p) {
            Ok(()) => Availability::Ready,
            Err(e) => Availability::Unavailable(e),
        },
    }
}

/// L1: 指定 PID のワーキングセットを整理（EmptyWorkingSet 相当）。個別失敗は記録して次へ。
pub fn run_l1(pids: &[u32]) -> CleanReport {
    let t = Instant::now();
    let mut r = CleanReport::new(Level::L1);
    for &pid in pids {
        unsafe {
            match OpenProcess(PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Err(e) => r.record_fail(pid.to_string(), e.code().0, format!("OpenProcess: {e}")),
                Ok(h) => {
                    let res = SetProcessWorkingSetSizeEx(h, usize::MAX, usize::MAX, SETPROCESSWORKINGSETSIZEEX_FLAGS(0));
                    match res {
                        Ok(()) => r.record_ok(),
                        Err(e) => r.record_fail(pid.to_string(), e.code().0, format!("SetProcessWorkingSetSizeEx: {e}")),
                    }
                    let _ = CloseHandle(h);
                }
            }
        }
    }
    r.elapsed_ms = t.elapsed().as_millis();
    r
}

fn nt(r: &mut CleanReport, what: &str, status: i32) {
    if status >= 0 {
        r.record_ok()
    } else {
        r.record_fail(what, status, format!("NTSTATUS 0x{:08X}", status as u32))
    }
}

/// L2/L3。L1 は対象選びが必要なので `run_l1` を使う。
pub fn run_level(level: Level) -> CleanReport {
    if level == Level::L1 {
        return CleanReport::unavailable(level, "L1 は run_l1 に PID を渡して実行する");
    }
    if let Availability::Unavailable(why) = availability(level) {
        return CleanReport::unavailable(level, why);
    }
    let t = Instant::now();
    let mut r = CleanReport::new(level);
    match level {
        Level::L2 => nt(&mut r, "PurgeLowPriorityStandbyList", memory_list(MemoryListCommand::PurgeLowPriorityStandbyList)),
        Level::L3 => {
            nt(&mut r, "FlushModifiedList", memory_list(MemoryListCommand::FlushModifiedList));
            nt(&mut r, "PurgeStandbyList", memory_list(MemoryListCommand::PurgeStandbyList));
        }
        Level::L1 => unreachable!(),
    }
    r.elapsed_ms = t.elapsed().as_millis();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l1_failure_on_nonexistent_pid_is_recorded_not_fatal() {
        let r = run_l1(&[u32::MAX - 1, std::process::id()]);
        assert_eq!(r.attempted, 2);
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.succeeded, 1); // 自プロセスは整理できる
    }

    #[test]
    fn l2_l3_report_unavailable_without_privilege_instead_of_failing() {
        // 非管理者なら unavailable、管理者なら実行結果。どちらでもパニックしない。
        for lv in [Level::L2, Level::L3] {
            let r = run_level(lv);
            assert!(r.unavailable.is_some() || r.attempted > 0);
        }
    }

    #[test]
    fn l1_via_run_level_is_rejected() {
        assert!(run_level(Level::L1).unavailable.is_some());
    }
}
