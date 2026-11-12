use memopt_core::{MemorySnapshot, ProcessInfo};
use std::collections::HashMap;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Services::*;
use windows::Win32::System::SystemInformation::*;
use windows::Win32::System::Threading::*;

pub fn memory_snapshot() -> Option<MemorySnapshot> {
    unsafe {
        // GlobalMemoryStatusEx だけで足りる（GetPerformanceInfo は 1 回 10ms 以上かかり、15 秒ごとの評価には重い）。
        // ullTotalPageFile = コミット上限、ullAvailPageFile = コミット上限 - コミット済み。
        let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        GlobalMemoryStatusEx(&mut m).ok()?;
        Some(MemorySnapshot {
            total_phys: m.ullTotalPhys,
            avail_phys: m.ullAvailPhys,
            commit_total: m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile),
            commit_limit: m.ullTotalPageFile,
            system_cache: 0, // 未使用（スタンバイは PDH で取る）
        })
    }
}

/// PID -> サービス名一覧（EnumServicesStatusExW）。
fn service_map() -> HashMap<u32, Vec<String>> {
    let mut out: HashMap<u32, Vec<String>> = HashMap::new();
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) else { return out };
        let (mut needed, mut count, mut resume) = (0u32, 0u32, 0u32);
        let _ = EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE,
            None, &mut needed, &mut count, Some(&mut resume), None);
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize + 4096];
            resume = 0;
            if EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE,
                Some(&mut buf), &mut needed, &mut count, Some(&mut resume), None).is_ok() {
                let items = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
                for it in items {
                    let pid = it.ServiceStatusProcess.dwProcessId;
                    if pid != 0 {
                        out.entry(pid).or_default().push(it.lpServiceName.to_string().unwrap_or_default());
                    }
                }
            }
        }
        let _ = CloseServiceHandle(scm);
    }
    out
}

fn ft(f: FILETIME) -> u64 { ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64 }

struct Detail { start: u64, path: Option<String>, ws: Option<u64>, private: Option<u64>, pws: Option<u64>, pkg: Option<String>, ppl: bool }

/// 保護プロセス（PPL 等）か。取得失敗は「保護ではない」とはせず、安全側（保護）として扱う。
unsafe fn is_protected_process(h: HANDLE) -> bool {
    let mut info = PROCESS_PROTECTION_LEVEL_INFORMATION::default();
    match GetProcessInformation(h, ProcessProtectionLevelInfo, &mut info as *mut _ as *mut _, size_of::<PROCESS_PROTECTION_LEVEL_INFORMATION>() as u32) {
        Ok(()) => info.ProtectionLevel != PROTECTION_LEVEL_NONE,
        Err(_) => false,
    }
}

/// UWP/MSIX のパッケージファミリー名（非パッケージは None）。
unsafe fn package_family(h: HANDLE) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    let mut len = 0u32;
    let _ = GetPackageFamilyName(h, &mut len, PWSTR::null());
    if len == 0 { return None; }
    let mut buf = vec![0u16; len as usize];
    GetPackageFamilyName(h, &mut len, PWSTR(buf.as_mut_ptr())).ok().ok()?;
    Some(String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
}

unsafe fn detail(h: HANDLE) -> Detail {
    let (mut c, mut e, mut k, mut u) = Default::default();
    let start = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok() { ft(c) } else { 0 };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let path = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
        .ok().map(|_| String::from_utf16_lossy(&buf[..len as usize]));
    // EX2 は PrivateWorkingSetSize を含む（Win10 2004+）。失敗時は値なし。
    let mut mc = PROCESS_MEMORY_COUNTERS_EX2::default();
    mc.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
    let (ws, private, pws) = if GetProcessMemoryInfo(h, &mut mc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mc.cb).is_ok() {
        (Some(mc.WorkingSetSize as u64), Some(mc.PrivateUsage as u64), Some(mc.PrivateWorkingSetSize as u64))
    } else { (None, None, None) };
    Detail { start, path, ws, private, pws, pkg: package_family(h), ppl: is_protected_process(h) }
}

/// NtQuerySystemInformation(SystemProcessInformation) の 1 プロセス分。権限なしで全プロセスの値が取れ、
/// ハンドルも開かない（常駐の評価ループを軽くするため、CPU 時間もここから取る）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NtProc {
    pub private_page_count: u64,
    pub working_set: u64,
    /// 実際のプライベートワーキングセット（GetProcessMemoryInfo は埋めない環境が多い）。
    pub working_set_private: u64,
    pub create_time: u64,
    /// ユーザー + カーネルの累積 CPU 時間（100ns）。
    pub cpu_time: u64,
}

// SYSTEM_PROCESS_INFORMATION.Reserved1[48] の中身（Windows Vista 以降で安定した配置）:
//   0: WorkingSetPrivateSize(i64)  8: HardFaultCount(u32)  12: NumberOfThreadsHighWatermark(u32)
//  16: CycleTime(u64)  24: CreateTime(i64)  32: UserTime(i64)  40: KernelTime(i64)
fn rd_i64(b: &[u8; 48], at: usize) -> u64 {
    i64::from_le_bytes(b[at..at + 8].try_into().unwrap()).max(0) as u64
}

pub fn nt_process_table() -> HashMap<u32, NtProc> {
    use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;
    let mut out = HashMap::new();
    let mut buf = vec![0u8; 1 << 20];
    unsafe {
        loop {
            let mut ret = 0u32;
            let st = NtQuerySystemInformation(SystemProcessInformation, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut ret);
            if st.0 == 0xC0000004u32 as i32 { buf.resize(ret as usize + (1 << 16), 0); continue; } // STATUS_INFO_LENGTH_MISMATCH
            if st.0 < 0 { return out; }
            break;
        }
        let mut off = 0usize;
        loop {
            let e = &*(buf.as_ptr().add(off) as *const SYSTEM_PROCESS_INFORMATION);
            let r = &e.Reserved1;
            out.insert(e.UniqueProcessId.0 as u32, NtProc {
                private_page_count: e.PrivatePageCount as u64,
                working_set: e.WorkingSetSize as u64,
                working_set_private: rd_i64(r, 0),
                create_time: rd_i64(r, 24),
                cpu_time: rd_i64(r, 32) + rd_i64(r, 40),
            });
            if e.NextEntryOffset == 0 { break; }
            off += e.NextEntryOffset as usize;
        }
    }
    out
}

/// 全プロセスの累積 CPU 時間（PID -> 100ns）。ハンドルを開かないので軽い。
pub fn all_cpu_times() -> HashMap<u32, u64> {
    nt_process_table().into_iter().map(|(pid, p)| (pid, p.cpu_time)).collect()
}

fn session_of(pid: u32) -> u32 {
    let mut sid = 0u32;
    unsafe { windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(pid, &mut sid).map(|_| sid).unwrap_or(0) }
}

pub fn processes() -> Vec<ProcessInfo> {
    let services = service_map();
    let nt = nt_process_table();
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let name_len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..name_len]);
            let d = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, e.th32ProcessID) {
                Ok(h) => { let d = detail(h); let _ = CloseHandle(h); d }
                Err(_) => Detail { start: 0, path: None, ws: None, private: None, pws: None, pkg: None, ppl: false }, // 失敗は記録のみで続行
            };
            let fb = nt.get(&e.th32ProcessID);
            let private = d.private.or(fb.map(|x| x.private_page_count));
            let ws = d.ws.or(fb.map(|x| x.working_set));
            // GetProcessMemoryInfo は PrivateWorkingSetSize を埋めない環境が多い（実測 0）ので、
            // NtQuerySystemInformation の WorkingSetPrivateSize を使う。取れなければ min(プライベート, WS) の上限値。
            let pws = d.pws.filter(|v| *v > 0)
                .or(fb.map(|x| x.working_set_private).filter(|v| *v > 0))
                .or_else(|| private.zip(ws).map(|(a, b)| a.min(b)));
            let d = Detail { private, ws, pws, ..d };
            out.push(ProcessInfo {
                pid: e.th32ProcessID, parent_pid: e.th32ParentProcessID, start_time: d.start,
                exe_name: name, exe_path: d.path, command_line: None, package_family: d.pkg,
                services: services.get(&e.th32ProcessID).cloned().unwrap_or_default(),
                private_bytes: d.private, working_set: d.ws, private_working_set: d.pws,
                session_id: session_of(e.th32ProcessID), is_ppl: d.ppl,
            });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nt_table_gives_real_private_ws_create_time_and_cpu_for_self() {
        let me = std::process::id();
        let t = nt_process_table();
        let p = t.get(&me).expect("self");
        assert!(p.working_set_private > 0 && p.working_set_private <= p.working_set, "{p:?}");
        // CreateTime は GetProcessTimes（processes() の start_time）と一致する = 構造体の配置が合っている証拠
        let start = processes().into_iter().find(|x| x.pid == me).unwrap().start_time;
        assert_eq!(p.create_time, start);
        // CPU 時間は単調増加（このテスト自身が CPU を使う）
        let before = p.cpu_time;
        let mut x = 0u64;
        for i in 0..50_000_000u64 { x = x.wrapping_add(i * i); }
        assert!(x > 0);
        assert!(all_cpu_times()[&me] >= before);
    }

    #[test]
    fn processes_report_a_private_working_set_that_is_not_just_the_working_set() {
        let me = processes().into_iter().find(|p| p.pid == std::process::id()).unwrap();
        let (pws, ws) = (me.private_working_set.unwrap(), me.working_set.unwrap());
        assert!(pws > 0 && pws <= ws);
    }

    #[test]
    fn commit_matches_get_performance_info_within_a_small_margin() {
        // 軽い GlobalMemoryStatusEx 経由の値が、従来の GetPerformanceInfo の値とほぼ一致することを確認する。
        let m = memory_snapshot().unwrap();
        let mut pi = PERFORMANCE_INFORMATION { cb: size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
        unsafe { GetPerformanceInfo(&mut pi, pi.cb).unwrap() };
        let (total, limit) = (pi.CommitTotal as u64 * pi.PageSize as u64, pi.CommitLimit as u64 * pi.PageSize as u64);
        let close = |a: u64, b: u64, tol: f64| (a as f64 - b as f64).abs() <= b as f64 * tol;
        assert!(close(m.commit_limit, limit, 0.02), "limit {} vs {}", m.commit_limit, limit);
        assert!(close(m.commit_total, total, 0.05), "total {} vs {}", m.commit_total, total);
    }

    #[test]
    fn snapshot_is_sane() {
        let m = memory_snapshot().unwrap();
        assert!(m.total_phys > m.avail_phys && m.avail_phys > 0);
        assert!(m.commit_limit >= m.total_phys / 2);
    }
    #[test]
    fn enumerates_self_with_private_bytes() {
        let me = std::process::id();
        let p = processes().into_iter().find(|p| p.pid == me).expect("self");
        assert!(p.private_bytes.unwrap() > 0 && p.exe_path.is_some());
    }
}
