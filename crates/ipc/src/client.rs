use crate::read_line_limited;
use memopt_core::ipc::{DecodeError, Frame, Msg, MAX_LINE_BYTES};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Decode(DecodeError),
    Timeout,
    Closed,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Decode(e) => write!(f, "{e}"),
            Self::Timeout => write!(f, "応答がありません"),
            Self::Closed => write!(f, "サービスとの接続が切れました"),
        }
    }
}
impl std::error::Error for ClientError {}

pub struct Client {
    rd: BufReader<ReadHalf<NamedPipeClient>>,
    wr: WriteHalf<NamedPipeClient>,
    next_id: u64,
    /// 応答待ちの間に届いた通知。
    pub notifications: VecDeque<Msg>,
}

impl Client {
    /// サービスが居ない/起動中のときは Err（GUI は読み取り専用の監視モードに落ちる）。
    pub fn connect(name: &str) -> Result<Self, ClientError> {
        let c = ClientOptions::new().open(name).map_err(ClientError::Io)?;
        let (rd, wr) = tokio::io::split(c);
        Ok(Self { rd: BufReader::new(rd), wr, next_id: 1, notifications: VecDeque::new() })
    }

    /// 要求を送り、同じ id の応答を待つ。通知は `notifications` に積む。
    pub async fn call(&mut self, msg: Msg, timeout: Duration) -> Result<Msg, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        self.wr.write_all(Frame::new(Some(id), msg).encode().as_bytes()).await.map_err(ClientError::Io)?;
        tokio::time::timeout(timeout, async {
            loop {
                let line = read_line_limited(&mut self.rd, MAX_LINE_BYTES)
                    .await
                    .map_err(ClientError::Io)?
                    .ok_or(ClientError::Closed)?;
                let f = Frame::decode(&line).map_err(ClientError::Decode)?;
                match f.id {
                    Some(i) if i == id => return Ok(f.msg),
                    None if matches!(f.msg, Msg::Error { .. }) => return Ok(f.msg), // 版ずれ等、id なしのエラー
                    None => self.notifications.push_back(f.msg),
                    Some(_) => {} // 古い応答は無視
                }
            }
        })
        .await
        .map_err(|_| ClientError::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use crate::server::{BoxFut, Handler, Server};
    use crate::Client;
    use memopt_core::ipc::{Msg, SessionInfo};
    use memopt_core::Level;
    use std::sync::Arc;
    use std::time::Duration;

    struct Echo;
    impl Handler for Echo {
        fn handle(&self, req: Msg, _caller: &crate::Caller) -> BoxFut {
            Box::pin(async move {
                match req {
                    Msg::RunClean { level } if level == Level::L3 => {
                        Msg::Error { code: "denied".into(), message: "L3 は手動のみ".into() }
                    }
                    Msg::ReportSession(_) => Msg::Ack,
                    _ => Msg::Ack,
                }
            })
        }
    }

    fn pipe_name(tag: &str) -> String {
        format!(r"\\.\pipe\memopt-test-{}-{}", tag, std::process::id())
    }

    async fn start(tag: &str) -> (Arc<Server>, tokio::sync::watch::Sender<bool>, String) {
        let name = pipe_name(tag);
        let srv = Arc::new(Server::new(&name, Arc::new(Echo)).unwrap());
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(srv.clone().run(rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        (srv, tx, name)
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let (_srv, stop, name) = start("rt").await;
        let mut c = Client::connect(&name).unwrap();
        let r = c.call(Msg::ReportSession(SessionInfo::default()), Duration::from_secs(2)).await.unwrap();
        assert_eq!(r, Msg::Ack);
        let r = c.call(Msg::RunClean { level: Level::L3 }, Duration::from_secs(2)).await.unwrap();
        assert!(matches!(r, Msg::Error { ref code, .. } if code == "denied"));
        let _ = stop.send(true);
    }

    #[tokio::test]
    async fn notifications_reach_connected_clients_without_blocking_calls() {
        let (srv, stop, name) = start("nt").await;
        let mut c = Client::connect(&name).unwrap();
        c.call(Msg::GetSnapshot, Duration::from_secs(2)).await.unwrap();
        srv.notify(Msg::PressureChanged { active: true });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // 次の呼び出しの応答待ち中に通知が積まれる
        let r = c.call(Msg::GetSnapshot, Duration::from_secs(2)).await.unwrap();
        assert_eq!(r, Msg::Ack);
        assert!(c.notifications.contains(&Msg::PressureChanged { active: true }));
        let _ = stop.send(true);
    }

    #[tokio::test]
    async fn bad_json_gets_error_and_connection_survives() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::windows::named_pipe::ClientOptions;
        let (_srv, stop, name) = start("bad").await;
        let raw = ClientOptions::new().open(&name).unwrap();
        let (r, mut w) = tokio::io::split(raw);
        let mut r = BufReader::new(r);
        w.write_all(b"this is not json\n").await.unwrap();
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert!(line.contains("bad_request"), "{line}");
        w.write_all(b"{\"protocol_version\":99,\"type\":\"GetSnapshot\"}\n").await.unwrap();
        line.clear();
        r.read_line(&mut line).await.unwrap();
        assert!(line.contains("version_mismatch"), "{line}");
        let _ = stop.send(true);
    }

    #[tokio::test]
    async fn connecting_without_server_fails_fast() {
        assert!(Client::connect(&pipe_name("none")).is_err());
    }

    #[tokio::test]
    async fn second_server_with_same_name_cannot_take_over() {
        let (_srv, stop, name) = start("dup").await;
        // first_pipe_instance(true) により、同名の偽サーバーは作れない。
        assert!(Server::new(&name, Arc::new(Echo)).unwrap().run_once_for_test().is_err());
        let _ = stop.send(true);
    }

    /// 呼び出し元の身元はサーバーが OS から取る。同じユーザーのクライアントなら、自分の SID と管理者かどうかが一致する。
    struct WhoAmI(std::sync::Mutex<Option<crate::Caller>>);
    impl Handler for WhoAmI {
        fn handle(&self, _req: Msg, caller: &crate::Caller) -> BoxFut {
            *self.0.lock().unwrap() = Some(caller.clone());
            Box::pin(async { Msg::Ack })
        }
    }

    #[tokio::test]
    async fn server_identifies_the_client_from_the_os_token() {
        let name = pipe_name("who");
        let h = Arc::new(WhoAmI(Default::default()));
        let srv = Arc::new(Server::new(&name, h.clone()).unwrap());
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(srv.clone().run(rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut c = Client::connect(&name).unwrap();
        c.call(Msg::GetSnapshot, Duration::from_secs(2)).await.unwrap();
        let who = h.0.lock().unwrap().clone().expect("handler が呼ばれた");
        assert_eq!(who.user_sid, crate::current_process_user_sid(), "SID が自分のものと一致する");
        assert!(who.user_sid.as_deref().is_some_and(|s| s.starts_with("S-1-")));
        // 管理者かどうかは、このテストプロセスの昇格状態と一致する（昇格していなければ false）。
        assert_eq!(who.is_admin, is_process_elevated());
        let _ = tx.send(true);
    }

    fn is_process_elevated() -> bool {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut t = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t).is_err() {
                return false;
            }
            let mut e = TOKEN_ELEVATION::default();
            let mut n = 0u32;
            let ok = GetTokenInformation(t, TokenElevation, Some(&mut e as *mut _ as *mut _), size_of::<TOKEN_ELEVATION>() as u32, &mut n).is_ok();
            let _ = CloseHandle(t);
            ok && e.TokenIsElevated != 0
        }
    }

    #[tokio::test]
    async fn a_bad_client_does_not_stop_the_server_from_accepting_the_next_one() {
        let (_srv, stop, name) = start("resil").await;
        // 接続してすぐ切る、を繰り返しても、次の接続は受け付けられる。
        // （サーバーが次のインスタンスを作るまでの一瞬は PIPE_BUSY になるので、クライアント側は再試行する。実際のトレイも同様。）
        async fn connect_retry(name: &str) -> Client {
            for _ in 0..50 {
                if let Ok(c) = Client::connect(name) {
                    return c;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("接続できない: {name}");
        }
        for _ in 0..5 {
            drop(connect_retry(&name).await);
        }
        let mut ok = connect_retry(&name).await;
        assert_eq!(ok.call(Msg::GetSnapshot, Duration::from_secs(2)).await.unwrap(), Msg::Ack);
        let _ = stop.send(true);
    }
}
