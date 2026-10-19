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

/// The length prefix of a frame of `length` bytes.
fn length_prefix(length: usize) -> Result<[u8; 4], FrameError> {
    u32::try_from(length)
        .ok()
        .filter(|length| *length <= MAX_FRAME_LEN)
        .map(u32::to_be_bytes)
        .ok_or(FrameError::TooLarge(length as u64))
}

/// How many bytes follow a length prefix.
fn body_length(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let length = u32::from_be_bytes(prefix);
    if length > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(u64::from(length)));
    }
    Ok(length as usize)
}

/// A stream ending where a length prefix should start is the peer closing it.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "`std::io::ErrorKind` is non-exhaustive; only an unexpected end of file means a closed stream"
)]
fn closed_at_prefix(error: std::io::Error) -> FrameError {
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof => FrameError::Closed,
        _ => error.into(),
    }
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
    writer.write_all(&length_prefix(bytes.len())?).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(closed_at_prefix)?;
    let mut bytes = vec![0u8; body_length(prefix)?];
    reader.read_exact(&mut bytes).await?;
    Ok(encoding::decode(&bytes)?)
}

/// The same frames over blocking streams, for the CLI's socket to the daemon.
pub mod blocking {
    use std::io::{Read, Write};

    use super::{
        DeserializeOwned, FrameError, Serialize, body_length, closed_at_prefix, encoding,
        length_prefix,
    };

    pub fn write_frame<W: Write, T: Serialize>(
        writer: &mut W,
        message: &T,
    ) -> Result<(), FrameError> {
        let bytes = encoding::encode(message)?;
        writer.write_all(&length_prefix(bytes.len())?)?;
        writer.write_all(&bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, FrameError> {
        let mut prefix = [0u8; 4];
        reader.read_exact(&mut prefix).map_err(closed_at_prefix)?;
        let mut bytes = vec![0u8; body_length(prefix)?];
        reader.read_exact(&mut bytes)?;
        Ok(encoding::decode(&bytes)?)
    }
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

    #[test]
    fn blocking_frames_are_the_same_bytes_as_async_ones() {
        let mut written = Vec::new();
        blocking::write_frame(&mut written, &vec![1u32, 2, 3]).unwrap();
        let read: Vec<u32> = blocking::read_frame(&mut written.as_slice()).unwrap();
        assert_eq!(read, [1, 2, 3]);

        let mut async_written = Vec::new();
        written_by_async(&mut async_written);
        assert_eq!(written, async_written);

        let result: Result<Vec<u32>, _> = blocking::read_frame(&mut [].as_slice());
        assert!(matches!(result, Err(FrameError::Closed)));
        let oversized = (MAX_FRAME_LEN + 1).to_be_bytes();
        let result: Result<Vec<u32>, _> = blocking::read_frame(&mut oversized.as_slice());
        assert!(matches!(result, Err(FrameError::TooLarge(_))));
    }

    fn written_by_async(written: &mut Vec<u8>) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(write_frame(written, &vec![1u32, 2, 3]))
            .unwrap();
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
