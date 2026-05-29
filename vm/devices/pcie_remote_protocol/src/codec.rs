// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Length-prefixed framing codec on top of `futures::io`. Each frame:
//! 4-byte little-endian length + protobuf payload.

use crate::MAX_FRAME_BYTES;
use futures::io::AsyncRead;
use futures::io::AsyncReadExt;
use futures::io::AsyncWrite;
use futures::io::AsyncWriteExt;
use prost::Message;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {0} exceeds limit")]
    FrameTooLarge(u32),
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
}

pub async fn read_frame<R: AsyncRead + Unpin, M: Message + Default>(
    reader: &mut R,
) -> Result<M, CodecError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    if len as usize > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(M::decode(buf.as_slice())?)
}

pub async fn write_frame<W: AsyncWrite + Unpin, M: Message>(
    writer: &mut W,
    msg: &M,
) -> Result<(), CodecError> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf)?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(buf.len() as u32));
    }
    writer.write_all(&(buf.len() as u32).to_le_bytes()).await?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hello;
    use futures::io::Cursor;
    use pal_async::async_test;

    #[async_test]
    async fn roundtrip_hello() {
        let msg = Hello {
            magic: 0x52504345,
            version: 1,
            instance_id: vec![0xab; 16],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        let mut cur = Cursor::new(buf);
        let got: Hello = read_frame(&mut cur).await.unwrap();
        assert_eq!(got.magic, msg.magic);
        assert_eq!(got.version, msg.version);
        assert_eq!(got.instance_id, msg.instance_id);
    }

    #[async_test]
    async fn frame_too_large_rejected() {
        let oversized = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        let mut cur = Cursor::new(oversized.to_vec());
        let r: Result<Hello, _> = read_frame(&mut cur).await;
        assert!(matches!(r, Err(CodecError::FrameTooLarge(_))));
    }

    #[async_test]
    async fn truncated_payload_returns_io_error() {
        let mut buf = 16u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        let mut cur = Cursor::new(buf);
        let r: Result<Hello, _> = read_frame(&mut cur).await;
        assert!(matches!(r, Err(CodecError::Io(_))));
    }
}
