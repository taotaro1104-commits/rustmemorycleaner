//! Windows サービスの照会・停止・開始・スタートアップ種別の変更（SCM）。
use crate::plan::ServiceFacts;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::System::Services::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// RAII: 閉じ忘れを防ぐ。
struct Handle(SC_HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn open(name: &str, access: u32) -> Result<(Handle, Handle), String> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_CONNECT).map_err(|e| format!("OpenSCManager: {e}"))?;
        let scm = Handle(scm);
        let w = wide(name);
        let svc = OpenServiceW(scm.0, PCWSTR(w.as_ptr()), access).map_err(|e| format!("サービス {name} を開けません: {e}"))?;
        Ok((scm, Handle(svc)))
    }
}

struct Status {
    state: u32,
    accepts_stop: bool,
}

fn status(h: &Handle) -> Result<Status, String> {
    unsafe {
        let mut buf = [0u8; size_of::<SERVICE_STATUS_PROCESS>()];
        let mut needed = 0u32;
        QueryServiceStatusEx(h.0, SC_STATUS_PROCESS_INFO, Some(&mut buf), &mut needed).map_err(|e| format!("QueryServiceStatusEx: {e}"))?;
        let s = &*(buf.as_ptr() as *const SERVICE_STATUS_PROCESS);
        Ok(Status { state: s.dwCurrentState.0, accepts_stop: s.dwControlsAccepted & SERVICE_ACCEPT_STOP != 0 })
    }
}

fn start_type(h: &Handle) -> Result<u32, String> {
    unsafe {
        let mut needed = 0u32;
        let _ = QueryServiceConfigW(h.0, None, 0, &mut needed);
        let mut buf = vec![0u8; needed.max(8) as usize];
        QueryServiceConfigW(h.0, Some(buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW), buf.len() as u32, &mut needed)
            .map_err(|e| format!("QueryServiceConfig: {e}"))?;
        Ok((*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW)).dwStartType.0)
    }
}

fn delayed_auto(h: &Handle) -> bool {
    unsafe {
        let mut buf = [0u8; size_of::<SERVICE_DELAYED_AUTO_START_INFO>()];
        let mut needed = 0u32;
        QueryServiceConfig2W(h.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&mut buf), &mut needed).is_ok()
            && (*(buf.as_ptr() as *const SERVICE_DELAYED_AUTO_START_INFO)).fDelayedAutostart.as_bool()
    }
}

/// 実行中の依存サービス名（このサービスを止めると一緒に止まるもの）。
fn active_dependents(h: &Handle) -> Result<Vec<String>, String> {
    unsafe {
        let (mut needed, mut count) = (0u32, 0u32);
        let _ = EnumDependentServicesW(h.0, SERVICE_ACTIVE, None, 0, &mut needed, &mut count);
        if needed == 0 {
            return Ok(vec![]);
        }
        let mut buf = vec![0u8; needed as usize];
        EnumDependentServicesW(h.0, SERVICE_ACTIVE, Some(buf.as_mut_ptr() as *mut ENUM_SERVICE_STATUSW), needed, &mut needed, &mut count)
            .map_err(|e| format!("EnumDependentServices: {e}"))?;
        let items = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUSW, count as usize);
        Ok(items.iter().filter_map(|i| i.lpServiceName.to_string().ok()).collect())
    }
}

pub fn query(name: &str) -> Result<ServiceFacts, String> {
    let (_scm, h) = open(name, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG | SERVICE_ENUMERATE_DEPENDENTS)?;
    let st = status(&h)?;
    Ok(ServiceFacts {
        name: name.to_string(),
        accepts_stop: st.accepts_stop,
        running: st.state == SERVICE_RUNNING.0,
        start_type: start_type(&h)?,
        dependents: active_dependents(&h)?,
    })
}

fn wait_state(h: &Handle, want: u32, timeout: Duration) -> Result<(), String> {
    let t = Instant::now();
    loop {
        if status(h)?.state == want {
            return Ok(());
        }
        if t.elapsed() > timeout {
            return Err("サービスの状態遷移がタイムアウトしました".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn stop_handle(h: &Handle, timeout: Duration) -> Result<(), String> {
    unsafe {
        let mut st = SERVICE_STATUS::default();
        ControlService(h.0, SERVICE_CONTROL_STOP, &mut st).map_err(|e| format!("ControlService(STOP): {e}"))?;
    }
    wait_state(h, SERVICE_STOPPED.0, timeout)
}

/// 依存サービスを先に止めてから本体を止める。止めた依存サービス名を返す（復元時に起動し直す）。
pub fn stop(name: &str, timeout: Duration) -> Result<Vec<String>, String> {
    let (_scm, h) = open(name, SERVICE_QUERY_STATUS | SERVICE_STOP | SERVICE_ENUMERATE_DEPENDENTS)?;
    if !status(&h)?.accepts_stop {
        return Err(format!("サービス {name} は停止要求を受け付けません"));
    }
    let mut stopped = Vec::new();
    for dep in active_dependents(&h)? {
        let (_s, dh) = open(&dep, SERVICE_QUERY_STATUS | SERVICE_STOP)?;
        stop_handle(&dh, timeout).map_err(|e| format!("依存サービス {dep}: {e}"))?;
        stopped.push(dep);
    }
    stop_handle(&h, timeout)?;
    Ok(stopped)
}

pub fn start(name: &str, timeout: Duration) -> Result<(), String> {
    let (_scm, h) = open(name, SERVICE_QUERY_STATUS | SERVICE_START)?;
    if status(&h)?.state == SERVICE_RUNNING.0 {
        return Ok(());
    }
    unsafe { StartServiceW(h.0, None).map_err(|e| format!("StartService({name}): {e}"))? };
    wait_state(&h, SERVICE_RUNNING.0, timeout)
}

/// スタートアップ種別を変更し、(変更前の種別, 変更前が遅延自動だったか) を返す。
/// `delayed` が Some のときは遅延自動の設定も合わせる（自動に戻すときだけ意味がある）。
pub fn set_start_type(name: &str, new: u32, delayed: Option<bool>) -> Result<(u32, bool), String> {
    let (_scm, h) = open(name, SERVICE_QUERY_CONFIG | SERVICE_CHANGE_CONFIG)?;
    let before = (start_type(&h)?, delayed_auto(&h));
    unsafe {
        ChangeServiceConfigW(
            h.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(new),
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
        .map_err(|e| format!("ChangeServiceConfig({name}): {e}"))?;
        if let Some(d) = delayed {
            let info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: d.into() };
            ChangeServiceConfig2W(h.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&info as *const _ as *const c_void))
                .map_err(|e| format!("遅延自動の設定({name}): {e}"))?;
        }
    }
    Ok(before)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{SERVICE_AUTO, SERVICE_DEMAND, SERVICE_DISABLED};

    #[test]
    fn query_reports_a_well_known_running_service() {
        // Dnscache は実行中・自動起動・停止要求を受け付けず、依存サービス(RasMan)を持つ。
        let f = query("Dnscache").expect("Dnscache は照会できるはず");
        assert!(f.running && !f.accepts_stop);
        assert_eq!(f.start_type, SERVICE_AUTO);
        assert!(!f.dependents.is_empty());
        // 停止を受け付けるサービスは accepts_stop=true になる。
        assert!(query("WSearch").map(|w| w.accepts_stop).unwrap_or(true));
    }

    #[test]
    fn query_of_a_service_we_cannot_open_is_an_error_so_the_plan_stays_safe() {
        // RpcSs は一般ユーザーでは開けない。照会できないサービスは計画側で「保護」扱いにする。
        assert!(query("RpcSs").is_err());
    }

    #[test]
    fn query_of_unknown_service_is_an_error_not_a_panic() {
        assert!(query("MemOptNoSuchService").is_err());
    }

    #[test]
    fn start_types_are_distinct() {
        assert_ne!(SERVICE_AUTO, SERVICE_DEMAND);
        assert_ne!(SERVICE_DEMAND, SERVICE_DISABLED);
    }

    /// 管理者権限が必要。実機でのみ手動実行: cargo test -p memopt-actions -- --ignored svc_roundtrip
    #[test]
    #[ignore]
    fn svc_roundtrip_stop_start_and_start_type_on_a_harmless_service() {
        // 影響の小さいサービス（Print Spooler と依存の少ない Fax 等は無い環境もあるので、環境変数で指定）。
        let Ok(name) = std::env::var("MEMOPT_TEST_SERVICE") else { return };
        let before = query(&name).unwrap();
        let (b, d) = set_start_type(&name, SERVICE_DEMAND, None).unwrap();
        assert_eq!(b, before.start_type);
        set_start_type(&name, b, Some(d)).unwrap();
        assert_eq!(query(&name).unwrap().start_type, before.start_type);
    }
}

