//! 名前付きパイプ IPC（設計書2章）。1 行 1 メッセージの JSON。
//! サーバーは「対話ユーザーと Administrators（+SYSTEM）」のみ接続可の DACL を付けて作る。
#[cfg(windows)]
mod client;
#[cfg(windows)]
mod security;
#[cfg(windows)]
mod server;

#[cfg(windows)]
pub use client::{Client, ClientError};
#[cfg(windows)]
pub use server::{BoxFut, Handler, Server};

/// ブロッキングしない 1 行読み取り。上限を超えたら Err(TooLong) 相当で切断する。
#[cfg(windows)]
pub(crate) async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
) -> std::io::Result<Option<String>> {
    use tokio::io::AsyncBufReadExt;
    let mut buf = Vec::new();
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if buf.is_empty() { None } else { Some(String::from_utf8_lossy(&buf).into_owned()) });
        }
        match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&chunk[..i]);
                r.consume(i + 1);
                return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
            }
            None => {
                let n = chunk.len();
                buf.extend_from_slice(chunk);
                r.consume(n);
                if buf.len() > max {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long"));
                }
            }
        }
    }
}
