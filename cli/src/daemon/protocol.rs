//! Length-prefixed JSON frame codec for the daemon control socket. Each frame is
//! a 4-byte big-endian length followed by that many bytes of JSON. Shared by the
//! daemon host and the CLI client.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Write one length-prefixed (u32 BE) JSON frame and flush.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: serde::Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let len = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

/// Read one frame; `Ok(None)` on clean EOF before any bytes of a new frame.
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf).map_err(std::io::Error::other)?;
    Ok(Some(value))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use models::daemon::{DaemonResponse, SubmittedResponse};

    #[tokio::test]
    async fn frame_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let msg = DaemonResponse::Submitted(SubmittedResponse { job_id: "x".into() });
        write_frame(&mut a, &msg).await.unwrap();
        let got: DaemonResponse = read_frame(&mut b).await.unwrap().unwrap();
        match got {
            DaemonResponse::Submitted(r) => assert_eq!(r.job_id, "x"),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        let got: Option<DaemonResponse> = read_frame(&mut b).await.unwrap();
        assert!(got.is_none());
    }
}
