//! memopt-service: Windows サービスとして動作（`--console` で前面実行）。
use memopt_core::ipc::PIPE_NAME;
use memopt_service::{acl, app, paths};
use std::ffi::OsString;
use std::time::Duration;
use tokio::sync::watch;
use windows_service::service::*;
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

const SERVICE_NAME: &str = "MemOptService";

define_windows_service!(ffi_service_main, service_main);

fn init_logging(console: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_appender::rolling::{Builder, Rotation};
    let dir = paths::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    // 日次ローテーション、14 日保持（設計書7章）。
    let file = Builder::new().rotation(Rotation::DAILY).filename_prefix("memopt.log").max_log_files(14).build(&dir).ok()?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let b = tracing_subscriber::fmt().with_ansi(false).with_max_level(tracing::Level::INFO);
    if console {
        // コンソール時はファイルと標準出力の両方に出す。
        use tracing_subscriber::fmt::writer::MakeWriterExt;
        let _ = b.with_writer(writer.and(std::io::stdout)).try_init();
    } else {
        let _ = b.with_writer(writer).try_init();
    }
    Some(guard)
}

fn run(stop: watch::Receiver<bool>) -> Result<(), String> {
    let dir = app::default_data_dir();
    if std::env::var_os(paths::DATA_DIR_ENV).is_none() {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = acl::secure_data_dir(&dir) {
            tracing::warn!("データフォルダーの ACL を設定できません: {e}");
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(app::serve(&dir, PIPE_NAME, stop))
}

fn service_main(_args: Vec<OsString>) {
    let _guard = init_logging(false);
    let (tx, rx) = watch::channel(false);
    let handler = move |ev| match ev {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(SERVICE_NAME, handler) else { return };
    let mk = |state, accept, code| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    let _ = status.set_service_status(mk(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, ServiceExitCode::Win32(0)));
    let result = run(rx);
    if let Err(e) = &result {
        tracing::error!("サービスが異常終了: {e}");
    }
    let code = if result.is_ok() { ServiceExitCode::Win32(0) } else { ServiceExitCode::ServiceSpecific(1) };
    let _ = status.set_service_status(mk(ServiceState::Stopped, ServiceControlAccept::empty(), code));
}

fn main() {
    if std::env::args().any(|a| a == "--console") {
        let _guard = init_logging(true);
        let (tx, rx) = watch::channel(false);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(true);
        });
        // run() は別ランタイムを作るため、ctrl-c 監視用ランタイムは別スレッドで駆動する。
        std::thread::spawn(move || rt.block_on(std::future::pending::<()>()));
        if let Err(e) = run(rx) {
            eprintln!("エラー: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        eprintln!("サービスとして起動できません（{e}）。前面で動かすには --console を付けてください。");
        std::process::exit(1);
    }
}
