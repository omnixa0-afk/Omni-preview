/// A bulk transfer pipe carrying [`AttachmentTransferFrameV2`]s in order.
///
/// Deliberately expressed over `AsyncRead`/`AsyncWrite` rather than a QUIC type, so the SAME framing
/// serves both lanes: the legacy carrier's QUIC stream and a mesh bulk stream. Callers hand in the
/// two halves; nothing here knows which transport produced them.
///
/// The pipe is assumed ORDERED and reliable (both lanes are QUIC streams), which is what lets a
/// receiver write straight through to disk instead of holding out-of-order gaps in RAM — the
/// property that makes multi-gigabyte transfers possible at constant memory.
pub struct AttachmentTransferStream {
    send: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    recv: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    /// ONE runtime for the life of the stream. The transfer worker is a plain OS thread, so it needs
    /// an executor to drive the async halves — but building one PER FRAME (as this used to) put a
    /// full runtime setup+teardown between every chunk of every file. Per stream it is paid once.
    runtime: tokio::runtime::Runtime,
}

impl AttachmentTransferStream {
    /// Build the transfer pipe from the two halves of an already-open, ordered stream.
    pub fn from_halves(
        send: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
        recv: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    ) -> OmniResult<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                OmniError::Transport(format!("attachment transfer IO runtime: {error}"))
            })?;
        Ok(Self {
            send,
            recv,
            runtime,
        })
    }

    pub fn write_frame(&mut self, frame: &AttachmentTransferFrameV2) -> OmniResult<()> {
        validate_attachment_transfer_frame_v2(frame)?;
        let frame_len = attachment_stream_frame_len(frame)?;
        if u64::from(frame_len) > ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES {
            return Err(OmniError::Transport(format!(
                "attachment stream frame exceeds maximum size of {} bytes",
                ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES
            )));
        }
        let send = &mut self.send;
        self.runtime
            .block_on(write_attachment_stream_frame(send, frame))
            .map_err(|error| {
                OmniError::Transport(format!("failed to write attachment stream frame: {error}"))
            })
    }

    pub fn read_frame(&mut self) -> OmniResult<Option<AttachmentTransferFrameV2>> {
        let recv = &mut self.recv;
        match self.runtime.block_on(read_attachment_stream_frame(recv)) {
            Ok(frame) => Ok(frame),
            Err(error) => Err(OmniError::Transport(format!(
                "failed to read attachment stream frame: {error}"
            ))),
        }
    }

    /// Finish the sending half: the peer's reader sees clean end-of-stream rather than a reset.
    pub fn close(&mut self) -> OmniResult<()> {
        use tokio::io::AsyncWriteExt as _;
        let send = &mut self.send;
        self.runtime.block_on(send.shutdown()).map_err(|error| {
            OmniError::Transport(format!("failed to close attachment stream: {error}"))
        })
    }
}

/// Write one length-prefixed frame. Generic over the writer so the legacy QUIC lane and the mesh
/// bulk lane share one encoder.
pub(crate) async fn write_attachment_stream_frame<W>(
    send: &mut W,
    frame: &AttachmentTransferFrameV2,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    let frame_len = attachment_stream_frame_len(frame).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    send.write_all(&frame_len.to_le_bytes()).await?;
    attachment_stream_write_frame_body(send, frame).await?;
    Ok(())
}

/// Read one length-prefixed frame. `Ok(None)` = the peer finished the stream cleanly at a frame
/// boundary (end of transfer), which is not an error.
pub(crate) async fn read_attachment_stream_frame<R>(
    recv: &mut R,
) -> std::io::Result<Option<AttachmentTransferFrameV2>>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    use tokio::io::AsyncReadExt as _;
    let mut len_bytes = [0_u8; 4];
    match recv.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let frame_len = u32::from_le_bytes(len_bytes) as usize;
    if frame_len == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "attachment stream frame length must be positive",
        ));
    }
    if frame_len as u64 > ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "attachment stream frame exceeds maximum size of {} bytes",
                ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES
            ),
        ));
    }
    attachment_stream_read_frame_body(recv, frame_len).await.map(Some)
}

fn attachment_stream_frame_len(frame: &AttachmentTransferFrameV2) -> OmniResult<u32> {
    let length = match frame {
        AttachmentTransferFrameV2::Start(start) => attachment_stream_checked_frame_len(&[
            attachment_stream_string_len(&start.transfer_id)?,
            attachment_stream_bytes_len(&start.payload)?,
            std::mem::size_of::<i64>(),
        ])?,
        AttachmentTransferFrameV2::Chunk(chunk) => attachment_stream_checked_frame_len(&[
            attachment_stream_string_len(&chunk.transfer_id)?,
            std::mem::size_of::<u64>(),
            attachment_stream_bytes_len(&chunk.payload)?,
            std::mem::size_of::<i64>(),
        ])?,
        AttachmentTransferFrameV2::Complete(complete) => attachment_stream_checked_frame_len(&[
            attachment_stream_string_len(&complete.transfer_id)?,
            attachment_stream_bytes_len(&complete.payload)?,
            std::mem::size_of::<i64>(),
        ])?,
    };
    u32::try_from(length)
        .map_err(|_| OmniError::Transport("attachment stream frame is too large".to_string()))
}

fn attachment_stream_checked_frame_len(fields: &[usize]) -> OmniResult<usize> {
    let mut length = std::mem::size_of::<u16>() + std::mem::size_of::<u8>();
    for field_len in fields {
        length = length.checked_add(*field_len).ok_or_else(|| {
            OmniError::Transport("attachment stream frame length overflowed".to_string())
        })?;
    }
    if length as u64 > ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES {
        return Err(OmniError::Transport(format!(
            "attachment stream frame exceeds maximum size of {} bytes",
            ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES
        )));
    }
    Ok(length)
}

fn attachment_stream_string_len(value: &str) -> OmniResult<usize> {
    attachment_stream_bytes_len(value.as_bytes())
}

fn attachment_stream_bytes_len(value: &[u8]) -> OmniResult<usize> {
    let _ = u32::try_from(value.len())
        .map_err(|_| OmniError::Transport("attachment stream field is too large".to_string()))?;
    std::mem::size_of::<u32>()
        .checked_add(value.len())
        .ok_or_else(|| {
            OmniError::Transport("attachment stream field length overflowed".to_string())
        })
}

async fn attachment_stream_write_frame_body<W>(
    stream: &mut W,
    frame: &AttachmentTransferFrameV2,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    attachment_stream_write_u16(stream, OMNI_PROTOCOL_VERSION_V1).await?;
    match frame {
        AttachmentTransferFrameV2::Start(start) => {
            attachment_stream_write_u8(stream, 1).await?;
            attachment_stream_write_string(stream, &start.transfer_id).await?;
            attachment_stream_write_bytes(stream, &start.payload).await?;
            attachment_stream_write_i64(stream, start.started_at_unix_ms).await?;
        }
        AttachmentTransferFrameV2::Chunk(chunk) => {
            attachment_stream_write_u8(stream, 2).await?;
            attachment_stream_write_string(stream, &chunk.transfer_id).await?;
            attachment_stream_write_u64(stream, chunk.chunk_index).await?;
            attachment_stream_write_bytes(stream, &chunk.payload).await?;
            attachment_stream_write_i64(stream, chunk.sent_at_unix_ms).await?;
        }
        AttachmentTransferFrameV2::Complete(complete) => {
            attachment_stream_write_u8(stream, 3).await?;
            attachment_stream_write_string(stream, &complete.transfer_id).await?;
            attachment_stream_write_bytes(stream, &complete.payload).await?;
            attachment_stream_write_i64(stream, complete.completed_at_unix_ms).await?;
        }
    }
    Ok(())
}

async fn attachment_stream_write_u8<W>(stream: &mut W, value: u8) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    stream.write_all(&[value]).await
}

async fn attachment_stream_write_u16<W>(stream: &mut W, value: u16) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    stream.write_all(&value.to_le_bytes()).await
}

async fn attachment_stream_write_u32<W>(stream: &mut W, value: u32) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    stream.write_all(&value.to_le_bytes()).await
}

async fn attachment_stream_write_u64<W>(stream: &mut W, value: u64) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    stream.write_all(&value.to_le_bytes()).await
}

async fn attachment_stream_write_i64<W>(stream: &mut W, value: i64) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    stream.write_all(&value.to_le_bytes()).await
}

async fn attachment_stream_write_string<W>(stream: &mut W, value: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    attachment_stream_write_bytes(stream, value.as_bytes()).await
}

async fn attachment_stream_write_bytes<W>(stream: &mut W, value: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    use tokio::io::AsyncWriteExt as _;
    let length = u32::try_from(value.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "attachment stream field is too large",
        )
    })?;
    attachment_stream_write_u32(stream, length).await?;
    stream.write_all(value).await
}

async fn attachment_stream_read_frame_body<R>(
    stream: &mut R,
    frame_len: usize,
) -> std::io::Result<AttachmentTransferFrameV2>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    let mut remaining = frame_len;
    let protocol_version = attachment_stream_read_u16(stream, &mut remaining).await?;
    if protocol_version != OMNI_PROTOCOL_VERSION_V1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported attachment stream frame version '{protocol_version}'"),
        ));
    }
    let tag = attachment_stream_read_u8(stream, &mut remaining).await?;
    let frame = match tag {
        1 => AttachmentTransferFrameV2::Start(AttachmentTransferStartV2 {
            transfer_id: attachment_stream_read_string(stream, &mut remaining).await?,
            payload: attachment_stream_read_bytes(
                stream,
                &mut remaining,
                ATTACHMENT_TRANSFER_START_SIZE_MAXIMUM_BYTES,
            )
            .await?,
            started_at_unix_ms: attachment_stream_read_i64(stream, &mut remaining).await?,
        }),
        2 => AttachmentTransferFrameV2::Chunk(AttachmentTransferChunkV2 {
            transfer_id: attachment_stream_read_string(stream, &mut remaining).await?,
            chunk_index: attachment_stream_read_u64(stream, &mut remaining).await?,
            payload: attachment_stream_read_bytes(
                stream,
                &mut remaining,
                ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES,
            )
            .await?,
            sent_at_unix_ms: attachment_stream_read_i64(stream, &mut remaining).await?,
        }),
        3 => AttachmentTransferFrameV2::Complete(AttachmentTransferCompleteV2 {
            transfer_id: attachment_stream_read_string(stream, &mut remaining).await?,
            payload: attachment_stream_read_bytes(
                stream,
                &mut remaining,
                ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES,
            )
            .await?,
            completed_at_unix_ms: attachment_stream_read_i64(stream, &mut remaining).await?,
        }),
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported attachment transfer frame tag '{other}'"),
            ));
        }
    };
    if remaining != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("attachment stream frame has {remaining} unexpected trailing bytes"),
        ));
    }
    validate_attachment_transfer_frame_v2(&frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(frame)
}

async fn attachment_stream_read_u8<R>(stream: &mut R, remaining: &mut usize) -> std::io::Result<u8>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    Ok(attachment_stream_read_fixed::<1, R>(stream, remaining).await?[0])
}

async fn attachment_stream_read_u16<R>(stream: &mut R, remaining: &mut usize) -> std::io::Result<u16>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    Ok(u16::from_le_bytes(
        attachment_stream_read_fixed::<2, R>(stream, remaining).await?,
    ))
}

async fn attachment_stream_read_u32<R>(stream: &mut R, remaining: &mut usize) -> std::io::Result<u32>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    Ok(u32::from_le_bytes(
        attachment_stream_read_fixed::<4, R>(stream, remaining).await?,
    ))
}

async fn attachment_stream_read_u64<R>(stream: &mut R, remaining: &mut usize) -> std::io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    Ok(u64::from_le_bytes(
        attachment_stream_read_fixed::<8, R>(stream, remaining).await?,
    ))
}

async fn attachment_stream_read_i64<R>(stream: &mut R, remaining: &mut usize) -> std::io::Result<i64>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    Ok(i64::from_le_bytes(
        attachment_stream_read_fixed::<8, R>(stream, remaining).await?,
    ))
}

async fn attachment_stream_read_fixed<const N: usize, R>(
    stream: &mut R,
    remaining: &mut usize,
) -> std::io::Result<[u8; N]>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    use tokio::io::AsyncReadExt as _;
    if *remaining < N {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("attachment stream frame is truncated: expected {N} bytes"),
        ));
    }
    let mut bytes = [0_u8; N];
    stream.read_exact(&mut bytes).await?;
    *remaining -= N;
    Ok(bytes)
}

async fn attachment_stream_read_string<R>(
    stream: &mut R,
    remaining: &mut usize,
) -> std::io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    let bytes = attachment_stream_read_bytes(
        stream,
        remaining,
        ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES as usize,
    )
    .await?;
    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("attachment stream string is not valid UTF-8: {error}"),
        )
    })
}

async fn attachment_stream_read_bytes<R>(
    stream: &mut R,
    remaining: &mut usize,
    maximum_size_bytes: usize,
) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    use tokio::io::AsyncReadExt as _;
    let length = attachment_stream_read_u32(stream, remaining).await? as usize;
    if length > maximum_size_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("attachment stream field exceeds maximum size of {maximum_size_bytes} bytes"),
        ));
    }
    if length > *remaining {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("attachment stream frame is truncated: expected {length} bytes"),
        ));
    }
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes).await?;
    *remaining -= length;
    Ok(bytes)
}
