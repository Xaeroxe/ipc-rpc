//! Inter-Process Communication Remote Procedure Calls
//!
//! [![Crates.io](https://img.shields.io/crates/v/ipc-rpc.svg)](https://crates.io/crates/ipc-rpc/)
//!
//! This Rust library is a wrapper over [`servo/ipc-channel`](https://github.com/servo/ipc-channel) that adds many new features.
//!
//! - Bi-directional communication by default.
//! - [Future](https://doc.rust-lang.org/stable/std/future/trait.Future.html) based reply mechanism allows easy and accurate pairing of requests and responses.
//! - (Optional, enabled by default) Validation on startup that message schemas match between the server and client. No more debugging bad deserializations. Relies on [`schemars`](https://crates.io/crates/schemars).
//! - Streamlined initialization of IPC channels for common use cases, while still allowing for more flexible and robust dynamic initialization in more unique scenarios.
//!
//! Compatible with anything that can run `servo/ipc-channel`, which at time of writing includes
//!
//! - Windows
//! - MacOS
//! - Linux
//! - FreeBSD
//! - OpenBSD
//!
//! Additionally, `servo/ipc-channel` supports the following platforms but only while in `inprocess` mode, which is not capable of communication between processes.
//!
//! - Android
//! - iOS
//! - WASI
//!
//! ## tokio
//!
//! This crate uses the [`tokio`](https://crates.io/crates/tokio) runtime for executing futures, and it is a hard requirement that users of `ipc-rpc` must use `tokio`. There are no plans to add support for other
//! executors, but sufficient demand for other executors may change that.
//!
//! # Cargo features
//!
//! This crate exposes one feature, `message-schema-validation`, which is on by default. This enables functionality related to the [`schemars`](https://crates.io/crates/schemars) crate.
//! When enabled, the software will attempt to validate the user message schema on initialization of the connection. Failure to validate is not a critical failure, and won't crash the program.
//! An error will be emitted in the logs, and this status can be retrieved programmatically via many functions, all called `schema_validated()`.
//!
//! If you decide that a failure to validate the schema should be a critical failure you can add the following line of code to your program for execution after a connection is established.
//!
//! ### Server
//! ```
//! # async {
//! # let (_key, mut server) = ipc_rpc::IpcRpcServer::initialize_server(|_| async {Option::<()>::None}).await.unwrap();
//! server.schema_validated().await.unwrap().assert_success();
//! # };
//! ```
//!
//! ### Client
//! ```
//! # async {
//! # use ipc_rpc::{IpcRpcClient, ConnectionKey};
//! # let mut client = IpcRpcClient::initialize_client(ConnectionKey::from(uuid::Uuid::nil()), |_| async {Option::<()>::None}).await.unwrap();
//! client.schema_validated().await.unwrap().assert_success();
//! # };
//! ```
//!
//! # Limitations
//!
//! Much like `servo/ipc-channel`, servers may only serve one client. Overcoming this limitation would require work within `servo/ipc-channel`.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt::Debug,
    future::Future,
    io,
    path::Path,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{Stream, StreamExt};
use ipc_channel::ipc::{IpcError, IpcReceiver, IpcSender, TryRecvError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[cfg(feature = "message-schema-validation")]
use schemars::{schema::RootSchema, schema_for, JsonSchema};

mod client;
pub use client::*;
mod server;
pub use server::*;

/// This key can be used to connect to the IPC server it came with, even outside of this process.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ConnectionKey(Uuid);

impl TryFrom<String> for ConnectionKey {
    type Error = uuid::Error;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        uuid::Uuid::parse_str(&s).map(Self)
    }
}

impl FromStr for ConnectionKey {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(s).map(Self)
    }
}

impl ToString for ConnectionKey {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

impl From<ConnectionKey> for OsString {
    fn from(s: ConnectionKey) -> Self {
        OsString::from(s.0.to_string())
    }
}

impl From<ConnectionKey> for String {
    fn from(key: ConnectionKey) -> Self {
        key.0.to_string()
    }
}

impl From<Uuid> for ConnectionKey {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl From<ConnectionKey> for Uuid {
    fn from(o: ConnectionKey) -> Self {
        o.0
    }
}

/// The pending reply box maintains a list of messages that are awaiting a reply. Messages are
/// paired via their uuids, and delivered via the tokio mpsc framework to a future awaiting
/// the reply. If a reply never arrives eventually a spawned Future responsible for maintenance
/// of this mail box will clean them out and resolve the futures with a time out.
type PendingReplyEntry<U> = (
    Uuid,
    (
        mpsc::UnboundedSender<Result<InternalMessageKind<U>, IpcRpcError>>,
        Instant,
    ),
);
/// Internal protocol message structure. Wraps the actual user message with some structure
/// helpful to the mail delivery system. You could liken this to a letter's envelope.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(bound(deserialize = ""))]
struct InternalMessage<U: UserMessage> {
    /// An identifier used for pairing messages with responses. A response to a message should
    /// carry the same UUID the message itself had. Otherwise, the UUID should be unique.
    uuid: uuid::Uuid,
    /// The actual contents of the message.
    kind: InternalMessageKind<U>,
}

/// There are many reasons we might need to send a message. This enumerates them. The enum also
/// contains some messages intended for internal use that downstream users shouldn't concern
/// themselves with.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(bound(deserialize = ""))]
enum InternalMessageKind<U: UserMessage> {
    /// Initialize connection. Includes an IpcSender which can be used to communicate with the client.
    InitConnection(IpcSender<InternalMessage<U>>),
    /// The conversation has come to a close, it's time to shut down.
    Hangup,
    /// Downstream has a message to exchange.
    UserMessage(U),
    /// Describes the user message schema for validation purposes
    UserMessageSchema(String),
    /// Returned if the user message schema matched
    UserMessageSchemaOk,
    /// Returned if the user message schema did not match
    UserMessageSchemaError { other_schema: String },
}

/// Errors which can occur with ipc-rpc during operation.
#[derive(Clone, Debug, Error)]
pub enum IpcRpcError {
    #[error("io error")]
    IoError(#[from] Arc<io::Error>),
    #[error("internal ipc channel error")]
    IpcChannelError(#[from] Arc<ipc_channel::Error>),
    #[error("connection initialization timed out")]
    ConnectTimeout,
    #[error("connection established, but initial handshake was not performed properly")]
    HandshakeFailure,
    #[error("client already connected")]
    ClientAlreadyConnected,
    #[error("peer disconnected")]
    Disconnected,
    #[error("time out while waiting for a reply")]
    ReplyTimeout,
    #[error("connection dropped pre-emptively")]
    ConnectionDropped,
}

impl From<io::Error> for IpcRpcError {
    fn from(e: io::Error) -> Self {
        Self::IoError(Arc::new(e))
    }
}

impl From<ipc_channel::Error> for IpcRpcError {
    fn from(e: ipc_channel::Error) -> Self {
        Self::IpcChannelError(Arc::new(e))
    }
}

/// The default timeout used for `send()` style methods. Use `send_timeout()` to use something else.
///
/// This may change in the future. The current value is
/// ```
/// # use tokio::time::Duration;
/// # assert_eq!(
/// # ipc_rpc::DEFAULT_REPLY_TIMEOUT,
/// Duration::from_secs(5)
/// # );
/// ```
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const PENDING_REPLY_CLEANUP_CHECK_DURATION: Duration = Duration::from_millis(100);

/// Processes incoming messages for both the client and the server. Responsible for distributing
/// and generating replies.
async fn process_incoming_mail<
    Fut: Future<Output = Option<U>> + Send,
    F: Fn(U) -> Fut + Send + Sync + 'static,
    U: UserMessage,
>(
    is_server: bool,
    mut pending_reply_receiver: mpsc::UnboundedReceiver<PendingReplyEntry<U>>,
    mut receiver: IpcReceiveStream<InternalMessage<U>>,
    message_handler: F,
    response_sender: IpcSender<InternalMessage<U>>,
    status_sender: watch::Sender<ConnectionStatus>,
) {
    let mut pending_replies = HashMap::<
        Uuid,
        (
            mpsc::UnboundedSender<Result<InternalMessageKind<U>, IpcRpcError>>,
            Instant,
        ),
    >::new();
    let message_handler = Arc::new(message_handler);
    let log_prefix = get_log_prefix(is_server);
    log::info!("{}Processing incoming mail!", log_prefix);
    let mut consecutive_error_count = 0;
    let now = Instant::now();
    let mut pending_reply_scheduled_time = now + PENDING_REPLY_CLEANUP_CHECK_DURATION;
    loop {
        // Empty out the pending reply receiver before entering the select below. This guarantees
        // that all queued reply drop boxes are processed before we start receiving incoming mail.
        while let Ok(pending_reply) = pending_reply_receiver.try_recv() {
            pending_replies.insert(pending_reply.0, pending_reply.1);
        }
        tokio::select! {
            _ = tokio::time::sleep_until(pending_reply_scheduled_time) => {
                pending_replies.retain(|_k, v| {
                    let keep = v.1 <= pending_reply_scheduled_time;
                    if !keep {
                        let _ = v.0.send(Err(IpcRpcError::ReplyTimeout));
                    }
                    keep
                });
                pending_reply_scheduled_time += PENDING_REPLY_CLEANUP_CHECK_DURATION;
            }
            pending_reply = pending_reply_receiver.recv() => {
                match pending_reply {
                    None => {
                        // Sender got dropped, time to close up shop.
                        break;
                    }
                    Some((reply_identifer, reply_entry)) => {
                        pending_replies.insert(reply_identifer, reply_entry);
                    }
                }
            },
            r = receiver.next() => {
                match r {
                    None => {
                        // incoming mail stream ended, time to shut down.
                        break;
                    }
                    Some(Err(e)) => {
                        if let IpcError::Disconnected = e {
                            log::info!("{}Peer disconnected.", log_prefix);
                            break;
                        } else {
                            log::error!("{}Error receiving message from peer {:?}", log_prefix, e);
                            consecutive_error_count += 1;
                            if consecutive_error_count > 20 {
                                log::error!("{}Too many consecutive errors, shutting down.", log_prefix);
                                break;
                            }
                        }
                    }
                    Some(Ok(message)) => {
                        consecutive_error_count = 0;
                        log::debug!("{}Got message! {:?}", log_prefix, message);
                        let reply = pending_replies.remove(&message.uuid);
                        if let Some((reply_drop_box, _)) = reply {
                            log::debug!("{}It's a reply, forwarding!", log_prefix);
                            // If the end user doesn't want this message that's fine.
                            let _ = reply_drop_box.send(Ok(message.kind));
                        } else {
                            log::debug!("{}It's not a reply, handling!", log_prefix);
                            let message_uuid = message.uuid;
                            match message.kind {
                                InternalMessageKind::UserMessage(user_message) => {
                                    let message_handler = Arc::clone(&message_handler);
                                    let response_sender = response_sender.clone();
                                    tokio::spawn(async move {
                                        if let Some(m) = message_handler(user_message).await {
                                            let r = response_sender.send(InternalMessage {
                                                uuid: message_uuid,
                                                kind: InternalMessageKind::UserMessage(m),
                                            });
                                            if let Err(e) = r {
                                                log::error!("Failed to send reply {e:?}");
                                            }
                                        }
                                    });
                                }
                                #[cfg(feature = "message-schema-validation")]
                                InternalMessageKind::UserMessageSchema(other_schema) => {
                                    let my_schema = schema_for!(U);
                                    let kind = match serde_json::from_str::<RootSchema>(&other_schema) {
                                        Ok(other_schema) => {
                                            if other_schema == my_schema {
                                                InternalMessageKind::UserMessageSchemaOk
                                            } else {
                                                InternalMessageKind::UserMessageSchemaError {
                                                    other_schema: serde_json::to_string(&my_schema).expect("upstream guarantees this won't fail")
                                                }
                                            }
                                        },
                                        Err(_) => {
                                            log::error!("Failed to deserialize incoming schema properly, got {other_schema:?}");
                                            InternalMessageKind::UserMessageSchemaError {
                                                other_schema: serde_json::to_string(&my_schema).expect("upstream guarantees this won't fail")
                                            }
                                        }
                                    };
                                    let r = response_sender.send(InternalMessage {
                                        uuid: message_uuid,
                                        kind,
                                    });
                                    if let Err(e) = r {
                                        log::error!("Failed to send validation response {e:#?}");
                                    }
                                }
                                InternalMessageKind::Hangup => {
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = status_sender.send(ConnectionStatus::DisconnectedCleanly);
}

fn get_log_prefix(is_server: bool) -> String {
    let first_arg = env::args()
        .next()
        .unwrap_or_else(|| String::from("Unknown"));
    let process = Path::new(&first_arg)
        .file_name()
        .unwrap_or_else(|| "Unknown".as_ref())
        .to_string_lossy();
    if is_server {
        format!("{} as Server: ", process)
    } else {
        format!("{} as Client: ", process)
    }
}

/// Reports the status of the connection between the server and the client
#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    /// The server is waiting for a client to connect. A client never waits for a server to connect.
    WaitingForClient,
    /// The connection is active.
    Connected,
    /// A shutdown happened, and this was normal. Nothing unexpected happened.
    DisconnectedCleanly,
    /// An unexpected shutdown occurred, error contained within.
    DisconnectError(IpcRpcError),
}

impl ConnectionStatus {
    pub fn session_end_result(&self) -> Option<Result<(), IpcRpcError>> {
        match self {
            ConnectionStatus::WaitingForClient | ConnectionStatus::Connected => None,
            ConnectionStatus::DisconnectedCleanly => Some(Ok(())),
            ConnectionStatus::DisconnectError(e) => Some(Err(e.clone())),
        }
    }
}

/// This future represents a reply to a previous message. It is not a public type. This is to permit
/// future flexibility in how this interface is implemented.
struct IpcReplyFuture<U: UserMessage> {
    receiver: mpsc::UnboundedReceiver<Result<InternalMessageKind<U>, IpcRpcError>>,
}

impl<U: UserMessage> Future for IpcReplyFuture<U> {
    type Output = Result<InternalMessageKind<U>, IpcRpcError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.receiver.poll_recv(cx).map(|o| match o {
            Some(m) => m,
            None => Err(IpcRpcError::ConnectionDropped),
        })
    }
}

struct IpcReceiveStream<T> {
    receiver: mpsc::UnboundedReceiver<Result<T, IpcError>>,
}

impl<T> IpcReceiveStream<T>
where
    T: Send + for<'de> Deserialize<'de> + Serialize + 'static,
{
    pub fn new(ipc_receiver: IpcReceiver<T>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::task::spawn_blocking(move || loop {
            match ipc_receiver.try_recv_timeout(Duration::from_millis(250)) {
                Ok(msg) => {
                    if sender.send(Ok(msg)).is_err() {
                        break;
                    }
                }
                Err(TryRecvError::IpcError(e)) => {
                    if sender.send(Err(e)).is_err() {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {
                    if sender.is_closed() {
                        break;
                    }
                }
            }
        });
        Self { receiver }
    }
}

impl<T> Stream for IpcReceiveStream<T> {
    type Item = Result<T, IpcError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

/// Reports the outcome of automatic schema validation testing. This testing is performed
/// on connection initiation.
#[derive(Debug, Clone)]
pub enum SchemaValidationStatus {
    /// Crate was compiled without default features, so validation does not function.
    ValidationDisabledAtCompileTime,
    /// Internal error
    ValidationNotPerformedProperly,
    /// Internal error
    ValidationCommunicationFailed(IpcRpcError),
    /// Schemas matched, all clear
    SchemasMatched,
    /// Schemas didn't match, schemas provided for comparison
    SchemaMismatch {
        our_schema: String,
        their_schema: String,
    },
}

impl SchemaValidationStatus {
    /// Returns true if this is an instance of [`Self::SchemasMatched`], and false otherwise.
    pub fn is_success(&self) -> bool {
        matches!(self, SchemaValidationStatus::SchemasMatched)
    }

    /// Panics if this status is not [`Self::SchemasMatched`], and prints the debug information into the panic.
    pub fn assert_success(&self) {
        if !self.is_success() {
            panic!("ipc-rpc user message schema failed to validate, error {self:#?}");
        }
    }
}

/// A combination trait which represents all of the traits needed for a type to be
/// used as a message with this library.
///
/// # Note on `JsonSchema`
/// `JsonSchema` is a trait from the [`schemars`](https://crates.io/crates/schemars) crate.
/// Deriving it for your message allows `ipc-rpc` to perform validation on message schemas
/// after communication has started. The results of this validation are logged, and stored in
/// [IpcRpcClient::schema_validated], [IpcRpcServer::schema_validated], and [IpcRpc::schema_validated].
/// A failed validation will not crash the program.
///
/// Despite what this may imply, `ipc-rpc` does not use JSON for messages internally.
///
/// If you find yourself unable to derive `JsonSchema` then consider turning off
/// default features for the `ipc-rpc` crate. Once you do, the `JsonSchema` requirement
/// will go away.
#[cfg(feature = "message-schema-validation")]
pub trait UserMessage:
    'static + Send + Debug + Clone + DeserializeOwned + Serialize + JsonSchema
{
}

#[cfg(feature = "message-schema-validation")]
impl<T> UserMessage for T where
    T: 'static + Send + Debug + Clone + DeserializeOwned + Serialize + JsonSchema
{
}

/// A combination trait which represents all of the traits needed for a type to be
/// used as a message with this library.
#[cfg(not(feature = "message-schema-validation"))]
pub trait UserMessage: 'static + Send + Debug + Clone + DeserializeOwned + Serialize {}

#[cfg(not(feature = "message-schema-validation"))]
impl<T> UserMessage for T where T: 'static + Send + Debug + Clone + DeserializeOwned + Serialize {}

/// Invokes an RPC call within the current async runtime.
///
/// # Params
/// - `sender`: A reference to an [IpcRpcServer], [IpcRpcClient], or [IpcRpc]
/// - `to_send`: The message sent to the remote
/// - `receiver`: The expected response pattern, followed by a handler for it.
///
/// # Returns
/// The value returned by `receiver`.
///
/// # Panics
/// Panics if the message received from the remote doesn't match the pattern specified
/// in receiver.
///
/// # Example
///
/// ```
/// use serde::{Deserialize, Serialize};
/// use schemars::JsonSchema;
/// use std::fmt::Debug;
///
/// #[derive(Deserialize, Serialize, Debug, Clone, JsonSchema)]
/// enum Message {
///     MakeMeASandwich,
///     /// The sandwiches are made of i32 around here, don't judge.
///     ASandwich(Vec<i32>),
/// }
///
/// // Initialize a client
///
/// # async {
/// # use ipc_rpc::{IpcRpcClient, ConnectionKey, rpc_call};
/// # let mut client = IpcRpcClient::initialize_client(ConnectionKey::from(uuid::Uuid::nil()), |_| async {Option::<Message>::None}).await.unwrap();
/// rpc_call!(
///     sender: client,
///     to_send: Message::MakeMeASandwich,
///     receiver: Message::ASandwich(parts) => {
///         log::info!("I got a sandwich! It contains\n{parts:#?}");
///     },
/// )
/// .unwrap()
/// # };
/// ```
#[macro_export]
macro_rules! rpc_call {
    (sender: $sender:expr, to_send: $to_send:expr, receiver: $received:pat_param => $to_do:block,) => {
        $sender.send($to_send).await.map(|m| match m {
            $received => $to_do,
            _ => panic!("rpc_call response didn't match given pattern"),
        })
    };
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;

    #[cfg(not(feature = "message-schema-validation"))]
    compile_error!("Tests must be executed with all features on");

    #[derive(Deserialize, Serialize, Debug, Clone, JsonSchema)]
    pub struct IpcProtocolMessage {
        pub kind: IpcProtocolMessageKind,
    }

    #[derive(Deserialize, Serialize, Debug, Clone, JsonSchema)]
    pub enum IpcProtocolMessageKind {
        TestMessage,
        ClientTestReply,
        ServerTestReply,
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 3))]
    async fn basic_dialogue() {
        let (server_key, mut server) =
            server::IpcRpcServer::initialize_server(|message: IpcProtocolMessage| async move {
                match message.kind {
                    IpcProtocolMessageKind::TestMessage => Some(IpcProtocolMessage {
                        kind: IpcProtocolMessageKind::ServerTestReply,
                    }),
                    _ => None,
                }
            })
            .await
            .unwrap();
        let mut client = client::IpcRpcClient::initialize_client(
            server_key,
            |message: IpcProtocolMessage| async move {
                match message.kind {
                    IpcProtocolMessageKind::TestMessage => Some(IpcProtocolMessage {
                        kind: IpcProtocolMessageKind::ClientTestReply,
                    }),
                    _ => None,
                }
            },
        )
        .await
        .unwrap();
        server.schema_validated().await.unwrap().assert_success();
        client.schema_validated().await.unwrap().assert_success();
        let client_reply = server
            .send(IpcProtocolMessage {
                kind: IpcProtocolMessageKind::TestMessage,
            })
            .await;
        if !matches!(
            client_reply.as_ref().map(|r| &r.kind),
            Ok(IpcProtocolMessageKind::ClientTestReply)
        ) {
            panic!("client reply was of unexpected type: {:?}", client_reply);
        }
        let server_reply = client
            .send(IpcProtocolMessage {
                kind: IpcProtocolMessageKind::TestMessage,
            })
            .await;
        if !matches!(
            server_reply.as_ref().map(|r| &r.kind),
            Ok(IpcProtocolMessageKind::ServerTestReply)
        ) {
            panic!("server reply was of unexpected type: {:?}", server_reply);
        }
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 3))]
    async fn send_without_await() {
        let (server_success_sender, mut server_success_receiver) = mpsc::unbounded_channel();
        let (client_success_sender, mut client_success_receiver) = mpsc::unbounded_channel();
        let (server_key, mut server) =
            server::IpcRpcServer::initialize_server(move |message: IpcProtocolMessage| {
                let server_success_sender = server_success_sender.clone();
                async move {
                    match message.kind {
                        IpcProtocolMessageKind::TestMessage => {
                            server_success_sender.send(()).unwrap()
                        }
                        _ => {}
                    }
                    None
                }
            })
            .await
            .unwrap();
        let mut client = client::IpcRpcClient::initialize_client(
            server_key,
            move |message: IpcProtocolMessage| {
                let client_success_sender = client_success_sender.clone();
                async move {
                    match message.kind {
                        IpcProtocolMessageKind::TestMessage => {
                            client_success_sender.send(()).unwrap()
                        }
                        _ => {}
                    }
                    None
                }
            },
        )
        .await
        .unwrap();
        server.schema_validated().await.unwrap().assert_success();
        client.schema_validated().await.unwrap().assert_success();
        let _ = server.send(IpcProtocolMessage {
            kind: IpcProtocolMessageKind::TestMessage,
        });
        let _ = client.send(IpcProtocolMessage {
            kind: IpcProtocolMessageKind::TestMessage,
        });
        assert_eq!(
            timeout(Duration::from_secs(3), server_success_receiver.recv()).await,
            Ok(Some(()))
        );
        assert_eq!(
            timeout(Duration::from_secs(3), client_success_receiver.recv()).await,
            Ok(Some(()))
        );
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 3))]
    async fn timeout_test() {
        let (server_key, mut server) =
            server::IpcRpcServer::initialize_server(|message: IpcProtocolMessage| async move {
                match message.kind {
                    _ => None,
                }
            })
            .await
            .unwrap();
        let mut client = client::IpcRpcClient::initialize_client(
            server_key,
            |message: IpcProtocolMessage| async move {
                match message.kind {
                    _ => None,
                }
            },
        )
        .await
        .unwrap();
        server.schema_validated().await.unwrap().assert_success();
        client.schema_validated().await.unwrap().assert_success();
        let client_reply = server
            .send(IpcProtocolMessage {
                kind: IpcProtocolMessageKind::TestMessage,
            })
            .await;
        if !matches!(client_reply, Err(IpcRpcError::ReplyTimeout)) {
            panic!("client reply was of unexpected type: {:?}", client_reply);
        }
        let server_reply = client
            .send(IpcProtocolMessage {
                kind: IpcProtocolMessageKind::TestMessage,
            })
            .await;
        if !matches!(server_reply, Err(IpcRpcError::ReplyTimeout)) {
            panic!("server reply was of unexpected type: {:?}", server_reply);
        }
    }

    // This test checks to see if a task has come to an end, so it must wait for the runtime to drop.
    // Therefore tokio::test is not an option.
    #[test_log::test]
    fn server_disconnect_test() {
        // We use an empty Arc as a resource inside the message handler to prove the message handler
        // was dropped. If the message handler was dropped then the cleanup routines were executed.
        let drop_detector = Arc::new(());
        let drop_detector_clone = drop_detector.clone();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (server_key, server) = server::IpcRpcServer::initialize_server({
                let drop_detector_clone = drop_detector_clone.clone();
                move |message: IpcProtocolMessage| {
                    let drop_detector_clone = drop_detector_clone.clone();
                    async move {
                        match message.kind {
                            _ => {
                                // I don't expect this to ever be called, I just need the closure to take
                                // ownership of the Arc.
                                let _ = drop_detector_clone.clone();
                                None
                            }
                        }
                    }
                }
            })
            .await
            .unwrap();
            let client = client::IpcRpcClient::initialize_client(
                server_key,
                |message: IpcProtocolMessage| async move {
                    match message.kind {
                        _ => None,
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(Arc::strong_count(&drop_detector_clone), 3);
            drop(server);
            client.wait_for_server_to_disconnect().await.unwrap();
        });
        // It's important that the runtime be able to shut down quickly, so we'll assert it
        // shut down within 3 seconds.
        let start_shutdown = Instant::now();
        runtime.shutdown_timeout(Duration::from_secs(5));
        assert!(start_shutdown.elapsed() < Duration::from_secs(3));
        assert_eq!(Arc::strong_count(&drop_detector), 1);
    }

    // This test checks to see if a task has come to an end, so it must wait for the runtime to drop.
    // Therefore tokio::test is not an option.
    #[test_log::test]
    fn client_disconnect_test() {
        use std::time::Instant;

        // We use an empty Arc as a resource inside the message handler to prove the message handler
        // was dropped. If the message handler was dropped then the cleanup routines were executed.
        let drop_detector = Arc::new(());
        let drop_detector_clone = drop_detector.clone();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (server_key, mut server) =
                server::IpcRpcServer::initialize_server(|message: IpcProtocolMessage| async move {
                    match message.kind {
                        _ => None,
                    }
                })
                .await
                .unwrap();
            let client = client::IpcRpcClient::initialize_client(server_key, {
                let drop_detector_clone = drop_detector_clone.clone();
                move |message: IpcProtocolMessage| {
                    let _drop_detector_clone = drop_detector_clone.clone();
                    async move {
                        match message.kind {
                            _ => None,
                        }
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(Arc::strong_count(&drop_detector_clone), 3);
            drop(client);
            server.wait_for_client_to_disconnect().await.unwrap();
        });
        // It's important that the runtime be able to shut down quickly, so we'll assert it
        // shut down within 3 seconds.
        let start_shutdown = Instant::now();
        runtime.shutdown_timeout(Duration::from_secs(5));
        assert!(start_shutdown.elapsed() < Duration::from_secs(3));
        assert_eq!(Arc::strong_count(&drop_detector), 1);
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 3))]
    async fn rpc_call_macro_test() {
        let (server_key, mut server) =
            server::IpcRpcServer::initialize_server(|message: IpcProtocolMessage| async move {
                match message.kind {
                    _ => Some(IpcProtocolMessage {
                        kind: IpcProtocolMessageKind::ServerTestReply,
                    }),
                }
            })
            .await
            .unwrap();
        let mut client = client::IpcRpcClient::initialize_client(
            server_key,
            |message: IpcProtocolMessage| async move {
                match message.kind {
                    _ => Some(IpcProtocolMessage {
                        kind: IpcProtocolMessageKind::ClientTestReply,
                    }),
                }
            },
        )
        .await
        .unwrap();
        server.schema_validated().await.unwrap().assert_success();
        client.schema_validated().await.unwrap().assert_success();
        rpc_call!(
            sender: server,
            to_send: IpcProtocolMessage {
                kind: IpcProtocolMessageKind::TestMessage
            },
            receiver: IpcProtocolMessage {
                kind: IpcProtocolMessageKind::ClientTestReply
            } => {

            },
        )
        .unwrap();
        rpc_call!(
            sender: client,
            to_send: IpcProtocolMessage {
                kind: IpcProtocolMessageKind::TestMessage
            },
            receiver: IpcProtocolMessage {
                kind: IpcProtocolMessageKind::ServerTestReply
            } => {

            },
        )
        .unwrap();
    }
}
