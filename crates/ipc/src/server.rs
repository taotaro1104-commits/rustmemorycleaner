use crate::read_line_limited;
use crate::security::PipeSecurity;
use memopt_core::ipc::{DecodeError, Frame, Msg, MAX_LINE_BYTES, PROTOCOL_VERSION};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::broadcast;

pub type BoxFut = Pin<Box<dyn Future<Output = Msg> + Send>>;

/// 要求(Msg)を受けて応答(Msg)を返す。内容の再検証は必ずハンドラ側（サービス）で行う。
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Msg) -> BoxFut;
}

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

    /// 接続を受け付け続ける。`stop` が true になったら新規受付を止めて戻る。
    pub async fn run(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) -> std::io::Result<()> {
        let mut pipe = self.create(true)?; // 先着の偽サーバーを避けるため first_pipe_instance
        loop {
            tokio::select! {
                r = pipe.connect() => {
                    r?;
                    let conn = pipe;
                    pipe = self.create(false)?;
                    let me = self.clone();
                    tokio::spawn(async move { me.serve(conn).await });
                }
                _ = stop.changed() => {
                    if *stop.borrow() { return Ok(()); }
                }
            }
        }
    }

    async fn serve(&self, conn: NamedPipeServer) {
        let (rd, mut wr) = tokio::io::split(conn);
        let mut rd = BufReader::new(rd);
        let mut rx = self.notify.subscribe();
        loop {
            tokio::select! {
                line = read_line_limited(&mut rd, MAX_LINE_BYTES) => {
                    let line = match line { Ok(Some(l)) => l, _ => return };
                    let reply = match Frame::decode(&line) {
                        Ok(f) => {
                            let id = f.id;
                            Frame::new(id, self.handler.handle(f.msg).await)
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
