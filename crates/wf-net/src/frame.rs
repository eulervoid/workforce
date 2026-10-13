//! Length-prefixed postcard frames.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wf_core::encoding::{self, EncodingError};

/// Larger frames are refused before they are read.
pub const MAX_FRAME_LEN: u32 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("connection failed")]
    Io(#[from] std::io::Error),
    #[error("message does not decode")]
    Encoding(#[from] EncodingError),
    #[error("frame of {0} bytes is too large")]
    TooLarge(u64),
    #[error("the peer closed the stream")]
    Closed,
}

pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_encoded_frame(writer, &encoding::encode(message)?).await
}

/// Writes a message that is already encoded.
pub async fn write_encoded_frame<W>(writer: &mut W, bytes: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let length = u32::try_from(bytes.len())
        .ok()
        .filter(|length| *length <= MAX_FRAME_LEN)
        .ok_or(FrameError::TooLarge(bytes.len() as u64))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::Closed);
        }
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(length);
    if length > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(u64::from(length)));
    }
    let mut bytes = vec![0u8; length as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(encoding::decode(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_oversized_frames_are_refused() {
        let (mut left, mut right) = tokio::io::duplex(1024);
        write_frame(&mut left, &vec![1u32, 2, 3]).await.unwrap();
        let read: Vec<u32> = read_frame(&mut right).await.unwrap();
        assert_eq!(read, [1, 2, 3]);

        left.write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .await
            .unwrap();
        let result: Result<Vec<u32>, _> = read_frame(&mut right).await;
        assert!(matches!(result, Err(FrameError::TooLarge(_))));

        drop(left);
        let result: Result<Vec<u32>, _> = read_frame(&mut right).await;
        assert!(matches!(result, Err(FrameError::Closed)));
    }

    #[tokio::test]
    async fn a_message_that_is_too_large_is_refused_before_anything_is_written() {
        let mut written = Vec::new();
        let message = vec![7u8; MAX_FRAME_LEN as usize + 1];
        let result = write_frame(&mut written, &message).await;
        assert!(matches!(result, Err(FrameError::TooLarge(_))));
        assert!(written.is_empty());
    }
}
