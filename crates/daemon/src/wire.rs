use std::io::{self, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use torrential_core::protocol::{decode_frame, encode_frame, frame_len};

fn encode(message: &impl Serialize) -> io::Result<Vec<u8>> {
    encode_frame(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write(w: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    w.write_all(&encode(message)?)?;
    w.flush()
}

pub async fn write_async(w: &mut (impl AsyncWrite + Unpin), message: &impl Serialize) -> io::Result<()> {
    w.write_all(&encode(message)?).await?;
    w.flush().await
}

pub async fn read_async<T: DeserializeOwned>(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<T>> {
    let mut header = [0; 4];
    if r.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut header[1..]).await?;
    let mut body = vec![0; frame_len(header)?];
    r.read_exact(&mut body).await?;
    decode_frame(&body).map(Some)
}
