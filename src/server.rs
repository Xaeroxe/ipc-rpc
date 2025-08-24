use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::Arc,
    thread,
};

use ipc_channel::ipc::{IpcOneShotServer, IpcSender};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, watch},
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{
    get_log_prefix, ConnectionStatus, IpcReceiveStream, IpcReplyFuture, IpcRpcError,
    PendingReplyEntry, SchemaValidationStatus, UserMessage,
};

#[cfg(feature = "message-schema-validation")]
use schemars::{schema_for, Schema};

use super::{ConnectionKey, InternalMessage, InternalMessageKind};

/// Passes messages along from the internal mpsc channel to the actual IPC channel. This is necessary
/// because the IPC channel is not available at the time the server is initialized. That channel
/// only becomes available once a connection is established.
async fn process_outgoing_server_mail<U: UserMessage>(
    mut internal_receiver: mpsc::UnboundedReceiver<InternalMessage<U>>,
    ipc_sender: IpcSender<InternalMessage<U>>,
    log_prefix: Arc<str>,
) {
    log::info!("{}Processing outgoing server mail!", log_prefix);
    while let Some(message) = internal_receiver.recv().await {
        if let Err(e) = ipc_sender.send(message) {
            log::error!("{}Failed to send message to client: {:?}", log_prefix, e);
        }
    }
    log::info!("{}Exiting outgoing server mail loop", log_prefix)
}

/// Used to send messages to the connected client (assuming one does connect)
#[derive(Debug, Clone)]
pub struct IpcRpcServer<U: UserMessage> {
    sender: mpsc::UnboundedSender<InternalMessage<U>>,
    status_receiver: watch::Receiver<ConnectionStatus>,
    #[cfg(feature = "message-schema-validation")]
    validation_receiver: watch::Receiver<Option<SchemaValidationStatus>>,
    pending_reply_sender: mpsc::UnboundedSender<PendingReplyEntry<U>>,
    log_prefix: Arc<str>,
}

/// Initializes a server and owns the connected client. This structure is the
/// preferred way for servers to manage a relationship with a client.
#[derive(Debug)]
pub struct IpcRpc<U: UserMessage> {
    pub server: IpcRpcServer<U>,
    client_process: Child,
}

impl<U: UserMessage> IpcRpcServer<U> {
    /// Initializes a server and provides a [`ConnectionKey`] which clients can use to connect to it,
    /// even out of process.
    pub async fn initialize_server<F, Fut>(
        message_handler: F,
    ) -> Result<(ConnectionKey, IpcRpcServer<U>), IpcRpcError>
    where
        F: Fn(U) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<U>> + Send,
    {
        let (server, server_name) = IpcOneShotServer::<InternalMessage<U>>::new()?;
        let (internal_sender, internal_receiver) = mpsc::unbounded_channel::<InternalMessage<U>>();
        let runtime = tokio::runtime::Handle::current();
        let (status_sender, status_receiver) = watch::channel(ConnectionStatus::WaitingForClient);
        #[cfg(feature = "message-schema-validation")]
        let (validation_sender, validation_receiver) = watch::channel(None);
        let log_prefix = Arc::from(get_log_prefix(true));
        let (pending_reply_sender, pending_reply_reciever) = mpsc::unbounded_channel();
        let pending_reply_sender_clone = pending_reply_sender.clone();
        log::info!("{}Server initialized!", log_prefix);
        thread::spawn({
            let log_prefix = Arc::clone(&log_prefix);
            move || {
                Self::startup(
                    server,
                    internal_receiver,
                    pending_reply_sender_clone,
                    pending_reply_reciever,
                    message_handler,
                    status_sender,
                    #[cfg(feature = "message-schema-validation")]
                    validation_sender,
                    log_prefix,
                    runtime,
                )
            }
        });

        Ok((
            ConnectionKey::from_str(&server_name).expect("server_name is always uuid"),
            IpcRpcServer {
                sender: internal_sender,
                pending_reply_sender,
                status_receiver,
                #[cfg(feature = "message-schema-validation")]
                validation_receiver,
                log_prefix,
            },
        ))
    }

    /// Responsible for initializing the various futures and threads needed to run the server.
    ///
    /// This may block.
    // Reducing argument count here doesn't help with code clarity much.
    #[allow(clippy::too_many_arguments)]
    fn startup<Fut: Future<Output = Option<U>> + Send, F: Fn(U) -> Fut + Send + Sync + 'static>(
        server: IpcOneShotServer<InternalMessage<U>>,
        internal_receiver: mpsc::UnboundedReceiver<InternalMessage<U>>,
        #[allow(unused)] pending_reply_sender: mpsc::UnboundedSender<PendingReplyEntry<U>>,
        pending_reply_receiver: mpsc::UnboundedReceiver<PendingReplyEntry<U>>,
        message_handler: F,
        status_sender: watch::Sender<ConnectionStatus>,
        #[cfg(feature = "message-schema-validation")] validation_sender: watch::Sender<
            Option<SchemaValidationStatus>,
        >,
        log_prefix: Arc<str>,
        runtime: tokio::runtime::Handle,
    ) {
        // This will block. Luckily it's okay to do that here.
        let new_client = server.accept();
        // Now that we're no longer blocking, enter the tokio runtime.
        let _handle = runtime.enter();
        match new_client {
            Err(e) => {
                log::error!("{}Error opening connection to client {:?}", log_prefix, e);
                let e = IpcRpcError::from(e);
                let _ = status_sender.send(ConnectionStatus::DisconnectError(e));
            }
            Ok((receive_from_client, first_message)) => {
                if let InternalMessageKind::InitConnection(ipc_sender) = first_message.kind {
                    let _ = status_sender.send(ConnectionStatus::Connected);
                    log::info!("{}Connection established!", log_prefix);
                    #[cfg(feature = "message-schema-validation")]
                    {
                        let (sender, receiver) = mpsc::unbounded_channel();
                        let message = InternalMessage {
                            uuid: Uuid::new_v4(),
                            kind: InternalMessageKind::UserMessageSchema(
                                serde_json::to_string(&schema_for!(U))
                                    .expect("upstream guarantees this won't fail"),
                            ),
                        };
                        if let Err(e) = pending_reply_sender.send((
                            message.uuid,
                            (sender, Instant::now() + crate::DEFAULT_REPLY_TIMEOUT),
                        )) {
                            log::error!("Failed to send entry for reply drop box {:?}", e);
                        }
                        match ipc_sender.send(message) {
                            Ok(()) => {
                                let reply_future = IpcReplyFuture { receiver };
                                tokio::spawn(async move {
                                    match reply_future.await {
                                        Ok(InternalMessageKind::UserMessageSchemaOk) => {
                                            log::info!(
                                                "Remote client validated user message schema"
                                            );
                                            if let Err(e) = validation_sender
                                                .send(Some(SchemaValidationStatus::SchemasMatched))
                                            {
                                                log::error!(
                                                    "Failed to set validation_status {e:#?}"
                                                );
                                            }
                                        }
                                        Ok(InternalMessageKind::UserMessageSchemaError {
                                            other_schema,
                                        }) => {
                                            let my_schema = schema_for!(U);
                                            let res = validation_sender.send(Some(
                                                SchemaValidationStatus::SchemaMismatch {
                                                    our_schema: serde_json::to_string(&my_schema)
                                                        .expect(
                                                            "upstream guarantees this won't fail",
                                                        ),
                                                    their_schema: other_schema.clone(),
                                                },
                                            ));
                                            if let Err(e) = res {
                                                log::error!(
                                                    "Failed to set validation_status {e:#?}"
                                                );
                                            }
                                            match serde_json::from_str::<Schema>(&other_schema) {
                                                Ok(other_schema) => {
                                                    if other_schema == my_schema {
                                                        log::error!("Client failed validation on user message schema, but the schemas match. This is probably a bug in ipc-rpc.");
                                                    } else {
                                                        log::error!("Failed to validate that user messages have the same schema. Messages may fail to serialize and deserialize correctly. This is a serious problem.\nServer Schema {my_schema:#?}\nClient Schema {other_schema:#?}")
                                                    }
                                                }
                                                Err(_) => {
                                                    log::error!("Server failed validation on user schema, and we failed to deserialize incoming schema properly, got {other_schema:?}");
                                                }
                                            }
                                        }
                                        Ok(m) => {
                                            log::error!("Unexpected reply for user message schema validation {m:#?}");
                                            if let Err(e) = validation_sender.send(Some(SchemaValidationStatus::ValidationNotPerformedProperly)) {
                                                log::error!("Failed to set validation_status {e:#?}");
                                            }
                                        }
                                        Err(IpcRpcError::ConnectionDropped) => {
                                            // Do nothing, connection was dropped before validation completed.
                                        }
                                        Err(e) => {
                                            log::error!("Failed to validate user message schema, messages may fail to serialize and deserialize correctly. Was the client compiled without the message-schema-validation feature? {e:#?}");
                                            if let Err(e) = validation_sender.send(Some(SchemaValidationStatus::ValidationCommunicationFailed(e))) {
                                                log::error!("Failed to set validation_status {e:#?}");
                                            }
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                log::error!("Failed to send validation request to client {e:#?}");
                            }
                        }
                    }
                    // We need to run this outside of the tokio runtime worker pool, but still using
                    // the tokio runtime so that it continues to run correctly while tokio is shutting
                    // down.
                    thread::Builder::new()
                        .name("outgoing_mail".to_string())
                        .spawn({
                            let ipc_sender = ipc_sender.clone();
                            let runtime = runtime.clone();
                            move || {
                                runtime.block_on(process_outgoing_server_mail(
                                    internal_receiver,
                                    ipc_sender,
                                    log_prefix,
                                ));
                            }
                        })
                        .unwrap();
                    runtime.spawn(async move {
                        // This will only return when the client disconnects or some other kind of error happens.
                        crate::process_incoming_mail(
                            true,
                            pending_reply_receiver,
                            IpcReceiveStream::new(receive_from_client),
                            message_handler,
                            ipc_sender,
                            status_sender,
                        )
                        .await;
                    });
                } else {
                    log::error!("{}First message received was not an InitConnection message. Dropping connection.", log_prefix);
                    let _ = status_sender.send(ConnectionStatus::DisconnectError(
                        IpcRpcError::HandshakeFailure,
                    ));
                }
            }
        }
    }

    fn internal_send(
        &self,
        message_kind: InternalMessageKind<U>,
        timeout: Duration,
    ) -> impl Future<Output = Result<InternalMessageKind<U>, IpcRpcError>> + Send + 'static {
        let message = InternalMessage {
            uuid: Uuid::new_v4(),
            kind: message_kind,
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Err(e) = self
            .pending_reply_sender
            .send((message.uuid, (sender, Instant::now() + timeout)))
        {
            log::error!("Failed to send entry for reply drop box {:?}", e);
        }
        self.sender.send(message).unwrap();
        IpcReplyFuture { receiver }
    }

    /// Sends a message, will give up on receiving a reply after the [`DEFAULT_REPLY_TIMEOUT`](./constant.DEFAULT_REPLY_TIMEOUT.html) has passed.
    pub fn send(
        &self,
        user_message: U,
    ) -> impl Future<Output = Result<U, IpcRpcError>> + Send + 'static {
        self.send_timeout(user_message, crate::DEFAULT_REPLY_TIMEOUT)
    }

    /// Sends a message, waiting the given `timeout` for a reply.
    pub fn send_timeout(
        &self,
        user_message: U,
        timeout: Duration,
    ) -> impl Future<Output = Result<U, IpcRpcError>> + Send + 'static {
        let send_fut = self.internal_send(InternalMessageKind::UserMessage(user_message), timeout);
        async move {
            send_fut.await.map(|m| match m {
                InternalMessageKind::UserMessage(m) => m,
                _ => panic!(
                    "Got a non-user message reply to a user message. This is a bug in ipc-rpc."
                ),
            })
        }
    }

    pub fn client_connected(&self) -> bool {
        matches!(*self.status_receiver.borrow(), ConnectionStatus::Connected)
    }

    pub fn wait_for_client_to_connect(
        &mut self,
    ) -> impl Future<Output = Result<(), IpcRpcError>> + Send + 'static {
        let mut status_receiver = self.status_receiver.clone();
        async move {
            loop {
                // Put the borrow in an inner scope to make sure we only hold it for short
                // durations.
                {
                    let borrow = status_receiver.borrow();
                    if let ConnectionStatus::Connected = *borrow {
                        return Ok(());
                    }
                    if let Some(r) = borrow.session_end_result() {
                        return r;
                    }
                }
                if status_receiver.changed().await.is_err() {
                    return Err(IpcRpcError::ConnectionDropped);
                }
            }
        }
    }

    pub fn wait_for_client_to_disconnect(
        &mut self,
    ) -> impl Future<Output = Result<(), IpcRpcError>> + Send + 'static {
        let mut status_receiver = self.status_receiver.clone();
        async move {
            // Has the session already ended?
            if let Some(r) = status_receiver.borrow().session_end_result() {
                return r;
            }
            // If not, wait for the session to end.
            loop {
                if status_receiver.changed().await.is_err() {
                    return Err(IpcRpcError::ConnectionDropped);
                }
                if let Some(r) = status_receiver.borrow().session_end_result() {
                    return r;
                }
            }
        }
    }

    /// Returns the outcome of automatic schema validation testing. This testing is performed
    /// on connection initiation.
    pub async fn schema_validated(&mut self) -> Result<SchemaValidationStatus, IpcRpcError> {
        #[cfg(not(feature = "message-schema-validation"))]
        {
            Ok(SchemaValidationStatus::ValidationDisabledAtCompileTime)
        }
        #[cfg(feature = "message-schema-validation")]
        {
            if self.validation_receiver.borrow_and_update().is_none() {
                self.validation_receiver
                    .changed()
                    .await
                    .map_err(|_| IpcRpcError::ConnectionDropped)?;
            }
            Ok(self
                .validation_receiver
                .borrow()
                .as_ref()
                .expect("the prior guaranteed this isn't empty")
                .clone())
        }
    }
}

impl<U: UserMessage> IpcRpc<U> {
    pub fn build() -> IpcRpcBuilder<U> {
        IpcRpcBuilder::new()
    }

    /// Initializes a server and client, connects the two, then returns a combination structure
    /// which can be used for the server side of the relationship.
    ///
    /// # Params
    ///
    /// - path_to_exe: The path to the exe which is expected to connect to the server on startup
    /// - message_handler:  A function for handling spontaneous messages from the new client
    /// - arguments_fn: This method **MUST** provide the server connect key to the client. The easiest way
    ///   to do this is to pass in the key as a command line argument. The client must be
    ///   prepared to read the key from wherever this function puts it.
    /// - env_vars: Additional environment variables to pass in to the new client
    /// - current_dir: The current directory of the client on startup. If not specified, this will default
    ///   to the current directory of the server process.
    ///   (The caller of `initialize_server_with_client` is the server process.)
    async fn initialize_server_with_client<SE, F, Fut, A, ENVS, SK, SV>(
        path_to_exe: SE,
        message_handler: F,
        arguments_fn: A,
        env_vars: ENVS,
        current_dir: Option<&Path>,
    ) -> Result<IpcRpc<U>, IpcRpcError>
    where
        SE: AsRef<OsStr>,
        F: Fn(U) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<U>> + Send,
        A: FnOnce(ConnectionKey, &mut Command),
        ENVS: IntoIterator<Item = (SK, SV)>,
        SK: AsRef<OsStr>,
        SV: AsRef<OsStr>,
    {
        let (server_connect_key, mut server) =
            IpcRpcServer::initialize_server(message_handler).await?;
        log::info!(
            "Starting {} in dir {:?}",
            path_to_exe.as_ref().to_string_lossy(),
            current_dir.as_ref().map(|p| p.display()),
        );
        let mut command = Command::new(path_to_exe);
        command
            .stdin(Stdio::piped())
            .envs(env_vars)
            .kill_on_drop(true);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        arguments_fn(server_connect_key, &mut command);
        let client_process = command.spawn()?;
        match client_process.id() {
            Some(pid) => {
                log::info!("Started client with PID {}", pid);
            }
            None => {
                log::error!("Started a client but it exited immediately")
            }
        }
        server.wait_for_client_to_connect().await.unwrap();
        Ok(IpcRpc {
            server,
            client_process,
        })
    }

    /// Sends a message, will give up on receiving a reply after the [`DEFAULT_REPLY_TIMEOUT`](./constant.DEFAULT_REPLY_TIMEOUT.html) has passed.
    pub fn send(
        &self,
        user_message: U,
    ) -> impl Future<Output = Result<U, IpcRpcError>> + Send + 'static {
        self.server.send(user_message)
    }

    /// Sends a message, waiting the given `timeout` for a reply.
    pub fn send_timeout(
        &self,
        user_message: U,
        timeout: Duration,
    ) -> impl Future<Output = Result<U, IpcRpcError>> + Send + 'static {
        self.server.send_timeout(user_message, timeout)
    }

    /// Returns the outcome of automatic schema validation testing. This testing is performed
    /// on connection initiation.
    pub async fn schema_validated(&mut self) -> Result<SchemaValidationStatus, IpcRpcError> {
        self.server.schema_validated().await
    }
}

impl<U: UserMessage> Drop for IpcRpc<U> {
    fn drop(&mut self) {
        if let Ok(Some(status)) = self.client_process.try_wait() {
            if !status.success() {
                log::error!(
                    "{}Child process exited unsuccessfully, code: {:?}",
                    self.server.log_prefix,
                    status.code()
                )
            }
        } else {
            log::info!("{}Child process still running", self.server.log_prefix);
        }
    }
}

impl<U: UserMessage> Drop for IpcRpcServer<U> {
    fn drop(&mut self) {
        if let Err(e) = self.sender.send(InternalMessage {
            uuid: Uuid::new_v4(),
            kind: InternalMessageKind::Hangup,
        }) {
            log::error!(
                "{}Error sending hangup message to client: {:?}",
                self.log_prefix,
                e
            );
        }
    }
}

/// Builds an [IpcRpc]. This is initialized via [IpcRpc::build()]
#[derive(Debug, Clone)]
pub struct IpcRpcBuilder<U: UserMessage> {
    current_dir: Option<PathBuf>,
    env_vars: HashMap<OsString, OsString>,
    phantom: PhantomData<U>,
}

impl<U: UserMessage> IpcRpcBuilder<U> {
    fn new() -> Self {
        Self {
            current_dir: None,
            env_vars: HashMap::new(),
            phantom: PhantomData,
        }
    }

    /// Additional environment variable to pass in to the new client.
    pub fn env<K: Into<OsString>, V: Into<OsString>>(&mut self, key: K, value: V) -> &mut Self {
        self.env_vars.insert(key.into(), value.into());
        self
    }

    /// Additional environment variables to pass in to the new client.
    pub fn envs<K: Into<OsString>, V: Into<OsString>, I: Iterator<Item = (K, V)>>(
        &mut self,
        iter: I,
    ) -> &mut Self {
        self.env_vars
            .extend(iter.map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Sets the current directory for the new client process
    pub fn current_dir<P: Into<PathBuf>>(&mut self, path: P) -> &mut Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Initializes a server and client, connects the two, then returns a combination structure
    /// which can be used for the server side of the relationship.
    ///
    ///
    /// - path_to_exe: The path to the exe which is expected to connect to the server on startup
    /// - message_handler:  A function for handling spontaneous messages from the new client
    /// - arguments_fn: This method **MUST** provide the server connect key to the client. The easiest way
    ///   to do this is to pass in the key as a command line argument. The client must be
    ///   prepared to read the key from wherever this function puts it.
    pub async fn finish<SE, F, Fut, A>(
        &mut self,
        path_to_exe: SE,
        message_handler: F,
        arguments_fn: A,
    ) -> Result<IpcRpc<U>, IpcRpcError>
    where
        SE: AsRef<OsStr>,
        F: Fn(U) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<U>> + Send,
        A: FnOnce(ConnectionKey, &mut Command),
    {
        IpcRpc::initialize_server_with_client(
            path_to_exe,
            message_handler,
            arguments_fn,
            self.env_vars
                .iter()
                .map(|(k, v)| (k.as_os_str(), v.as_os_str())),
            self.current_dir.as_deref(),
        )
        .await
    }
}

impl<U: UserMessage> Default for IpcRpcBuilder<U> {
    fn default() -> Self {
        Self::new()
    }
}
