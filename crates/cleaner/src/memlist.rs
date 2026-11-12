//! NtSetSystemInformation(SystemMemoryListInformation) ラッパー
//! （文書化が限定的なためここ 1 か所に集約。windows クレートに束縛が無いので自前宣言）。
use std::ffi::c_void;

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtSetSystemInformation(class: i32, info: *mut c_void, len: u32) -> i32;
}

const SYSTEM_MEMORY_LIST_INFORMATION: i32 = 80;

#[derive(Debug, Clone, Copy)]
#[repr(i32)]
pub enum MemoryListCommand {
    FlushModifiedList = 3,
    PurgeStandbyList = 4,
    PurgeLowPriorityStandbyList = 5,
}

/// 戻り値は NTSTATUS（0 以上で成功）。
pub fn memory_list(cmd: MemoryListCommand) -> i32 {
    let mut c: i32 = cmd as i32;
    unsafe { NtSetSystemInformation(SYSTEM_MEMORY_LIST_INFORMATION, &mut c as *mut _ as *mut c_void, size_of::<i32>() as u32) }
}
