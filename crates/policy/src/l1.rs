//! L1（ワーキングセット整理）の対象選び（設計書3章・6章）。
use memopt_core::ProcessInfo;
use std::collections::HashSet;

pub struct L1Context<'a> {
    pub now_filetime: u64,
    pub self_pid: u32,
    pub foreground_pid: Option<u32>,
    pub audio_pids: HashSet<u32>,
    /// 直近 N 秒に CPU を使っていたプロセス（呼び出し側が CPU 時間差分から算出）。
    pub busy_pids: HashSet<u32>,
    pub user_protected: &'a [String],
    pub max_processes: usize,
    pub skip_foreground: bool,
    pub skip_audio: bool,
}

const FILETIME_PER_SEC: u64 = 10_000_000;
const RECENT_START_SECS: u64 = 60;

/// 整理対象の PID を、WS の大きい順に最大 `max_processes` 件返す。
/// 最前面・オーディオ・直近60秒起動・CPU使用中・保護（組み込み/ユーザー）・自分は除外。
pub fn select_l1_targets(procs: &[ProcessInfo], c: &L1Context) -> Vec<u32> {
    let mut v: Vec<(&ProcessInfo, u64)> = procs.iter()
        .filter(|p| p.pid != 0 && p.pid != 4)
        .filter(|p| p.pid != c.self_pid)
        .filter(|p| !(c.skip_foreground && c.foreground_pid == Some(p.pid)))
        .filter(|p| !(c.skip_audio && c.audio_pids.contains(&p.pid)))
        .filter(|p| !c.busy_pids.contains(&p.pid))
        .filter(|p| !memopt_grouping::is_protected(p, c.user_protected))
        // 起動時刻が不明(0)＝開けないプロセスは対象外。直近起動も除外。
        .filter(|p| p.start_time != 0
            && c.now_filetime.saturating_sub(p.start_time) >= RECENT_START_SECS * FILETIME_PER_SEC)
        .filter_map(|p| p.working_set.filter(|w| *w > 0).map(|w| (p, w)))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(c.max_processes);
    v.into_iter().map(|(p, _)| p.pid).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: u64 = 1_000 * FILETIME_PER_SEC;

    fn proc(pid: u32, ws: u64) -> ProcessInfo {
        ProcessInfo {
            pid, parent_pid: 1, start_time: NOW - 600 * FILETIME_PER_SEC,
            exe_name: format!("p{pid}.exe"), exe_path: Some(format!(r"C:\apps\p{pid}.exe")),
            command_line: None, package_family: None, services: vec![],
            private_bytes: Some(ws), working_set: Some(ws), private_working_set: Some(ws),
            ..Default::default()
        }
    }
    fn ctx<'a>(user: &'a [String]) -> L1Context<'a> {
        L1Context { now_filetime: NOW, self_pid: 999, foreground_pid: None, audio_pids: HashSet::new(),
            busy_pids: HashSet::new(), user_protected: user, max_processes: 50,
            skip_foreground: true, skip_audio: true }
    }

    #[test]
    fn sorted_by_working_set_desc_and_capped() {
        let ps = vec![proc(10, 100), proc(11, 300), proc(12, 200)];
        let mut c = ctx(&[]); c.max_processes = 2;
        assert_eq!(select_l1_targets(&ps, &c), vec![11, 12]);
    }
    #[test]
    fn excludes_foreground_audio_busy_self() {
        let ps = vec![proc(1, 10), proc(2, 20), proc(3, 30), proc(999, 40), proc(5, 50)];
        let mut c = ctx(&[]);
        c.foreground_pid = Some(1); c.audio_pids.insert(2); c.busy_pids.insert(3);
        assert_eq!(select_l1_targets(&ps, &c), vec![5]);
    }
    #[test]
    fn foreground_and_audio_flags_can_be_disabled() {
        let ps = vec![proc(1, 10), proc(2, 20)];
        let mut c = ctx(&[]);
        c.foreground_pid = Some(1); c.audio_pids.insert(2);
        c.skip_foreground = false; c.skip_audio = false;
        assert_eq!(select_l1_targets(&ps, &c).len(), 2);
    }
    #[test]
    fn excludes_recently_started_and_unknown_start() {
        let mut young = proc(1, 10); young.start_time = NOW - 30 * FILETIME_PER_SEC;
        let mut unknown = proc(2, 10); unknown.start_time = 0;
        let ok = proc(3, 10);
        assert_eq!(select_l1_targets(&[young, unknown, ok], &ctx(&[])), vec![3]);
    }
    #[test]
    fn excludes_builtin_and_user_protected_but_not_lookalikes() {
        let mut real = proc(1, 10); real.exe_name = "lsass.exe".into(); real.exe_path = Some(r"C:\Windows\System32\lsass.exe".into());
        let mut fake = proc(2, 10); fake.exe_name = "lsass.exe".into(); fake.exe_path = Some(r"C:\tmp\lsass.exe".into());
        let daw = proc(3, 10);
        let user = vec![r"C:\apps\p3.exe".to_string()];
        assert_eq!(select_l1_targets(&[real, fake, daw], &ctx(&user)), vec![2]);
    }
    #[test]
    fn skips_unreadable_and_zero_ws_and_system_pids() {
        let mut none = proc(1, 10); none.working_set = None;
        let zero = proc(2, 0);
        let sys = proc(4, 100);
        assert!(select_l1_targets(&[none, zero, sys], &ctx(&[])).is_empty());
    }
}
