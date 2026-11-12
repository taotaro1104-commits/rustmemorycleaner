use crate::security::{identify_client, PipeSecurity};
use crate::{read_line_limited, Caller};
use memopt_core::ipc::{DecodeError, Frame, Msg, MAX_LINE_BYTES, PROTOCOL_VERSION};
use std::future::Future;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::broadcast;

pub type BoxFut = Pin<Box<dyn Future<Output = Msg> + Send>>;

/// 要求(Msg)を受けて応答(Msg)を返す。内容の再検証は必ずハンドラ側（サービス）で行う。
/// `caller` は接続時に OS から取得した身元で、要求の内容とは無関係に信頼できる。
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Msg, caller: &Caller) -> BoxFut;
}

/// 受付エラー後に新しいインスタンスを作り直すまでの待ち時間。
const RETRY_DELAY: Duration = Duration::from_millis(300);

pub struct Server {
    name: String,
    handler: Arc<dyn Handler>,
    notify: broadcast::Sender<Frame>,
    security: Arc<PipeSecurity>,
}

impl Server {
    pub fn new(name: &str, handler: Arc<dyn Handler>) -> std::io::Result<Self> {
        let (notify, _) = broadcast::channel(64);
        Ok(Self { name: name.to_string(), handler, notify, security: Arc::new(PipeSecurity::new()?) })
    }

    /// 全接続中のクライアントへ通知を送る（受信者がいなくてもエラーにしない）。
    pub fn notify(&self, msg: Msg) {
        let _ = self.notify.send(Frame::new(None, msg));
    }

    pub fn notifier(&self) -> broadcast::Sender<Frame> {
        self.notify.clone()
    }

    fn create(&self, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut o = ServerOptions::new();
        o.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: as_raw は self.security が生きている間有効な SECURITY_ATTRIBUTES を指す。
        unsafe { o.create_with_security_attributes_raw(&self.name, self.security.as_raw()) }
    }

    /// 次の待ち受けインスタンスを作る。失敗しても諦めず、`stop` まで間隔を置いて再試行する
    /// （一時的なリソース不足でサービス全体が止まらないようにする）。`stop` で None を返す。
    async fn next_instance(&self, stop: &mut tokio::sync::watch::Receiver<bool>) -> Option<NamedPipeServer> {
        loop {
            match self.create(false) {
                Ok(p) => return Some(p),
                Err(e) => eprintln!("memopt-ipc: パイプのインスタンスを作れません（再試行します）: {e}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(RETRY_DELAY) => {}
                _ = stop.changed() => { if *stop.borrow() { return None; } }
            }
        }
    }

    /// 接続を受け付け続ける。`stop` が true になったら新規受付を止めて戻る。
    /// 起動時（最初のインスタンス作成）の失敗だけがエラー。以降の個別の接続エラーでは止まらない。
    pub async fn run(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) -> std::io::Result<()> {
        let mut pipe = self.create(true)?; // 先着の偽サーバーを避けるため first_pipe_instance
        loop {
            tokio::select! {
                r = pipe.connect() => {
                    match r {
                        Ok(()) => {
                            let conn = pipe;
                            match self.next_instance(&mut stop).await { Some(p) => pipe = p, None => return Ok(()) }
                            let me = self.clone();
                            tokio::spawn(async move { me.serve(conn).await });
                        }
                        Err(e) => {
                            // 壊れたインスタンスは捨て、作り直して続行する。
                            eprintln!("memopt-ipc: 接続の受け付けに失敗（続行します）: {e}");
                            match self.next_instance(&mut stop).await { Some(p) => pipe = p, None => return Ok(()) }
                        }
                    }
                }
                _ = stop.changed() => {
                    if *stop.borrow() { return Ok(()); }
                }
            }
        }
    }

    async fn serve(&self, conn: NamedPipeServer) {
        // 生のハンドル値（分割後も conn の半分が生きている間は有効）。身元の取得に使う。
        let raw = conn.as_raw_handle() as usize;
        let mut caller: Option<Caller> = None;
        let (rd, mut wr) = tokio::io::split(conn);
        let mut rd = BufReader::new(rd);
        let mut rx = self.notify.subscribe();
        loop {
            tokio::select! {
                line = read_line_limited(&mut rd, MAX_LINE_BYTES) => {
                    let line = match line { Ok(Some(l)) => l, _ => return };
                    // ImpersonateNamedPipeClient は「サーバーが 1 回以上読み取った後」でないと使えない。
                    // 最初の要求を読んだ直後に、OS が保証する身元を取る（要求に書かれた自己申告は信用しない）。
                    // この取得は await をまたがない（偽装が別スレッドに漏れない）。
                    let who = caller.get_or_insert_with(|| identify_client(raw as std::os::windows::io::RawHandle)).clone();
                    let reply = match Frame::decode(&line) {
                        Ok(f) => {
                            let id = f.id;
                            Frame::new(id, self.handler.handle(f.msg, &who).await)
                        }
                        Err(DecodeError::VersionMismatch { theirs }) => Frame::new(None, Msg::Error {
                            code: "version_mismatch".into(),
                            message: format!("サービスはプロトコル v{PROTOCOL_VERSION}、クライアントは v{theirs}"),
                        }),
                        Err(e) => Frame::new(None, Msg::Error { code: "bad_request".into(), message: e.to_string() }),
                    };
                    if wr.write_all(reply.encode().as_bytes()).await.is_err() { return; }
                }
                n = rx.recv() => {
                    match n {
                        Ok(f) => if wr.write_all(f.encode().as_bytes()).await.is_err() { return; },
                        Err(broadcast::error::RecvError::Lagged(_)) => {} // 古い通知は捨てる
                        Err(_) => return,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
impl Server {
    /// first_pipe_instance の検証用: 最初のインスタンス作成だけ試す。
    pub(crate) fn run_once_for_test(&self) -> std::io::Result<()> {
        self.create(true).map(|_| ())
    }
}
