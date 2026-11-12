//! memopt-tray: 既定は常駐トレイ。`--window` でダッシュボード、`--dump` で機能一覧を標準出力。
mod fmt;
mod graceful;
mod hotkey;
mod icon;
mod service_link;
mod session_info;
mod stop_cmds;
mod stop_ui;
mod tray_mode;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // --dump: GUI なしで機能一覧を標準出力（タスクマネージャーとの突き合わせ用）
    if args.iter().any(|a| a == "--dump") {
        let (m, fs) = window::scan_local();
        println!("{m:?}");
        let total: u64 = fs.iter().map(|f| f.private_bytes).sum();
        for f in &fs {
            println!("{:>10} {:?} {} ({} procs)", fmt::gb(f.private_bytes), f.kind, f.name, f.pids.len());
        }
        println!("TOTAL private = {}", fmt::gb(total));
        return;
    }
    let result = if args.iter().any(|a| a == "--window") {
        window::run().map_err(|e| e.to_string())
    } else {
        tray_mode::run()
    };
    if let Err(e) = result {
        eprintln!("エラー: {e}");
        std::process::exit(1);
    }
}
