//! JSON-RPC 2.0 over a stream of lines, in both directions at once. The
//! plugin side, the host side and plain MCP serving all use this one peer, so
//! limits, timeouts, cancellation and error codes are the same everywhere.
//!
//! Each side can send requests and answer the other's. Incoming requests run
//! concurrently, up to a limit: past it, a request is refused at once rather
//! than queued. A request that's abandoned, by a timeout or by the caller
//! giving up, tells the other side with `notifications/cancelled`, and a
//! request the other side cancels is stopped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use schema::bounded;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use crate::read_line;

/// The message isn't JSON.
pub const PARSE_ERROR: i64 = -32700;
/// The message isn't a JSON-RPC request.
pub const INVALID_REQUEST: i64 = -32600;
/// No such method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The parameters don't fit the method.
pub const INVALID_PARAMS: i64 = -32602;
/// The server couldn't do it, for its own reasons.
pub const SERVER_ERROR: i64 = -32000;
/// Too many requests are running on this connection already.
pub const BUSY: i64 = -32004;

/// The notification that stops a request.
pub const CANCELLED: &str = "notifications/cancelled";

/// A successful reply.
#[must_use]
pub fn success(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// An error reply. The message is cut to a bounded length.
#[must_use]
pub fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": bounded(message) } })
}

/// What answers the other side's requests and notifications.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// The reply to a request. A notification gets `None`, and so may a
    /// request that should go unanswered.
    async fn handle(&self, message: Value) -> Option<Value>;
}

/// How much one connection may hold at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Incoming requests running at once. More are refused with [`BUSY`].
    pub in_flight: usize,
    /// Messages waiting to be written.
    pub queue: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            in_flight: 32,
            queue: 64,
        }
    }
}

/// A request didn't get a result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// The connection ended.
    #[error("the other side has gone")]
    Gone,
    /// No answer in time.
    #[error("no answer in time")]
    Timeout,
    /// The other side answered with an error.
    #[error("{message}")]
    Remote {
        /// Its code.
        code: i64,
        /// Its message.
        message: String,
    },
    /// The answer doesn't fit JSON-RPC.
    #[error("a malformed answer: {0}")]
    Malformed(String),
}

type Waiters = HashMap<u64, oneshot::Sender<Value>>;

/// One side of a connection. Requests go out through it; [`Peer::run`]
/// reads what comes back and serves what the other side asks.
#[derive(Debug)]
pub struct Peer {
    outgoing: mpsc::Sender<String>,
    waiting: Mutex<Waiters>,
    next_id: AtomicU64,
}

/// The parts of a connection that [`Peer::run`] drives.
#[derive(Debug)]
pub struct Io {
    queue: mpsc::Receiver<String>,
    limits: Limits,
}

/// Forgets an abandoned request, and tells the other side to stop it.
struct Pending<'a> {
    peer: &'a Peer,
    id: u64,
    answered: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.peer.waiters().remove(&self.id);
        if !self.answered {
            let cancel = json!({
                "jsonrpc": "2.0",
                "method": CANCELLED,
                "params": { "requestId": self.id, "reason": "the caller gave up" },
            });
            // Best effort: a full queue means the other side isn't reading.
            let _ = self.peer.outgoing.try_send(cancel.to_string());
        }
    }
}

impl Peer {
    /// A peer, and the parts [`Peer::run`] needs to drive it.
    #[must_use]
    pub fn new(limits: Limits) -> (Arc<Self>, Io) {
        let (outgoing, queue) = mpsc::channel(limits.queue.max(1));
        let peer = Arc::new(Self {
            outgoing,
            waiting: Mutex::default(),
            next_id: AtomicU64::new(1),
        });
        (peer, Io { queue, limits })
    }

    fn waiters(&self) -> std::sync::MutexGuard<'_, Waiters> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends a request and waits for its result, for at most `limit` if one
    /// is given. The wait covers sending it as well.
    ///
    /// # Errors
    ///
    /// [`RpcError`] if the connection ends, time runs out, or the other side
    /// answers with an error or nonsense.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        limit: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.waiters().insert(id, sender);
        let mut pending = Pending {
            peer: self,
            id,
            answered: false,
        };
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let exchange = async {
            self.outgoing
                .send(message.to_string())
                .await
                .map_err(|_| RpcError::Gone)?;
            receiver.await.map_err(|_| RpcError::Gone)
        };
        let reply = match limit {
            Some(limit) => tokio::time::timeout(limit, exchange)
                .await
                .map_err(|_| RpcError::Timeout)?,
            None => exchange.await,
        };
        // Gone or answered: either way there's nothing to cancel.
        pending.answered = true;
        result_of(&reply?)
    }

    /// Sends a notification. `Value::Null` sends it without parameters.
    ///
    /// # Errors
    ///
    /// [`RpcError::Gone`] if the connection has ended.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        let message = if params.is_null() {
            json!({ "jsonrpc": "2.0", "method": method })
        } else {
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
        };
        self.outgoing
            .send(message.to_string())
            .await
            .map_err(|_| RpcError::Gone)
    }

    fn answer(&self, reply: Value) {
        let waiter = reply
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| self.waiters().remove(&id));
        if let Some(waiter) = waiter {
            // The caller may have given up; that's fine.
            let _ = waiter.send(reply);
        }
    }

    /// Reads from `reader` and writes to `writer` until `reader` ends: routes
    /// replies to the requests waiting on them, and has `handler` answer
    /// the other side's messages. When the stream ends, waiting requests
    /// fail, the requests being served finish, and their replies are
    /// written before it returns.
    ///
    /// # Errors
    ///
    /// The read error. A write error only stops the writing, since the
    /// reading side learns of a broken connection on its own.
    pub async fn run<R, W>(
        &self,
        io: Io,
        reader: R,
        writer: W,
        handler: Arc<dyn Handler>,
    ) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Io { queue, limits } = io;
        let finished = Notify::new();
        let reading = async {
            let result = self.read_all(reader, handler, limits).await;
            finished.notify_one();
            result
        };
        let ((), result) = tokio::join!(write_all(queue, writer, &finished), reading);
        result
    }

    async fn read_all<R: AsyncRead + Unpin>(
        &self,
        reader: R,
        handler: Arc<dyn Handler>,
        limits: Limits,
    ) -> std::io::Result<()> {
        let permits = Arc::new(Semaphore::new(limits.in_flight.max(1)));
        let mut serving = Serving::default();
        let mut reader = BufReader::new(reader);
        let result = loop {
            serving.reap();
            let line = match read_line(&mut reader).await {
                Ok(Some(line)) => line,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                self.refuse(&Value::Null, PARSE_ERROR, "not JSON");
                continue;
            };
            if message.get("method").is_none() {
                self.answer(message);
                continue;
            }
            if message.get("method").and_then(Value::as_str) == Some(CANCELLED) {
                serving.cancel(&message);
                continue;
            }
            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                if let Some(id) = message.get("id") {
                    self.refuse(id, BUSY, "too many requests at once on this connection");
                }
                continue;
            };
            serving.start(message, Arc::clone(&handler), self.outgoing.clone(), permit);
        };
        // The other side is gone: anyone waiting on it hears so, and what it
        // asked for finishes and is answered as far as that can go.
        self.waiters().clear();
        serving.finish().await;
        result
    }

    /// Answers with an error now, without waiting for room to write it.
    fn refuse(&self, id: &Value, code: i64, message: &str) {
        let _ = self
            .outgoing
            .try_send(failure(id, code, message).to_string());
    }
}

/// The other side's requests being served, by id, so one can be cancelled.
#[derive(Default)]
struct Serving {
    tasks: JoinSet<Option<String>>,
    by_id: HashMap<String, AbortHandle>,
}

impl Serving {
    fn start(
        &mut self,
        message: Value,
        handler: Arc<dyn Handler>,
        outgoing: mpsc::Sender<String>,
        permit: OwnedSemaphorePermit,
    ) {
        let key = message.get("id").map(Value::to_string);
        let returned = key.clone();
        let task = self.tasks.spawn(async move {
            if let Some(reply) = handler.handle(message).await {
                let _ = outgoing.send(reply.to_string()).await;
            }
            drop(permit);
            returned
        });
        if let Some(key) = key {
            self.by_id.insert(key, task);
        }
    }

    fn reap(&mut self) {
        while let Some(done) = self.tasks.try_join_next() {
            if let Ok(Some(key)) = done {
                self.by_id.remove(&key);
            }
        }
    }

    fn cancel(&mut self, message: &Value) {
        let key = message
            .get("params")
            .and_then(|params| params.get("requestId"))
            .map(Value::to_string);
        if let Some(task) = key.and_then(|key| self.by_id.remove(&key)) {
            // A cancelled request gets no reply.
            task.abort();
        }
    }

    async fn finish(mut self) {
        while self.tasks.join_next().await.is_some() {}
    }
}

/// Writes each message as a line. After a write fails it keeps taking
/// messages, so no sender waits on a connection that's gone.
async fn write_all<W: AsyncWrite + Unpin>(
    mut queue: mpsc::Receiver<String>,
    mut writer: W,
    finished: &Notify,
) {
    let mut broken = false;
    let write = async |writer: &mut W, mut line: String| {
        line.push('\n');
        writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err()
    };
    loop {
        tokio::select! {
            biased;
            message = queue.recv() => match message {
                Some(line) if !broken => broken = write(&mut writer, line).await,
                Some(_) => {}
                None => return,
            },
            () = finished.notified() => break,
        }
    }
    // Reading has ended and every request it served has replied: write
    // what's left, and stop.
    queue.close();
    while let Some(line) = queue.recv().await {
        if !broken {
            broken = write(&mut writer, line).await;
        }
    }
}

fn result_of(reply: &Value) -> Result<Value, RpcError> {
    if let Some(error) = reply.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        return Err(RpcError::Remote {
            code,
            message: bounded(message),
        });
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| RpcError::Malformed("a reply with neither result nor error".to_owned()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#[allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, DuplexStream, duplex};

    /// Answers `echo` with its params, `wait` never, `slow` after a while
    /// and `fail` with an error.
    struct Echo;

    #[async_trait]
    impl Handler for Echo {
        async fn handle(&self, message: Value) -> Option<Value> {
            let id = message.get("id")?.clone();
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match message.get("method").and_then(Value::as_str) {
                Some("echo") => Some(success(&id, &params)),
                Some("wait") => {
                    std::future::pending::<()>().await;
                    None
                }
                Some("slow") => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some(success(&id, &json!("slow")))
                }
                Some("fail") => Some(failure(&id, INVALID_PARAMS, "nope")),
                _ => Some(failure(&id, METHOD_NOT_FOUND, "no such method")),
            }
        }
    }

    /// Two peers joined by an in-memory pipe, each running on its own task.
    fn pair(limits: Limits) -> (Arc<Peer>, Arc<Peer>) {
        let (left_stream, right_stream) = duplex(1 << 16);
        let start = |stream: DuplexStream| {
            let (peer, io) = Peer::new(limits);
            let (reader, writer) = tokio::io::split(stream);
            let running = Arc::clone(&peer);
            tokio::spawn(async move { running.run(io, reader, writer, Arc::new(Echo)).await });
            peer
        };
        (start(left_stream), start(right_stream))
    }

    #[tokio::test]
    async fn each_side_can_ask_the_other() {
        let (left, right) = pair(Limits::default());
        let asked = left.request("echo", json!({ "a": 1 }), None).await;
        assert_eq!(asked, Ok(json!({ "a": 1 })));
        let back = right.request("echo", json!("b"), None).await;
        assert_eq!(back, Ok(json!("b")));
    }

    #[tokio::test]
    async fn an_error_reply_keeps_its_code() {
        let (left, _right) = pair(Limits::default());
        let error = left.request("fail", json!({}), None).await.unwrap_err();
        assert_eq!(
            error,
            RpcError::Remote {
                code: INVALID_PARAMS,
                message: "nope".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn many_requests_at_once_each_get_their_own_answer() {
        let (left, _right) = pair(Limits::default());
        let asks = (0..20).map(|n| {
            let left = Arc::clone(&left);
            tokio::spawn(async move { left.request("echo", json!(n), None).await })
        });
        for (n, ask) in asks.enumerate() {
            assert_eq!(ask.await.unwrap(), Ok(json!(n)));
        }
    }

    #[tokio::test]
    async fn a_request_past_the_limit_is_refused_not_queued() {
        let limits = Limits {
            in_flight: 1,
            queue: 8,
        };
        let (left, _right) = pair(limits);
        let stuck = {
            let left = Arc::clone(&left);
            tokio::spawn(async move { left.request("wait", json!({}), None).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let refused = left.request("echo", json!(1), None).await.unwrap_err();
        assert!(
            matches!(refused, RpcError::Remote { code: BUSY, .. }),
            "{refused:?}"
        );
        stuck.abort();
    }

    #[tokio::test]
    async fn a_timeout_cancels_the_request_and_frees_its_place() {
        let limits = Limits {
            in_flight: 1,
            queue: 8,
        };
        let (left, _right) = pair(limits);
        let timed_out = left
            .request("wait", json!({}), Some(Duration::from_millis(20)))
            .await;
        assert_eq!(timed_out, Err(RpcError::Timeout));
        assert!(left.waiters().is_empty());
        // The other side stopped the request it was told to cancel, so its
        // one place is free again.
        let mut answered = None;
        for _ in 0..50 {
            if let Ok(value) = left.request("echo", json!(2), None).await {
                answered = Some(value);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(answered, Some(json!(2)));
    }

    #[tokio::test]
    async fn when_the_stream_ends_waiting_requests_fail_and_served_ones_finish() {
        // One pipe each way, so hanging up one direction ends the stream.
        let (mut their_writer, reader) = duplex(1 << 16);
        let (writer, their_reader) = duplex(1 << 16);
        let (peer, io) = Peer::new(Limits::default());
        let running = Arc::clone(&peer);
        let run =
            tokio::spawn(async move { running.run(io, reader, writer, Arc::new(Echo)).await });
        // They ask for something slow, and hang up before it's answered.
        their_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"slow\"}\nnot json\n")
            .await
            .unwrap();
        let waiting = {
            let peer = Arc::clone(&peer);
            tokio::spawn(async move { peer.request("wait", json!({}), None).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(their_writer);
        run.await.unwrap().unwrap();
        assert_eq!(waiting.await.unwrap(), Err(RpcError::Gone));
        // Everything written to them: the parse error, our request, and the
        // slow reply that finished after they hung up.
        let mut lines = BufReader::new(their_reader).lines();
        let mut seen = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            seen.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert!(
            seen.iter()
                .any(|m| m["error"]["code"] == PARSE_ERROR && m["id"].is_null())
        );
        assert!(seen.iter().any(|m| m["method"] == "wait"));
        assert!(
            seen.iter().any(|m| m["id"] == 7 && m["result"] == "slow"),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_request_after_the_end_is_gone() {
        let (peer, io) = Peer::new(Limits::default());
        drop(io);
        let error = peer.request("echo", json!(1), None).await.unwrap_err();
        assert_eq!(error, RpcError::Gone);
    }

    #[test]
    fn a_reply_with_neither_result_nor_error_is_malformed() {
        let error = result_of(&json!({ "jsonrpc": "2.0", "id": 1 })).unwrap_err();
        assert!(matches!(error, RpcError::Malformed(_)));
    }
}
