//! 解放処理 L1〜L3 と特権の有効化（設計書3章）。Windows 依存の呼び出しはここに集約。
//! NtSetSystemInformation は `memlist.rs` の 1 か所だけで呼ぶ。
mod report;
pub use report::{CleanReport, Failure};
#[cfg(windows)]
mod memlist;
#[cfg(windows)]
mod priv_token;
#[cfg(windows)]
mod run;
#[cfg(windows)]
pub use priv_token::{enable_privilege, is_elevated, Privilege};
#[cfg(windows)]
pub use run::{availability, run_l1, run_level, Availability};
