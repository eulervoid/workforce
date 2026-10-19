//! Length-prefixed postcard frames.

use std::time::Duration;

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
    #[error("the peer stopped answering")]
    TimedOut,
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
fn body_length(prefix: [u8; 4], max_len: u32) -> Result<usize, FrameError> {
    let length = u32::from_be_bytes(prefix);
    if length > max_len {
        return Err(FrameError::TooLarge(u64::from(length)));
    }
    Ok(length as usize)
}

/// Frames are read and written in pieces of this size, so a deadline can be set on each piece.
const CHUNK_LEN: usize = 64 * 1024;

/// A deadline that never expires, for streams where waiting is normal.
const NO_DEADLINE: Duration = Duration::MAX;

/// A stream ending where a length prefix should start is the peer closing it. One ending inside
/// the prefix is a connection that broke.
fn closed_at_prefix(first_read: usize) -> Result<(), FrameError> {
    match first_read {
        0 => Err(FrameError::Closed),
        _ => Ok(()),
    }
}

/// Fails with [`FrameError::TimedOut`] when `operation` does not finish within `idle`.
async fn within<T>(
    idle: Duration,
    operation: impl Future<Output = std::io::Result<T>>,
) -> Result<T, FrameError> {
    tokio::time::timeout(idle, operation)
        .await
        .map_err(|_| FrameError::TimedOut)?
        .map_err(FrameError::from)
}

pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_frame_within(writer, message, NO_DEADLINE).await
}

/// Writes a message that is already encoded.
pub async fn write_encoded_frame<W>(writer: &mut W, bytes: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    write_encoded_frame_within(writer, bytes, NO_DEADLINE).await
}

/// Like [`write_frame`], failing when the peer takes `idle` to accept any piece of the frame.
pub async fn write_frame_within<W, T>(
    writer: &mut W,
    message: &T,
    idle: Duration,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_encoded_frame_within(writer, &encoding::encode(message)?, idle).await
}

async fn write_encoded_frame_within<W>(
    writer: &mut W,
    bytes: &[u8],
    idle: Duration,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    within(idle, writer.write_all(&length_prefix(bytes.len())?)).await?;
    for chunk in bytes.chunks(CHUNK_LEN) {
        within(idle, writer.write_all(chunk)).await?;
    }
    within(idle, writer.flush()).await
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_frame_within(reader, MAX_FRAME_LEN, NO_DEADLINE).await
}

/// Like [`read_frame`], refusing frames over `max_len` bytes and failing when the peer sends
/// nothing for `idle` while a frame is due or half-read. A slow peer that keeps sending is not
/// idle.
pub async fn read_frame_within<R, T>(
    reader: &mut R,
    max_len: u32,
    idle: Duration,
) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    closed_at_prefix(within(idle, reader.read(&mut prefix[..1])).await?)?;
    within(idle, reader.read_exact(&mut prefix[1..])).await?;
    let length = body_length(prefix, max_len)?;
    // Grown as the bytes arrive, so a peer cannot make us reserve what it never sends.
    let mut bytes = Vec::new();
    while bytes.len() < length {
        let start = bytes.len();
        bytes.resize(length.min(start + CHUNK_LEN), 0);
        within(idle, reader.read_exact(&mut bytes[start..])).await?;
    }
    Ok(encoding::decode(&bytes)?)
}

/// The same frames over blocking streams, for the CLI's socket to the daemon.
pub mod blocking {
    use std::io::{Read, Write};

    use super::{
        DeserializeOwned, FrameError, MAX_FRAME_LEN, Serialize, body_length, closed_at_prefix,
        encoding, length_prefix,
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
        closed_at_prefix(reader.read(&mut prefix[..1])?)?;
        reader.read_exact(&mut prefix[1..])?;
        let mut bytes = vec![0u8; body_length(prefix, MAX_FRAME_LEN)?];
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
        let result: Result<Vec<u32>, _> = blocking::read_frame(&mut [0u8, 0].as_slice());
        assert!(matches!(result, Err(FrameError::Io(_))));
        let oversized = (MAX_FRAME_LEN + 1).to_be_bytes();
        let result: Result<Vec<u32>, _> = blocking::read_frame(&mut oversized.as_slice());
        assert!(matches!(result, Err(FrameError::TooLarge(_))));
    }

    fn written_by_async(written: &mut Vec<u8>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
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

    #[tokio::test]
    async fn a_stream_ending_inside_the_length_prefix_is_an_error_not_a_close() {
        let (mut left, mut right) = tokio::io::duplex(1024);
        left.write_all(&[0, 0]).await.unwrap();
        drop(left);
        let result: Result<Vec<u32>, _> = read_frame(&mut right).await;
        assert!(matches!(result, Err(FrameError::Io(_))), "{result:?}");
    }

    #[tokio::test]
    async fn frames_over_the_limit_given_are_refused() {
        let (mut left, mut right) = tokio::io::duplex(1024);
        write_frame(&mut left, &vec![7u8; 100]).await.unwrap();
        let result: Result<Vec<u8>, _> =
            read_frame_within(&mut right, 50, Duration::from_secs(1)).await;
        assert!(matches!(result, Err(FrameError::TooLarge(_))), "{result:?}");
    }

    #[tokio::test]
    async fn the_deadline_is_for_each_piece_of_a_frame_not_the_whole() {
        let idle = Duration::from_millis(200);
        let message = vec![7u8; 3 * CHUNK_LEN];
        let bytes = encoding::encode(&message).unwrap();
        let (mut left, mut right) = tokio::io::duplex(2 * CHUNK_LEN);
        let slow_writer = tokio::spawn(async move {
            left.write_all(&(bytes.len() as u32).to_be_bytes())
                .await
                .unwrap();
            for chunk in bytes.chunks(CHUNK_LEN) {
                left.write_all(chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            left
        });
        let read: Vec<u8> = read_frame_within(&mut right, MAX_FRAME_LEN, idle)
            .await
            .unwrap();
        assert_eq!(read, message);
        let mut left = slow_writer.await.unwrap();

        // Half a frame, then nothing.
        left.write_all(&100u32.to_be_bytes()).await.unwrap();
        left.write_all(&[0; 10]).await.unwrap();
        let result: Result<Vec<u8>, _> = read_frame_within(&mut right, MAX_FRAME_LEN, idle).await;
        assert!(matches!(result, Err(FrameError::TimedOut)), "{result:?}");
    }

    #[tokio::test]
    async fn a_write_to_a_peer_that_reads_nothing_times_out() {
        let (mut left, _unread) = tokio::io::duplex(1024);
        let result =
            write_frame_within(&mut left, &vec![7u8; 10_000], Duration::from_millis(100)).await;
        assert!(matches!(result, Err(FrameError::TimedOut)), "{result:?}");
    }
}
