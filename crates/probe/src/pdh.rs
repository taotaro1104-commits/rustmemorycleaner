//! PDH カウンター（スタンバイ/変更リスト・ハードフォルト・ディスク読み込み）。
use memopt_core::LoadSample;
use windows::core::{w, PCWSTR};
use windows::Win32::System::Performance::*;

const STANDBY: [PCWSTR; 3] = [
    w!(r"\Memory\Standby Cache Reserve Bytes"),
    w!(r"\Memory\Standby Cache Normal Priority Bytes"),
    w!(r"\Memory\Standby Cache Core Bytes"),
];
const MODIFIED: PCWSTR = w!(r"\Memory\Modified Page List Bytes");
const PAGES_INPUT: PCWSTR = w!(r"\Memory\Pages Input/sec");
const DISK_TIME: PCWSTR = w!(r"\PhysicalDisk(_Total)\% Disk Time");
const DISK_READ: PCWSTR = w!(r"\PhysicalDisk(_Total)\Disk Read Bytes/sec");

pub struct Pdh {
    query: isize,
    standby: Vec<isize>,
    modified: Option<isize>,
    pages_input: Option<isize>,
    disk_read: Option<isize>,
    disk_time: Option<isize>,
}

fn add(q: isize, path: PCWSTR) -> Option<isize> {
    let mut c = 0isize;
    // 英語名で追加するのでロケールに依存しない。失敗したカウンターは None（空欄扱い）。
    (unsafe { PdhAddEnglishCounterW(q, path, 0, &mut c) } == 0).then_some(c)
}

fn value(c: isize) -> Option<f64> {
    let mut v = PDH_FMT_COUNTERVALUE::default();
    let r = unsafe { PdhGetFormattedCounterValue(c, PDH_FMT_DOUBLE, None, &mut v) };
    (r == 0 && v.CStatus == 0).then(|| unsafe { v.Anonymous.doubleValue })
}

impl Pdh {
    pub fn new() -> Option<Self> {
        let mut q = 0isize;
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut q) } != 0 {
            return None;
        }
        let me = Self {
            query: q,
            standby: STANDBY.iter().filter_map(|p| add(q, *p)).collect(),
            modified: add(q, MODIFIED),
            pages_input: add(q, PAGES_INPUT),
            disk_read: add(q, DISK_READ),
            disk_time: add(q, DISK_TIME),
        };
        me.collect(); // レート系は 2 回目以降の収集から値が出る
        Some(me)
    }

    pub fn collect(&self) {
        unsafe { PdhCollectQueryData(self.query) };
    }

    /// スタンバイ合計（全優先度）。カウンターが 1 つも取れなければ None。
    pub fn standby_bytes(&self) -> Option<u64> {
        let vals: Vec<f64> = self.standby.iter().filter_map(|c| value(*c)).collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f64>() as u64)
    }

    pub fn modified_bytes(&self) -> Option<u64> {
        self.modified.and_then(value).map(|v| v as u64)
    }

    /// ディスク使用率（0〜100 にクランプ。% Disk Time は 100 を超えることがある）。
    pub fn disk_busy_pct(&self) -> Option<f64> {
        self.disk_time.and_then(value).map(|v| v.clamp(0.0, 100.0))
    }

    /// 直近の collect 間隔でのハードフォルト率とディスク読み込み量。
    pub fn load(&self) -> Option<LoadSample> {
        Some(LoadSample {
            pages_input_per_sec: self.pages_input.and_then(value)?,
            disk_read_bytes_per_sec: self.disk_read.and_then(value).unwrap_or(0.0),
        })
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_readable() {
        let p = Pdh::new().expect("pdh");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        p.collect();
        assert!(p.standby_bytes().is_some_and(|b| b > 0));
        assert!(p.modified_bytes().is_some());
        let l = p.load().expect("load");
        assert!(l.pages_input_per_sec >= 0.0);
    }
}
