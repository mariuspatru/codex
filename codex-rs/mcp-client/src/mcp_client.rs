//! A minimal async client for the Model Context Protocol (MCP).
//!
//! The client is intentionally lightweight – it is only capable of:
//!   1. Spawning a subprocess that launches a conforming MCP server that
//!      communicates over stdio.
//!   2. Sending MCP requests and pairing them with their corresponding
//!      responses.
//!   3. Offering a convenience helper for the common `tools/list` request.
//!
//! The crate hides all JSON‐RPC framing details behind a typed API. Users
//! interact with the [`ModelContextProtocolRequest`] trait from `mcp-types` to
//! issue requests and receive strongly-typed results.

use std::collections::HashMap;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use mcp_types::JSONRPCMessage;
use mcp_types::JSONRPCNotification;
use mcp_types::JSONRPCRequest;
use mcp_types::JSONRPCResponse;
use mcp_types::ListToolsRequest;
use mcp_types::ListToolsRequestParams;
use mcp_types::ListToolsResult;
use mcp_types::ModelContextProtocolRequest;
use mcp_types::RequestId;
use mcp_types::JSONRPC_VERSION;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tracing::error;
use tracing::info;
use tracing::warn;

/// Capacity of the bounded channels used for transporting messages between the
/// client API and the IO tasks.
const CHANNEL_CAPACITY: usize = 128;

/// Internal representation of a pending request sender.
type PendingSender = oneshot::Sender<JSONRPCMessage>;

/// A running MCP client instance.
pub struct McpClient {
    /// Retain this child process until the client is dropped. The Tokio runtime
    /// will make a "best effort" to reap the process after it exits, but it is
    /// not a guarantee. See the `kill_on_drop` documentation for details.
    #[allow(dead_code)]
    child: tokio::process::Child,

    /// Channel for sending JSON-RPC messages *to* the background writer task.
    outgoing_tx: mpsc::Sender<JSONRPCMessage>,

    /// Map of `request.id -> oneshot::Sender` used to dispatch responses back
    /// to the originating caller.
    pending: Arc<Mutex<HashMap<i64, PendingSender>>>,

    /// Monotonically increasing counter used to generate request IDs.
    id_counter: AtomicI64,
}

impl McpClient {
    /// Spawn the given command and establish an MCP session over its STDIO.
    ///
    /// `args` follows the Unix convention where the first element is the
    /// executable path and the rest are arguments.  For example:
    ///
    /// ```no_run
    /// # use codex_mcp_client::McpClient;
    /// # async fn run() -> anyhow::Result<()> {
    /// let client = McpClient::new_stdio_client(vec![
    ///     "codex-mcp-server".to_string(),
    /// ]).await?;
    /// # Ok(()) }
    /// ```
    pub async fn new_stdio_client(args: Vec<String>) -> std::io::Result<Self> {
        if args.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "expected at least one element in `args` - the program to spawn",
            ));
        }

        let program = &args[0];
        let mut command = Command::new(program);
        if args.len() > 1 {
            command.args(&args[1..]);
        }

        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::null());
        // As noted in the `kill_on_drop` documentation, the Tokio runtime makes
        // a "best effort" to reap-after-exit to avoid zombie processes, but it
        // is not a guarantee.
        command.kill_on_drop(true);
        let mut child = command.spawn()?;

        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "failed to capture child stdin")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "failed to capture child stdout")
        })?;

        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<JSONRPCMessage>(CHANNEL_CAPACITY);
        let pending: Arc<Mutex<HashMap<i64, PendingSender>>> = Arc::new(Mutex::new(HashMap::new()));

        // Spawn writer task. It listens on the `outgoing_rx` channel and
        // writes messages to the child's STDIN.
        let writer_handle = {
            let mut stdin = stdin;
            tokio::spawn(async move {
                while let Some(msg) = outgoing_rx.recv().await {
                    match serde_json::to_string(&msg) {
                        Ok(json) => {
                            if stdin.write_all(json.as_bytes()).await.is_err() {
                                error!("failed to write message to child stdin");
                                break;
                            }
                            if stdin.write_all(b"\n").await.is_err() {
                                error!("failed to write newline to child stdin");
                                break;
                            }
                            if stdin.flush().await.is_err() {
                                error!("failed to flush child stdin");
                                break;
                            }
                        }
                        Err(e) => error!("failed to serialize JSONRPCMessage: {e}"),
                    }
                }
            })
        };

        // Spawn reader task. It reads line-delimited JSON from the child's
        // STDOUT and dispatches responses to the pending map.
        let reader_handle = {
            let pending = pending.clone();
            let mut lines = BufReader::new(stdout).lines();

            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    match serde_json::from_str::<JSONRPCMessage>(&line) {
                        Ok(JSONRPCMessage::Response(resp)) => {
                            Self::dispatch_response(resp, &pending).await;
                        }
                        Ok(JSONRPCMessage::Error(err)) => {
                            Self::dispatch_error(err, &pending).await;
                        }
                        Ok(JSONRPCMessage::Notification(JSONRPCNotification { .. })) => {
                            // For now we only log server-initiated notifications.
                            info!("<- notification: {}", line);
                        }
                        Ok(other) => {
                            // Batch responses and requests are currently not
                            // expected from the server – log and ignore.
                            info!("<- unhandled message: {:?}", other);
                        }
                        Err(e) => {
                            error!("failed to deserialize JSONRPCMessage: {e}; line = {}", line)
                        }
                    }
                }
            })
        };

        // We intentionally *detach* the tasks. They will keep running in the
        // background as long as their respective resources (channels/stdin/
        // stdout) are alive. Dropping `McpClient` cancels the tasks due to
        // dropped resources.
        let _ = (writer_handle, reader_handle);

        Ok(Self {
            child,
            outgoing_tx,
            pending,
            id_counter: AtomicI64::new(1),
        })
    }

    /// Send an arbitrary MCP request and await the typed result.
    pub async fn send_request<R>(&self, params: R::Params) -> Result<R::Result>
    where
        R: ModelContextProtocolRequest,
        R::Params: Serialize,
        R::Result: DeserializeOwned,
    {
        // Create a new unique ID.
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let request_id = RequestId::Integer(id);

        // Serialize params -> JSON. For many request types `Params` is
        // `Option<T>` and `None` should be encoded as *absence* of the field.
        let params_json = serde_json::to_value(&params)?;
        let params_field = if params_json.is_null() {
            None
        } else {
            Some(params_json)
        };

        let jsonrpc_request = JSONRPCRequest {
            id: request_id.clone(),
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: R::METHOD.to_string(),
            params: params_field,
        };

        let message = JSONRPCMessage::Request(jsonrpc_request);

        // oneshot channel for the response.
        let (tx, rx) = oneshot::channel();

        // Register in pending map *before* sending the message so a race where
        // the response arrives immediately cannot be lost.
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }

        // Send to writer task.
        if self.outgoing_tx.send(message).await.is_err() {
            return Err(anyhow!(
                "failed to send message to writer task – channel closed"
            ));
        }

        // Await the response.
        let msg = rx
            .await
            .map_err(|_| anyhow!("response channel closed before a reply was received"))?;

        match msg {
            JSONRPCMessage::Response(JSONRPCResponse { result, .. }) => {
                let typed: R::Result = serde_json::from_value(result)?;
                Ok(typed)
            }
            JSONRPCMessage::Error(err) => Err(anyhow!(format!(
                "server returned JSON-RPC error: code = {}, message = {}",
                err.error.code, err.error.message
            ))),
            other => Err(anyhow!(format!(
                "unexpected message variant received in reply path: {:?}",
                other
            ))),
        }
    }

    /// Convenience wrapper around `tools/list`.
    pub async fn list_tools(
        &self,
        params: Option<ListToolsRequestParams>,
    ) -> Result<ListToolsResult> {
        self.send_request::<ListToolsRequest>(params).await
    }

    /// Internal helper: route a JSON-RPC *response* object to the pending map.
    async fn dispatch_response(
        resp: JSONRPCResponse,
        pending: &Arc<Mutex<HashMap<i64, PendingSender>>>,
    ) {
        let id = match resp.id {
            RequestId::Integer(i) => i,
            RequestId::String(_) => {
                // We only ever generate integer IDs. Receiving a string here
                // means we will not find a matching entry in `pending`.
                error!("response with string ID - no matching pending request");
                return;
            }
        };

        if let Some(tx) = pending.lock().await.remove(&id) {
            // Ignore send errors – the receiver might have been dropped.
            let _ = tx.send(JSONRPCMessage::Response(resp));
        } else {
            warn!(id, "no pending request found for response");
        }
    }

    /// Internal helper: route a JSON-RPC *error* object to the pending map.
    async fn dispatch_error(
        err: mcp_types::JSONRPCError,
        pending: &Arc<Mutex<HashMap<i64, PendingSender>>>,
    ) {
        let id = match err.id {
            RequestId::Integer(i) => i,
            RequestId::String(_) => return, // see comment above
        };

        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(JSONRPCMessage::Error(err));
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Even though we have already tagged this process with
        // `kill_on_drop(true)` above, this extra check has the benefit of
        // forcing the process to be reaped immediately if it has already exited
        // instead of waiting for the Tokio runtime to reap it later.
        let _ = self.child.try_wait();
    }
}
