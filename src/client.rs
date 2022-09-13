use std::{future::Future, sync::Arc};

use crate::{
    get_log_prefix, process_incoming_mail, ConnectionKey, ConnectionStatus, InternalMessage,
    InternalMessageKind, IpcReceiveStream, IpcReplyFuture, IpcRpcError, PendingReplyEntry,
    SchemaValidationStatus, UserMessage,
};
use ipc_channel::ipc::{self, IpcSender};
use tokio::{
    sync::{mpsc, watch},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[cfg(feature = "message-schema-validation")]
use schemars::{schema::RootSchema, schema_for};

/// Used to send messages to the connected server.
#[derive(Debug, Clone)]
pub struct IpcRpcClient<U: UserMessage> {
    ipc_sender: IpcSender<InternalMessage<U>>,
    pending_reply_sender: mpsc::UnboundedSender<PendingReplyEntry<U>>,
    status_receiver: watch::Receiver<ConnectionStatus>,
    #[cfg(feature = "message-schema-validation")]
    validation_receiver: watch::Receiver<Option<SchemaValidationStatus>>,
    log_prefix: String,
    ref_count: Option<Arc<()>>,
}

impl<U: UserMessage> IpcRpcClient<U> {
    /// Initializes a client connected to the server which was paired with the given [`ConnectionKey`].
    pub async fn initialize_client<F, Fut>(
        key: ConnectionKey,
        message_handler: F,
    ) -> Result<IpcRpcClient<U>, IpcRpcError>
    where
        F: Fn(U) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<U>> + Send,
    {
        let ipc_sender = IpcSender::connect(key.to_string())?;
        let (sender, receiver) = ipc::channel::<InternalMessage<U>>()?;
        ipc_sender.send(InternalMessage {
            uuid: Uuid::new_v4(),
            kind: InternalMessageKind::InitConnection(sender),
        })?;
        let ipc_sender_clone = ipc_sender.clone();
        let (pending_reply_sender, pending_reply_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (status_sender, status_receiver) = watch::channel(ConnectionStatus::Connected);
        #[cfg(feature = "message-schema-validation")]
        let (validation_sender, validation_receiver) = watch::channel(None);
        tokio::task::spawn(async move {
            process_incoming_mail(
                false,
                pending_reply_receiver,
                IpcReceiveStream::new(receiver),
                message_handler,
                ipc_sender_clone,
                status_sender,
            )
            .await;
        });
        let log_prefix = get_log_prefix(false);
        log::info!("{}Client initialized!", log_prefix);
        let ret = IpcRpcClient {
            ipc_sender,
            pending_reply_sender,
            status_receiver,
            #[cfg(feature = "message-schema-validation")]
            validation_receiver,
            log_prefix,
            ref_count: Some(Arc::new(())),
        };
        #[cfg(feature = "message-schema-validation")]
        {
            let reply_future = ret.internal_send(
                InternalMessageKind::UserMessageSchema(
                    serde_json::to_string(&schema_for!(U))
                        .expect("upstream guarantees this won't fail"),
                ),
                crate::DEFAULT_REPLY_TIMEOUT,
            );
            tokio::spawn(async move {
                match reply_future.await {
                    Ok(InternalMessageKind::UserMessageSchemaOk) => {
                        log::info!("Remote server validated user message schema");
                        if let Err(e) =
                            validation_sender.send(Some(SchemaValidationStatus::SchemasMatched))
                        {
                            log::error!("Failed to set validation_status {e:#?}");
                        }
                    }
                    Ok(InternalMessageKind::UserMessageSchemaError { other_schema }) => {
                        let my_schema = schema_for!(U);
                        let res =
                            validation_sender.send(Some(SchemaValidationStatus::SchemaMismatch {
                                our_schema: serde_json::to_string(&my_schema)
                                    .expect("upstream guarantees this won't fail"),
                                their_schema: other_schema.clone(),
                            }));
                        if let Err(e) = res {
                            log::error!("Failed to set validation_status {e:#?}");
                        }
                        match serde_json::from_str::<RootSchema>(&other_schema) {
                            Ok(other_schema) => {
                                if other_schema == my_schema {
                                    log::error!("Server failed validation on user message schema, but the schemas match. This is probably a bug in ipc-rpc.");
                                } else {
                                    log::error!("Failed to validate that user messages have the same schema. Messages may fail to serialize and deserialize correctly. This is a serious problem.\nClient Schema {my_schema:#?}\nServer Schema {other_schema:#?}");
                                }
                            }
                            Err(_) => {
                                log::error!("Server failed validation on user schema, and we failed to deserialize incoming schema properly, got {other_schema:?}");
                            }
                        }
                    }
                    Ok(m) => {
                        log::error!("Unexpected reply for user message schema validation {m:#?}");
                        if let Err(e) = validation_sender
                            .send(Some(SchemaValidationStatus::ValidationNotPerformedProperly))
                        {
                            log::error!("Failed to set validation_status {e:#?}");
                        }
                    }
                    Err(IpcRpcError::ConnectionDropped) => {
                        // Do nothing, connection was dropped before validation completed.
                    }
                    Err(e) => {
                        log::error!("Failed to validate user message schema, messages may fail to serialize and deserialize correctly. Was the server compiled without the message-schema-validation feature? {e:#?}");
                        if let Err(e) = validation_sender.send(Some(
                            SchemaValidationStatus::ValidationCommunicationFailed(e),
                        )) {
                            log::error!("Failed to set validation_status {e:#?}");
                        }
                    }
                }
            });
        }
        Ok(ret)
    }

    fn internal_send(
        &self,
        message_kind: InternalMessageKind<U>,
        timeout: Duration,
    ) -> impl Future<Output = Result<InternalMessageKind<U>, IpcRpcError>> + Send + 'static {
        let (sender, receiver) = mpsc::unbounded_channel();
        let message = InternalMessage {
            uuid: Uuid::new_v4(),
            kind: message_kind,
        };
        if let Err(e) = self
            .pending_reply_sender
            .send((message.uuid, (sender, Instant::now() + timeout)))
        {
            log::error!("Failed to send entry for reply drop box {:?}", e);
        }
        let result = self.ipc_sender.send(message);
        async move {
            result?;
            IpcReplyFuture { receiver }.await
        }
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

    /// Sends a message, will give up on receiving a reply after the [`DEFAULT_REPLY_TIMEOUT`](./constant.DEFAULT_REPLY_TIMEMOUT.html) has passed.
    pub fn send(
        &self,
        user_message: U,
    ) -> impl Future<Output = Result<U, IpcRpcError>> + Send + 'static {
        self.send_timeout(user_message, crate::DEFAULT_REPLY_TIMEOUT)
    }

    pub fn wait_for_server_to_disconnect(
        &self,
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

impl<U: UserMessage> Drop for IpcRpcClient<U> {
    fn drop(&mut self) {
        if Arc::try_unwrap(self.ref_count.take().unwrap()).is_ok() {
            if let Err(e) = self.ipc_sender.send(InternalMessage {
                uuid: Uuid::new_v4(),
                kind: InternalMessageKind::Hangup,
            }) {
                log::error!(
                    "{}Error sending hangup message to server: {:?}",
                    self.log_prefix,
                    e
                );
            }
        }
    }
}
