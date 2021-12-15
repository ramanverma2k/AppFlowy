use crate::{
    errors::FlowyError,
    module::DocumentUser,
    services::doc::{
        edit::{EditCommand, EditCommandQueue, OpenDocAction, TransformDeltas},
        revision::{RevisionDownStream, RevisionManager, SteamStopTx},
        DocumentWebSocket,
        WsDocumentHandler,
    },
};
use bytes::Bytes;
use flowy_collaboration::{
    core::document::history::UndoResult,
    entities::{doc::DocDelta, ws::WsDocumentData},
    errors::CollaborateResult,
};
use flowy_database::ConnectionPool;
use flowy_error::{internal_error, FlowyResult};
use lib_infra::retry::{ExponentialBackoff, Retry};
use lib_ot::{
    core::Interval,
    revision::{RevId, RevType, Revision},
    rich_text::{RichTextAttribute, RichTextDelta},
};
use lib_ws::WsConnectState;
use std::sync::Arc;
use tokio::sync::{mpsc, mpsc::UnboundedSender, oneshot};

pub type DocId = String;

pub struct ClientDocEditor {
    pub doc_id: DocId,
    rev_manager: Arc<RevisionManager>,
    edit_cmd_tx: UnboundedSender<EditCommand>,
    ws_sender: Arc<dyn DocumentWebSocket>,
    user: Arc<dyn DocumentUser>,
    ws_msg_tx: UnboundedSender<WsDocumentData>,
    stop_sync_tx: tokio::sync::broadcast::Sender<()>,
}

impl ClientDocEditor {
    pub(crate) async fn new(
        doc_id: &str,
        user: Arc<dyn DocumentUser>,
        pool: Arc<ConnectionPool>,
        mut rev_manager: RevisionManager,
        ws_sender: Arc<dyn DocumentWebSocket>,
    ) -> FlowyResult<Arc<Self>> {
        let delta = rev_manager.load_document().await?;
        let edit_cmd_tx = spawn_edit_queue(doc_id, delta, pool.clone());
        let doc_id = doc_id.to_string();
        let rev_manager = Arc::new(rev_manager);
        let (ws_msg_tx, ws_msg_rx) = mpsc::unbounded_channel();
        let (stop_sync_tx, _) = tokio::sync::broadcast::channel(2);
        let cloned_stop_sync_tx = stop_sync_tx.clone();
        let edit_doc = Arc::new(Self {
            doc_id,
            rev_manager,
            edit_cmd_tx,
            ws_sender,
            user,
            ws_msg_tx,
            stop_sync_tx,
        });

        edit_doc.connect_to_doc();

        start_sync(edit_doc.clone(), ws_msg_rx, cloned_stop_sync_tx);
        Ok(edit_doc)
    }

    pub async fn insert<T: ToString>(&self, index: usize, data: T) -> Result<(), FlowyError> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<RichTextDelta>>();
        let msg = EditCommand::Insert {
            index,
            data: data.to_string(),
            ret,
        };
        let _ = self.edit_cmd_tx.send(msg);
        let delta = rx.await.map_err(internal_error)??;
        let _ = self.save_local_delta(delta).await?;
        Ok(())
    }

    pub async fn delete(&self, interval: Interval) -> Result<(), FlowyError> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<RichTextDelta>>();
        let msg = EditCommand::Delete { interval, ret };
        let _ = self.edit_cmd_tx.send(msg);
        let delta = rx.await.map_err(internal_error)??;
        let _ = self.save_local_delta(delta).await?;
        Ok(())
    }

    pub async fn format(&self, interval: Interval, attribute: RichTextAttribute) -> Result<(), FlowyError> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<RichTextDelta>>();
        let msg = EditCommand::Format {
            interval,
            attribute,
            ret,
        };
        let _ = self.edit_cmd_tx.send(msg);
        let delta = rx.await.map_err(internal_error)??;
        let _ = self.save_local_delta(delta).await?;
        Ok(())
    }

    pub async fn replace<T: ToString>(&self, interval: Interval, data: T) -> Result<(), FlowyError> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<RichTextDelta>>();
        let msg = EditCommand::Replace {
            interval,
            data: data.to_string(),
            ret,
        };
        let _ = self.edit_cmd_tx.send(msg);
        let delta = rx.await.map_err(internal_error)??;
        let _ = self.save_local_delta(delta).await?;
        Ok(())
    }

    pub async fn can_undo(&self) -> bool {
        let (ret, rx) = oneshot::channel::<bool>();
        let msg = EditCommand::CanUndo { ret };
        let _ = self.edit_cmd_tx.send(msg);
        rx.await.unwrap_or(false)
    }

    pub async fn can_redo(&self) -> bool {
        let (ret, rx) = oneshot::channel::<bool>();
        let msg = EditCommand::CanRedo { ret };
        let _ = self.edit_cmd_tx.send(msg);
        rx.await.unwrap_or(false)
    }

    pub async fn undo(&self) -> Result<UndoResult, FlowyError> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<UndoResult>>();
        let msg = EditCommand::Undo { ret };
        let _ = self.edit_cmd_tx.send(msg);
        let r = rx.await.map_err(internal_error)??;
        Ok(r)
    }

    pub async fn redo(&self) -> Result<UndoResult, FlowyError> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<UndoResult>>();
        let msg = EditCommand::Redo { ret };
        let _ = self.edit_cmd_tx.send(msg);
        let r = rx.await.map_err(internal_error)??;
        Ok(r)
    }

    pub async fn delta(&self) -> FlowyResult<DocDelta> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<String>>();
        let msg = EditCommand::ReadDoc { ret };
        let _ = self.edit_cmd_tx.send(msg);
        let data = rx.await.map_err(internal_error)??;

        Ok(DocDelta {
            doc_id: self.doc_id.clone(),
            data,
        })
    }

    async fn save_local_delta(&self, delta: RichTextDelta) -> Result<RevId, FlowyError> {
        let delta_data = delta.to_bytes();
        let (base_rev_id, rev_id) = self.rev_manager.next_rev_id();
        let delta_data = delta_data.to_vec();
        let user_id = self.user.user_id()?;
        let revision = Revision::new(base_rev_id, rev_id, delta_data, &self.doc_id, RevType::Local, user_id);
        let _ = self.rev_manager.add_local_revision(&revision).await?;
        Ok(rev_id.into())
    }

    #[tracing::instrument(level = "debug", skip(self, data), err)]
    pub(crate) async fn composing_local_delta(&self, data: Bytes) -> Result<(), FlowyError> {
        let delta = RichTextDelta::from_bytes(&data)?;
        let (ret, rx) = oneshot::channel::<CollaborateResult<()>>();
        let msg = EditCommand::ComposeDelta {
            delta: delta.clone(),
            ret,
        };
        let _ = self.edit_cmd_tx.send(msg);
        let _ = rx.await.map_err(internal_error)??;

        let _ = self.save_local_delta(delta).await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub fn stop_sync(&self) {
        tracing::debug!("{} stop sync", self.doc_id);
        let _ = self.stop_sync_tx.send(());
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn connect_to_doc(&self) {
        let rev_id: RevId = self.rev_manager.rev_id().into();
        if let Ok(user_id) = self.user.user_id() {
            let action = OpenDocAction::new(&user_id, &self.doc_id, &rev_id, &self.ws_sender);
            let strategy = ExponentialBackoff::from_millis(50).take(3);
            let retry = Retry::spawn(strategy, action);
            tokio::spawn(async move {
                match retry.await {
                    Ok(_) => log::debug!("Notify open doc success"),
                    Err(e) => log::error!("Notify open doc failed: {}", e),
                }
            });
        }
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub(crate) async fn handle_push_rev(&self, bytes: Bytes) -> FlowyResult<()> {
        // Transform the revision
        let (ret, rx) = oneshot::channel::<CollaborateResult<TransformDeltas>>();
        let _ = self.edit_cmd_tx.send(EditCommand::ProcessRemoteRevision { bytes, ret });
        let TransformDeltas {
            client_prime,
            server_prime,
            server_rev_id,
        } = rx.await.map_err(internal_error)??;

        if self.rev_manager.rev_id() >= server_rev_id.value {
            // Ignore this push revision if local_rev_id >= server_rev_id
            return Ok(());
        }

        // compose delta
        let (ret, rx) = oneshot::channel::<CollaborateResult<()>>();
        let msg = EditCommand::ComposeDelta {
            delta: client_prime.clone(),
            ret,
        };
        let _ = self.edit_cmd_tx.send(msg);
        let _ = rx.await.map_err(internal_error)??;

        // update rev id
        self.rev_manager
            .update_rev_id_counter_value(server_rev_id.clone().into());
        let (local_base_rev_id, local_rev_id) = self.rev_manager.next_rev_id();

        // save the revision
        let user_id = self.user.user_id()?;
        let revision = Revision::new(
            local_base_rev_id,
            local_rev_id,
            client_prime.to_bytes().to_vec(),
            &self.doc_id,
            RevType::Remote,
            user_id,
        );
        let _ = self.rev_manager.add_remote_revision(&revision).await?;

        // send the server_prime delta
        let user_id = self.user.user_id()?;
        let revision = Revision::new(
            local_base_rev_id,
            local_rev_id,
            server_prime.to_bytes().to_vec(),
            &self.doc_id,
            RevType::Remote,
            user_id,
        );
        let _ = self.ws_sender.send(revision.into());
        Ok(())
    }

    pub async fn handle_ws_message(&self, doc_data: WsDocumentData) -> FlowyResult<()> {
        match self.ws_msg_tx.send(doc_data) {
            Ok(_) => {},
            Err(e) => tracing::error!("❌Propagate ws message failed. {}", e),
        }
        Ok(())
    }
}

pub struct EditDocWsHandler(pub Arc<ClientDocEditor>);

impl std::ops::Deref for EditDocWsHandler {
    type Target = Arc<ClientDocEditor>;

    fn deref(&self) -> &Self::Target { &self.0 }
}

impl WsDocumentHandler for EditDocWsHandler {
    fn receive(&self, doc_data: WsDocumentData) {
        let edit_doc = self.0.clone();
        tokio::spawn(async move {
            if let Err(e) = edit_doc.handle_ws_message(doc_data).await {
                tracing::error!("❌{:?}", e);
            }
        });
    }

    fn state_changed(&self, state: &WsConnectState) {
        match state {
            WsConnectState::Init => {},
            WsConnectState::Connecting => {},
            WsConnectState::Connected => self.connect_to_doc(),
            WsConnectState::Disconnected => {},
        }
    }
}

fn spawn_edit_queue(doc_id: &str, delta: RichTextDelta, _pool: Arc<ConnectionPool>) -> UnboundedSender<EditCommand> {
    let (sender, receiver) = mpsc::unbounded_channel::<EditCommand>();
    let actor = EditCommandQueue::new(doc_id, delta, receiver);
    tokio::spawn(actor.run());
    sender
}

fn start_sync(
    editor: Arc<ClientDocEditor>,
    ws_msg_rx: mpsc::UnboundedReceiver<WsDocumentData>,
    stop_sync_tx: SteamStopTx,
) {
    let rev_manager = editor.rev_manager.clone();
    let ws_sender = editor.ws_sender.clone();

    let up_stream = editor.rev_manager.make_up_stream(stop_sync_tx.subscribe());
    let down_stream = RevisionDownStream::new(editor, rev_manager, ws_msg_rx, ws_sender, stop_sync_tx.subscribe());

    tokio::spawn(up_stream.run());
    tokio::spawn(down_stream.run());
}

#[cfg(feature = "flowy_unit_test")]
impl ClientDocEditor {
    pub async fn doc_json(&self) -> FlowyResult<String> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<String>>();
        let msg = EditCommand::ReadDoc { ret };
        let _ = self.edit_cmd_tx.send(msg);
        let s = rx.await.map_err(internal_error)??;
        Ok(s)
    }

    pub async fn doc_delta(&self) -> FlowyResult<RichTextDelta> {
        let (ret, rx) = oneshot::channel::<CollaborateResult<RichTextDelta>>();
        let msg = EditCommand::ReadDocDelta { ret };
        let _ = self.edit_cmd_tx.send(msg);
        let delta = rx.await.map_err(internal_error)??;
        Ok(delta)
    }

    pub fn rev_manager(&self) -> Arc<RevisionManager> { self.rev_manager.clone() }
}
