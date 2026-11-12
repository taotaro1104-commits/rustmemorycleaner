//! Windows API による計測（読み取り専用・特権不要）。
mod cmdline;
pub use cmdline::extract_exe_path;
#[cfg(windows)]
mod pdh;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod startup;
#[cfg(windows)]
mod system;
#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use startup::startup_entries;
#[cfg(windows)]
pub use win::{all_cpu_times, memory_snapshot, nt_process_table, processes, NtProc};
#[cfg(windows)]
pub use pdh::Pdh;
#[cfg(windows)]
pub use session::{audio_pids, busy_pids, cpu_times, foreground_pid};
#[cfg(windows)]
pub use system::{battery_saver_on, local_time_parts, uptime_secs, LowMemoryWatcher};
