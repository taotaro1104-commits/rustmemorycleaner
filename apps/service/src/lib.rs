//! memopt-service: 常駐サービス本体（監視・トリガー判定・解放・IPC サーバー）。
pub mod acl;
pub mod app;
pub mod config_io;
pub mod evalloop;
pub mod handler;
pub mod handler_stop;
pub mod loadring;
pub mod paths;
pub mod runner;
pub mod stopsvc;
pub mod sync;
pub mod state;
