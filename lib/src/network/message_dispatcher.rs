//! Utilities for sending and receiving messages across the network.

use super::{
    connection::{ConnectionPermit, ConnectionPermitHalf},
    message::Message,
    message_io::{MessageSink, MessageStream},
};
use crate::repository::PublicRepositoryId;
use futures_util::{ready, stream::SelectAll, Sink, SinkExt, Stream, StreamExt};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};
use tokio::{
    net::{tcp, TcpStream},
    select,
    sync::watch,
};

/// Reads/writes messages from/to the underlying TCP streams and dispatches them to individual
/// streams/sinks based on their ids.
pub(super) struct MessageDispatcher {
    recv: Arc<RecvState>,
    send: Arc<MultiSink>,
}

impl MessageDispatcher {
    pub fn new() -> Self {
        let (queues_changed_tx, _) = watch::channel(());

        Self {
            recv: Arc::new(RecvState {
                reader: MultiStream::new(),
                queues: Mutex::new(HashMap::default()),
                queues_changed_tx,
            }),
            send: Arc::new(MultiSink::new()),
        }
    }

    /// Bind this dispatcher to the given TCP socket. Can be bound to multiple sockets and the
    /// failed ones are automatically removed.
    pub fn bind(&self, stream: TcpStream, permit: ConnectionPermit) {
        let (reader, writer) = stream.into_split();
        let (reader_permit, writer_permit) = permit.split();

        self.recv
            .reader
            .add(PermittedStream::new(reader, reader_permit));
        self.send.add(PermittedSink::new(writer, writer_permit));
    }

    /// Opens a stream for receiving messages with the given id.
    pub fn open_recv(&self, id: PublicRepositoryId) -> ContentStream {
        ContentStream::new(id, self.recv.clone())
    }

    /// Opens a sink for sending messages with the given id.
    pub fn open_send(&self, id: PublicRepositoryId) -> ContentSink {
        ContentSink {
            id,
            state: self.send.clone(),
        }
    }
}

impl Drop for MessageDispatcher {
    fn drop(&mut self) {
        self.recv.reader.close();
        self.send.close();
    }
}

pub(super) struct ContentStream {
    id: PublicRepositoryId,
    state: Arc<RecvState>,
    queues_changed_rx: watch::Receiver<()>,
}

impl ContentStream {
    fn new(id: PublicRepositoryId, state: Arc<RecvState>) -> Self {
        let queues_changed_rx = state.queues_changed_tx.subscribe();

        Self {
            id,
            state,
            queues_changed_rx,
        }
    }

    pub fn id(&self) -> &PublicRepositoryId {
        &self.id
    }

    /// Receive the next message content.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        let mut closed = false;

        loop {
            if let Some(content) = self.state.pop(&self.id) {
                return Some(content);
            }

            if closed {
                return None;
            }

            select! {
                message = self.state.reader.recv() => {
                    if let Some(message) = message {
                        if message.id == self.id {
                            return Some(message.content);
                        } else {
                            self.state.push(message)
                        }
                    } else {
                        // If the reader closed we still want to check the queues one more time
                        // because other streams might have pushed a message in the meantime.
                        closed = true;
                    }
                }
                _ = self.queues_changed_rx.changed() => ()
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct ContentSink {
    id: PublicRepositoryId,
    state: Arc<MultiSink>,
}

impl ContentSink {
    /// Returns whether the send succeeded.
    pub async fn send(&self, content: Vec<u8>) -> bool {
        self.state
            .send(Message {
                id: self.id,
                content,
            })
            .await
    }
}

struct RecvState {
    reader: MultiStream,
    queues: Mutex<HashMap<PublicRepositoryId, VecDeque<Vec<u8>>>>,
    queues_changed_tx: watch::Sender<()>,
}

impl RecvState {
    // Pops a message from the corresponding queue.
    fn pop(&self, id: &PublicRepositoryId) -> Option<Vec<u8>> {
        self.queues.lock().unwrap().get_mut(id)?.pop_back()
    }

    // Pushes the message into the corresponding queue, creating it if it didn't exist. Wakes up any
    // waiting streams so they can grab the message if it is for them.
    fn push(&self, message: Message) {
        self.queues
            .lock()
            .unwrap()
            .entry(message.id)
            .or_default()
            .push_front(message.content);
        self.queues_changed_tx.send(()).unwrap_or(());
    }
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Internal

// Stream of `Message` backed by a `TcpStream`. Closes on first error. Contains a connection
// permit which gets released on drop.
struct PermittedStream {
    inner: MessageStream<tcp::OwnedReadHalf>,
    _permit: ConnectionPermitHalf,
}

impl PermittedStream {
    fn new(stream: tcp::OwnedReadHalf, permit: ConnectionPermitHalf) -> Self {
        Self {
            inner: MessageStream::new(stream),
            _permit: permit,
        }
    }
}

impl Stream for PermittedStream {
    type Item = Message;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.inner.poll_next_unpin(cx)) {
            Some(Ok(message)) => Poll::Ready(Some(message)),
            Some(Err(_)) | None => Poll::Ready(None),
        }
    }
}

// Sink for `Message` backed by a `TcpStream`.
// Contains a connection permit which gets released on drop.
struct PermittedSink {
    inner: MessageSink<tcp::OwnedWriteHalf>,
    _permit: ConnectionPermitHalf,
}

impl PermittedSink {
    fn new(stream: tcp::OwnedWriteHalf, permit: ConnectionPermitHalf) -> Self {
        Self {
            inner: MessageSink::new(stream),
            _permit: permit,
        }
    }
}

// `Sink` impl just trivially delegates to the underlying sink.
impl Sink<Message> for PermittedSink {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.inner.start_send_unpin(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_close_unpin(cx)
    }
}

// Stream that reads `Message`s from multiple underlying TCP streams concurrently.
struct MultiStream {
    inner: Mutex<MultiStreamInner>,
}

impl MultiStream {
    fn new() -> Self {
        Self {
            inner: Mutex::new(MultiStreamInner {
                streams: SelectAll::new(),
                waker: None,
            }),
        }
    }

    fn add(&self, stream: PermittedStream) {
        let mut inner = self.inner.lock().unwrap();
        inner.streams.push(stream);
        inner.wake();
    }

    // Receive next message from this stream. Equivalent to
    //
    // ```ignore
    // async fn recv(&self) -> Option<Message>;
    // ```
    fn recv(&self) -> Recv {
        Recv { inner: &self.inner }
    }

    // Closes this stream. Any subsequent `recv` will immediately return `None` unless new
    // streams are added first.
    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.streams.clear();
        inner.wake();
    }
}

struct MultiStreamInner {
    streams: SelectAll<PermittedStream>,
    waker: Option<Waker>,
}

impl MultiStreamInner {
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake()
        }
    }
}

// Future returned from [`MultiStream::recv`].
struct Recv<'a> {
    inner: &'a Mutex<MultiStreamInner>,
}

impl Future for Recv<'_> {
    type Output = Option<Message>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut inner = self.inner.lock().unwrap();

        match inner.streams.poll_next_unpin(cx) {
            Poll::Ready(message) => Poll::Ready(message),
            Poll::Pending => {
                if inner.waker.is_none() {
                    inner.waker = Some(cx.waker().clone());
                }

                Poll::Pending
            }
        }
    }
}

// Sink that writes to multiple underlying TCP streams sequentially until one of them succeeds,
// automatically removing the failed ones.
//
// NOTE: Doesn't actually implement the `Sink` trait currently because we don't need it, only
// provides an async `send` method.
struct MultiSink {
    inner: Mutex<MultiSinkInner>,
}

impl MultiSink {
    fn new() -> Self {
        Self {
            inner: Mutex::new(MultiSinkInner {
                sinks: Vec::new(),
                waker: None,
            }),
        }
    }

    fn add(&self, sink: PermittedSink) {
        let mut inner = self.inner.lock().unwrap();
        inner.sinks.push(sink);
        inner.wake();
    }

    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.sinks.clear();
        inner.wake();
    }

    // Returns whether the send succeeded.
    //
    // Equivalent to
    //
    // ```ignore
    // async fn send(&self, message: Message) -> bool;
    // ```
    //
    fn send(&self, message: Message) -> Send {
        Send {
            message,
            pending: true,
            inner: &self.inner,
        }
    }
}

struct MultiSinkInner {
    sinks: Vec<PermittedSink>,
    waker: Option<Waker>,
}

impl MultiSinkInner {
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake()
        }
    }
}

// Future returned from [`MultiSink::send`].
struct Send<'a> {
    message: Message,
    pending: bool,
    inner: &'a Mutex<MultiSinkInner>,
}

impl Future for Send<'_> {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.lock().unwrap();

        loop {
            let sink = if let Some(sink) = inner.sinks.first_mut() {
                sink
            } else {
                return Poll::Ready(false);
            };

            match sink.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    if !self.pending {
                        return Poll::Ready(true);
                    }
                }
                Poll::Ready(Err(_)) => {
                    inner.sinks.swap_remove(0);
                    self.pending = true;
                    continue;
                }
                Poll::Pending => {
                    if inner.waker.is_none() {
                        inner.waker = Some(cx.waker().clone());
                    }

                    return Poll::Pending;
                }
            }

            // TODO: can we avoid the clone here?
            match sink.start_send_unpin(self.message.clone()) {
                Ok(()) => {
                    self.pending = false;
                }
                Err(_) => {
                    inner.sinks.swap_remove(0);
                    self.pending = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn recv_on_stream() {
        let (mut client, server) = setup().await;

        let repo_id = PublicRepositoryId::random();
        let send_content = b"hello world";

        client
            .send(Message {
                id: repo_id,
                content: send_content.to_vec(),
            })
            .await
            .unwrap();

        let mut server_stream = server.open_recv(repo_id);

        let recv_content = server_stream.recv().await.unwrap();
        assert_eq!(recv_content, send_content);
    }

    #[tokio::test]
    async fn recv_on_two_streams() {
        let (mut client, server) = setup().await;

        let repo_id0 = PublicRepositoryId::random();
        let repo_id1 = PublicRepositoryId::random();

        let send_content0 = b"one two three";
        let send_content1 = b"four five six";

        for (repo_id, content) in [(repo_id0, send_content0), (repo_id1, send_content1)] {
            client
                .send(Message {
                    id: repo_id,
                    content: content.to_vec(),
                })
                .await
                .unwrap();
        }

        let server_stream0 = server.open_recv(repo_id0);
        let server_stream1 = server.open_recv(repo_id1);

        for (mut server_stream, send_content) in [
            (server_stream0, send_content0),
            (server_stream1, send_content1),
        ] {
            let recv_content = server_stream.recv().await.unwrap();
            assert_eq!(recv_content, send_content);
        }
    }

    #[tokio::test]
    async fn drop_stream() {
        let (mut client, server) = setup().await;

        let repo_id = PublicRepositoryId::random();

        let send_content0 = b"one two three";
        let send_content1 = b"four five six";

        for content in [send_content0, send_content1] {
            client
                .send(Message {
                    id: repo_id,
                    content: content.to_vec(),
                })
                .await
                .unwrap();
        }

        let mut server_stream0 = server.open_recv(repo_id);
        let mut server_stream1 = server.open_recv(repo_id);

        let recv_content = server_stream0.recv().await.unwrap();
        assert_eq!(recv_content, send_content0);

        drop(server_stream0);

        let recv_content = server_stream1.recv().await.unwrap();
        assert_eq!(recv_content, send_content1)
    }

    #[tokio::test]
    async fn drop_dispatcher() {
        let (_client, server) = setup().await;

        let repo_id = PublicRepositoryId::random();

        let mut server_stream = server.open_recv(repo_id);

        drop(server);

        assert!(server_stream.recv().await.is_none());
    }

    #[tokio::test]
    async fn multi_stream_close() {
        let (client, server) = create_connected_sockets().await;
        let (server_reader, _server_writer) = server.into_split();

        let stream = MultiStream::new();
        stream.add(PermittedStream::new(
            server_reader,
            ConnectionPermit::dummy().split().0,
        ));

        let mut client = MessageSink::new(client);
        client
            .send(Message {
                id: PublicRepositoryId::random(),
                content: b"hello world".to_vec(),
            })
            .await
            .unwrap();

        stream.close();

        assert!(stream.recv().await.is_none());
    }

    async fn setup() -> (MessageSink<TcpStream>, MessageDispatcher) {
        let (client, server) = create_connected_sockets().await;
        let client_writer = MessageSink::new(client);

        let server_dispatcher = MessageDispatcher::new();
        server_dispatcher.bind(server, ConnectionPermit::dummy());

        (client_writer, server_dispatcher)
    }

    async fn create_connected_sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();

        (client, server)
    }
}
