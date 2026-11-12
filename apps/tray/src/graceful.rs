//! 正常終了（設計書5章）: ウィンドウを持つアプリのトップレベルウィンドウへ WM_CLOSE を送り、最大 10 秒待つ。
//! サービスはセッション 0 で動くためデスクトップのウィンドウへ送れず、GUI（ユーザーセッション）が行う。
//! 送る前に、サービスが渡した起動時刻と照合して PID の再利用を弾く。
use std::collections::HashSet;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseResult {
    /// 対象はすべて終了した。
    Closed,
    /// 10 秒以内に終了しなかった（保存確認の待ち、応答なし等）。自動で強制終了はしない。
    TimedOut,
    /// 閉じられるウィンドウが無かった（ウィンドウを持たないプロセス）。
    NoWindows,
}

/// 対象のうち、いま生きていて起動時刻も一致するもの。
fn alive(targets: &[(u32, u64)]) -> HashSet<u32> {
    let procs = memopt_probe::processes();
    targets.iter().filter(|(pid, st)| procs.iter().any(|p| p.pid == *pid && p.start_time == *st)).map(|(pid, _)| *pid).collect()
}

struct Ctx {
    pids: HashSet<u32>,
    posted: usize,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut Ctx);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    // 所有者のある子ダイアログではなくトップレベルの主ウィンドウだけに送る。
    let is_top = GetWindow(hwnd, GW_OWNER).map(|h| h.0.is_null()).unwrap_or(true);
    if ctx.pids.contains(&pid) && is_top && PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
        ctx.posted += 1;
    }
    BOOL(1)
}

fn post_close(pids: &HashSet<u32>) -> usize {
    let mut ctx = Ctx { pids: pids.clone(), posted: 0 };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.posted
}

pub fn close(targets: &[(u32, u64)], timeout: Duration) -> CloseResult {
    let live = alive(targets);
    if live.is_empty() {
        return CloseResult::Closed; // すでに終了している
    }
    if post_close(&live) == 0 {
        return CloseResult::NoWindows;
    }
    let t = Instant::now();
    while t.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(250));
        if alive(targets).is_empty() {
            return CloseResult::Closed;
        }
    }
    CloseResult::TimedOut
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn start_time(pid: u32) -> u64 {
        memopt_probe::processes().into_iter().find(|p| p.pid == pid).map(|p| p.start_time).expect("process")
    }

    /// charmap は Win32 の通常アプリ（UWP の起動シムではない）で、保存確認も出さずに閉じられる。
    fn spawn_gui() -> Child {
        let c = Command::new(r"C:\Windows\System32\charmap.exe").spawn().expect("charmap");
        std::thread::sleep(Duration::from_millis(1500));
        c
    }

    #[test]
    fn closes_a_real_window_gracefully() {
        let mut c = spawn_gui();
        let r = close(&[(c.id(), start_time(c.id()))], Duration::from_secs(10));
        assert_eq!(r, CloseResult::Closed);
        assert!(c.wait().unwrap().code().is_some());
    }

    #[test]
    fn process_without_windows_reports_no_windows_and_is_left_running() {
        let mut c = Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let r = close(&[(c.id(), start_time(c.id()))], Duration::from_secs(2));
        assert_eq!(r, CloseResult::NoWindows);
        assert!(c.try_wait().unwrap().is_none());
        let _ = c.kill();
    }

    #[test]
    fn a_reused_pid_with_a_different_start_time_is_never_signalled() {
        let mut c = spawn_gui();
        let r = close(&[(c.id(), start_time(c.id()) + 1)], Duration::from_secs(1));
        assert_eq!(r, CloseResult::Closed, "起動時刻が合わない PID は『すでに終了』として扱う");
        assert!(c.try_wait().unwrap().is_none(), "別プロセスのウィンドウを閉じてはいけない");
        let _ = c.kill();
    }

    #[test]
    fn already_exited_targets_are_reported_closed() {
        assert_eq!(close(&[(u32::MAX - 3, 1)], Duration::from_secs(1)), CloseResult::Closed);
    }
}
