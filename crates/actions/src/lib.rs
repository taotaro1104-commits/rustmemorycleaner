//! 機能の停止・無効化・復元（設計書5章）。判定は純粋ロジック、Windows 呼び出しは薄いラッパーに分離。
pub mod journal;
pub mod plan;
pub mod proc;
#[cfg(windows)]
pub mod exec;
#[cfg(windows)]
pub mod startup;
#[cfg(windows)]
pub mod svc;
#[cfg(windows)]
pub mod task;
