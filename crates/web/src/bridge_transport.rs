//! One logical transfer per direction. Small messages stay in one block;
//! larger messages spill to unnamed temporary storage, never a whole wire Vec.

use crate::{
    bridge::{BULK_CHUNK_BYTES, Wire},
    bridge_codec,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    io::{BufReader, BufWriter, Cursor, Seek, SeekFrom, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, mpsc, watch},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

// Explicit tags freeze the framing vocabulary independently of enum ordering.
#[derive(wincode::SchemaRead, wincode::SchemaWrite)]
struct Header {
    tag: u8,
    id: u64,
    offset: u64,
}
const BEGIN: u8 = 0;
const DATA: u8 = 1;
const END: u8 = 2;
const HEADER_BYTES: usize = 17;

struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// This writer allocates at most one block before spilling. BufWriter keeps
/// scalar codec writes from becoming one filesystem operation per field.
enum Staged {
    Memory(Vec<u8>),
    File(std::fs::File),
}
struct Stager {
    memory: Vec<u8>,
    file: Option<BufWriter<std::fs::File>>,
    cancelled: Arc<AtomicBool>,
}
impl Write for Stager {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::other("transfer cancelled"));
        }
        if self.file.is_none() && bytes.len() <= BULK_CHUNK_BYTES - self.memory.len() {
            self.memory.extend_from_slice(bytes);
        } else {
            if self.file.is_none() {
                let mut file = BufWriter::with_capacity(BULK_CHUNK_BYTES, tempfile::tempfile()?);
                file.write_all(&self.memory)?;
                self.memory = Vec::new();
                self.file = Some(file);
            }
            // Break up a huge string so cancellation is checked between blocks.
            for block in bytes.chunks(BULK_CHUNK_BYTES) {
                if self.cancelled.load(Ordering::Acquire) {
                    return Err(std::io::Error::other("transfer cancelled"));
                }
                self.file.as_mut().unwrap().write_all(block)?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }
}
impl Stager {
    fn finish(mut self) -> anyhow::Result<Staged> {
        self.flush()?;
        match self.file {
            None => Ok(Staged::Memory(self.memory)),
            Some(file) => {
                let mut file = file.into_inner().map_err(|e| e.into_error())?;
                file.seek(SeekFrom::Start(0))?;
                Ok(Staged::File(file))
            }
        }
    }
}
async fn stage(wire: Wire, stop: &mut watch::Receiver<bool>) -> anyhow::Result<Staged> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = Cancel(cancelled.clone());
    let mut work = tokio::task::spawn_blocking(move || {
        let mut writer = Stager {
            memory: Vec::new(),
            file: None,
            cancelled,
        };
        bridge_codec::encode(&mut writer, &wire)?;
        writer.finish()
    });
    tokio::select! {
        result = &mut work => result?,
        _ = stop.changed() => {
            cancel.0.store(true, Ordering::Release);
            let _ = work.await;
            anyhow::bail!("transfer cancelled")
        }
    }
}

enum Receiving {
    Memory(Vec<u8>),
    File(tokio::fs::File),
}
impl Receiving {
    async fn append(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if let Self::Memory(memory) = self {
            if bytes.len() <= BULK_CHUNK_BYTES - memory.len() {
                memory.extend_from_slice(bytes);
                return Ok(());
            }
            let file = tokio::task::spawn_blocking(tempfile::tempfile).await??;
            let mut file = tokio::fs::File::from_std(file);
            file.write_all(memory).await?;
            *self = Self::File(file);
        }
        if let Self::File(file) = self {
            file.write_all(bytes).await?;
        }
        Ok(())
    }
    async fn finish(self) -> anyhow::Result<Staged> {
        match self {
            Self::Memory(memory) => Ok(Staged::Memory(memory)),
            Self::File(mut file) => {
                file.flush().await?;
                file.seek(SeekFrom::Start(0)).await?;
                Ok(Staged::File(file.into_std().await))
            }
        }
    }
}
async fn decode(
    staged: Staged,
    bytes: u64,
    stop: &mut watch::Receiver<bool>,
) -> anyhow::Result<Wire> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = Cancel(cancelled.clone());
    let mut job = tokio::task::spawn_blocking(move || match staged {
        Staged::Memory(memory) => bridge_codec::decode(Cursor::new(memory), bytes, cancelled),
        Staged::File(file) => bridge_codec::decode(
            BufReader::with_capacity(BULK_CHUNK_BYTES, file),
            bytes,
            cancelled,
        ),
    });
    tokio::select! {
        decoded = &mut job => decoded?,
        _ = stop.changed() => {
            cancel.0.store(true, Ordering::Release);
            let _ = job.await;
            anyhow::bail!("transfer cancelled")
        }
    }
}
fn closed(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
    match error {
        Error::ConnectionClosed
        | Error::AlreadyClosed
        | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        Error::Io(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}

pub(crate) struct Connection {
    pub(crate) outbound: mpsc::Sender<Wire>,
    inbound: mpsc::Receiver<Wire>,
    failures: mpsc::Receiver<anyhow::Error>,
    cancel: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}
impl Connection {
    pub(crate) fn new<S>(socket: WebSocketStream<S>) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = socket.split();
        let (tx, mut outgoing) = mpsc::channel::<Wire>(1);
        let (incoming, rx) = mpsc::channel::<Wire>(1);
        // A separate fault slot avoids deadlock behind a full decoded-message
        // queue. First failure wins; cancellation always reaches both workers.
        let (errors, failures) = mpsc::channel(1);
        let (cancel, stop) = watch::channel(false);
        let flush = Arc::new(Notify::new());
        let writer_flush = flush.clone();
        let mut writer_stop = stop.clone();
        let writer_error = errors.clone();
        let writer_cancel = cancel.clone();
        let writer = tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                let mut id = 0u64;
                loop {
                    let wire = tokio::select! {
                        _ = writer_stop.changed() => return Ok(()),
                        _ = writer_flush.notified() => {
                            tokio::select! { _ = writer_stop.changed() => return Ok(()), sent = sink.flush() => sent? };
                            continue;
                        }
                        wire = outgoing.recv() => match wire { Some(wire) => wire, None => return Ok(()) },
                    };
                    let staged = stage(wire, &mut writer_stop).await?;
                    id = id.checked_add(1).ok_or_else(|| anyhow::anyhow!("transfer ID exhausted"))?;
                    let packet = wincode::serialize(&Header { tag: BEGIN, id, offset: 0 })?;
                    tokio::select! { _ = writer_stop.changed() => return Ok(()), sent = sink.send(Message::Binary(packet.into())) => sent? };
                    let mut offset = 0u64;
                    let (memory, mut file) = match staged {
                        Staged::Memory(memory) => (Some(memory), None),
                        Staged::File(file) => (None, Some(tokio::fs::File::from_std(file))),
                    };
                    let mut block = if file.is_some() { vec![0u8; BULK_CHUNK_BYTES] } else { Vec::new() };
                    loop {
                        let bytes = if let Some(memory) = &memory {
                            if offset != 0 || memory.is_empty() { break; }
                            memory.as_slice()
                        } else {
                            let size = tokio::select! { _ = writer_stop.changed() => return Ok(()), size = file.as_mut().unwrap().read(&mut block) => size? };
                            if size == 0 { break; }
                            &block[..size]
                        };
                        let mut packet = wincode::serialize(&Header { tag: DATA, id, offset })?;
                        packet.extend_from_slice(bytes);
                        tokio::select! { _ = writer_stop.changed() => return Ok(()), sent = sink.send(Message::Binary(packet.into())) => sent? };
                        offset = offset.checked_add(bytes.len() as u64).ok_or_else(|| anyhow::anyhow!("transfer length overflow"))?;
                    }
                    let packet = wincode::serialize(&Header { tag: END, id, offset })?;
                    tokio::select! { _ = writer_stop.changed() => return Ok(()), sent = sink.send(Message::Binary(packet.into())) => sent? };
                }
            }.await;
            if let Err(error) = result
                && !*writer_stop.borrow()
                && !error.downcast_ref().is_some_and(closed)
            {
                let _ = writer_error.try_send(error);
            }
            let _ = writer_cancel.send(true);
        });
        let mut reader_stop = stop;
        let reader_cancel = cancel.clone();
        let reader = tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                let mut transfer: Option<(u64, u64, Receiving)> = None;
                let mut last = 0u64;
                loop {
                    let message = tokio::select! {
                        _ = reader_stop.changed() => return Ok(()),
                        message = stream.next() => match message {
                            Some(Ok(message)) => message,
                            Some(Err(error)) if closed(&error) => return Ok(()),
                            Some(Err(error)) => return Err(error.into()),
                            None => return Ok(()),
                        },
                    };
                    let bytes = match message {
                        Message::Binary(bytes) => bytes,
                        Message::Close(_) => return Ok(()),
                        Message::Ping(_) => { flush.notify_one(); continue; }
                        Message::Pong(_) => continue,
                        _ => anyhow::bail!("nonbinary message after bridge handshake"),
                    };
                    anyhow::ensure!(bytes.len() <= BULK_CHUNK_BYTES + HEADER_BYTES, "transport block exceeds negotiated size");
                    let mut cursor = Cursor::new(bytes.as_ref());
                    let header: Header = wincode::deserialize_from(&mut cursor)?;
                    let data = &bytes[cursor.position() as usize..];
                    match header.tag {
                        BEGIN => {
                            anyhow::ensure!(header.offset == 0 && data.is_empty() && transfer.is_none() && header.id > last, "invalid transfer begin");
                            transfer = Some((header.id, 0, Receiving::Memory(Vec::new())));
                        }
                        DATA => {
                            let Some((current, received, storage)) = transfer.as_mut() else { anyhow::bail!("block without transfer"); };
                            anyhow::ensure!(*current == header.id && *received == header.offset && !data.is_empty() && data.len() <= BULK_CHUNK_BYTES, "invalid transfer offset or block");
                            // append/finish own their filesystem operations until complete;
                            // cancellation is observed between blocks, not by dropping I/O.
                            storage.append(data).await?;
                            if *reader_stop.borrow() { return Ok(()); }
                            *received = received.checked_add(data.len() as u64).ok_or_else(|| anyhow::anyhow!("transfer length overflow"))?;
                        }
                        END => {
                            let Some((current, received, storage)) = transfer.take() else { anyhow::bail!("end without transfer"); };
                            anyhow::ensure!(data.is_empty() && current == header.id && received == header.offset, "incomplete transfer");
                            let staged = storage.finish().await?;
                            let decoded = decode(staged, received, &mut reader_stop).await?;
                            tokio::select! { _ = reader_stop.changed() => return Ok(()), sent = incoming.send(decoded) => { if sent.is_err() { return Ok(()); } } };
                            last = header.id;
                        }
                        _ => anyhow::bail!("unknown transfer tag"),
                    }
                }
            }.await;
            if let Err(error) = result
                && !*reader_stop.borrow()
            {
                let _ = errors.try_send(error);
            }
            let _ = reader_cancel.send(true);
        });
        Self {
            outbound: tx,
            inbound: rx,
            failures,
            cancel,
            tasks: vec![reader, writer],
        }
    }
    pub(crate) async fn send(&self, wire: Wire) -> anyhow::Result<()> {
        self.outbound
            .send(wire)
            .await
            .map_err(|_| anyhow::anyhow!("bridge disconnected"))
    }
    pub(crate) async fn recv(&mut self) -> anyhow::Result<Option<Wire>> {
        tokio::select! {
            biased;
            error = self.failures.recv() => match error {
                Some(error) => Err(error),
                None => Ok(self.inbound.recv().await),
            },
            wire = self.inbound.recv() => Ok(wire),
        }
    }
    pub(crate) async fn shutdown(&mut self) {
        let _ = self.cancel.send(true);
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn pair() -> (Connection, WebSocketStream<tokio::io::DuplexStream>) {
        let (a, b) = tokio::io::duplex(BULK_CHUNK_BYTES * 2);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let client = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        (Connection::new(server), client)
    }
    async fn packet(
        peer: &mut WebSocketStream<tokio::io::DuplexStream>,
        tag: u8,
        offset: u64,
        body: &[u8],
    ) {
        let mut packet = wincode::serialize(&Header { tag, id: 1, offset }).unwrap();
        packet.extend_from_slice(body);
        peer.send(Message::Binary(packet.into())).await.unwrap();
    }
    #[test]
    fn framing_schema_has_a_stable_golden_layout() {
        assert_eq!(
            wincode::serialize(&Header {
                tag: DATA,
                id: 1,
                offset: 2
            })
            .unwrap(),
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]
        );
    }
    #[tokio::test]
    async fn malformed_transfers_never_publish_a_candidate() {
        for case in 0..7 {
            let (mut connection, mut peer) = pair().await;
            packet(&mut peer, BEGIN, 0, &[]).await;
            match case {
                0 => packet(&mut peer, DATA, 1, &[9]).await, // gap
                1 => {
                    packet(&mut peer, DATA, 0, &[9]).await;
                    packet(&mut peer, DATA, 0, &[9]).await;
                } // duplicate
                2 => {
                    packet(&mut peer, DATA, 0, &[9]).await;
                    packet(&mut peer, END, 2, &[]).await;
                } // wrong length
                3 => {
                    packet(&mut peer, DATA, 0, &[9, 0]).await;
                    packet(&mut peer, END, 2, &[]).await;
                } // trailing codec bytes
                4 => packet(&mut peer, BEGIN, 0, &[]).await, // interleaved transfer
                5 => packet(&mut peer, DATA, 0, &vec![0; BULK_CHUNK_BYTES + 1]).await,
                6 => {
                    packet(&mut peer, DATA, 0, &[255]).await;
                    packet(&mut peer, END, 1, &[]).await;
                }
                _ => unreachable!(),
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), connection.recv())
                .await
                .unwrap();
            assert!(result.is_err(), "case {case}: {result:?}");
            connection.shutdown().await;
        }
    }
    #[tokio::test]
    async fn cancellation_joins_a_stalled_writer_and_an_unfinished_spool() {
        let (mut connection, mut peer) = pair().await;
        packet(&mut peer, BEGIN, 0, &[]).await;
        for offset in [0, BULK_CHUNK_BYTES] {
            packet(&mut peer, DATA, offset as u64, &vec![0; BULK_CHUNK_BYTES]).await;
        }
        connection
            .send(Wire::Snapshot {
                snapshot: crate::bridge::Snapshot {
                    sequence: 1,
                    state: serde_json::json!({"text":"x".repeat(5 * 1024 * 1024)}),
                },
            })
            .await
            .unwrap();
        // Peer stops reading the outbound image and never Ends its own
        // transfer. Drop must cancel both tasks, which join codec jobs.
        let tasks = std::mem::take(&mut connection.tasks);
        drop(connection);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            for task in tasks {
                task.await.unwrap();
            }
        })
        .await
        .expect("both workers terminate without peer progress");
    }
    #[tokio::test]
    async fn an_idle_ping_is_flushed_without_a_publication() {
        let (mut connection, mut peer) = pair().await;
        peer.send(Message::Ping(vec![1, 2, 3].into()))
            .await
            .unwrap();
        let pong = tokio::time::timeout(std::time::Duration::from_secs(2), peer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(pong, Message::Pong(vec![1, 2, 3].into()));
        connection.shutdown().await;
    }
}
