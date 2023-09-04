use std::{collections::HashMap, fmt::Debug, io, sync::Arc, time::Duration};

use clippy_utilities::NumericCast;
use curp_external_api::cmd::{PbSerializeError, ProposeId};
use engine::SnapshotApi;
use event_listener::Event;
use futures::{pin_mut, stream::FuturesUnordered, Stream, StreamExt};
use madsim::rand::{thread_rng, Rng};
use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::MissedTickBehavior,
};
use tracing::{debug, error, info, trace, warn};
use utils::config::CurpConfig;

use super::{
    cmd_board::{CmdBoardRef, CommandBoard},
    cmd_worker::{conflict_checked_mpmc, start_cmd_workers},
    gc::run_gc_tasks,
    raw_curp::{AppendEntries, RawCurp, UncommittedPool, Vote},
    spec_pool::{SpecPoolRef, SpeculativePool},
    storage::{StorageApi, StorageError},
};
use crate::{
    cmd::{Command, CommandExecutor},
    error::RpcError,
    log_entry::LogEntry,
    members::{ClusterInfo, ServerId},
    role_change::RoleChange,
    rpc::{
        self, connect::ConnectApi, AppendEntriesRequest, AppendEntriesResponse,
        FetchClusterRequest, FetchClusterResponse, FetchLeaderRequest, FetchLeaderResponse,
        FetchReadStateRequest, FetchReadStateResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, ProposeConfChangeRequest, ProposeConfChangeResponse,
        ProposeRequest, ProposeResponse, VoteRequest, VoteResponse, WaitSyncedRequest,
        WaitSyncedResponse,
    },
    server::{cmd_worker::CEEventTxApi, raw_curp::SyncAction, storage::db::DB},
    snapshot::{Snapshot, SnapshotMeta},
    ConfChangeType, SnapshotAllocator,
};

/// Curp error
#[derive(Debug, Error)]
pub(super) enum CurpError {
    /// Encode or decode error
    #[error("encode or decode error")]
    EncodeDecode(String),
    /// Storage error
    #[error("storage error, {0}")]
    Storage(#[from] StorageError),
    /// Transport error
    #[error("transport error, {0}")]
    Transport(String),
    /// Io error
    #[error("io error, {0}")]
    IO(#[from] io::Error),
    /// Internal error
    #[error("internal error, {0}")]
    Internal(String),
}

impl From<bincode::Error> for CurpError {
    fn from(err: bincode::Error) -> Self {
        Self::EncodeDecode(err.to_string())
    }
}
impl From<PbSerializeError> for CurpError {
    fn from(err: PbSerializeError) -> Self {
        Self::EncodeDecode(err.to_string())
    }
}

/// Internal error encountered when sending `append_entries`
#[derive(Debug, Error)]
enum SendAEError {
    /// When self is no longer leader
    #[error("self is no longer leader")]
    NotLeader,
    /// When the follower rejects
    #[error("follower rejected")]
    Rejected,
    /// Transport
    #[error("transport error, {0}")]
    Transport(#[from] RpcError),
    /// Encode/Decode error
    #[error("encode or decode error")]
    EncodeDecode(#[from] bincode::Error),
}

/// Internal error encountered when sending snapshot
#[derive(Debug, Error)]
enum SendSnapshotError {
    /// When self is no longer leader
    #[error("self is no longer leader")]
    NotLeader,
    /// Transport
    #[error("transport error, {0}")]
    Transport(#[from] RpcError),
}

/// `CurpNode` represents a single node of curp cluster
pub(super) struct CurpNode<C: Command, RC: RoleChange> {
    /// `RawCurp` state machine
    curp: Arc<RawCurp<C, RC>>,
    /// The speculative cmd pool, shared with executor
    spec_pool: SpecPoolRef<C>,
    /// Cmd watch board for tracking the cmd sync results
    cmd_board: CmdBoardRef<C>,
    /// Shutdown trigger
    shutdown_trigger: Arc<Event>,
    /// CE event tx,
    ce_event_tx: Arc<dyn CEEventTxApi<C>>,
    /// Storage
    storage: Arc<dyn StorageApi<Command = C>>,
    /// Snapshot allocator
    snapshot_allocator: Box<dyn SnapshotAllocator>,
}

// handlers
impl<C: 'static + Command, RC: RoleChange + 'static> CurpNode<C, RC> {
    /// Handle `Propose` requests
    pub(super) async fn propose(&self, req: ProposeRequest) -> Result<ProposeResponse, CurpError> {
        let cmd: Arc<C> = Arc::new(req.cmd()?);

        // handle proposal
        let ((leader_id, term), result) = self.curp.handle_propose(Arc::clone(&cmd));
        let resp = match result {
            Ok(true) => {
                let er_res = CommandBoard::wait_for_er(&self.cmd_board, cmd.id()).await;
                ProposeResponse::new_result::<C>(leader_id, term, &er_res)
            }
            Ok(false) => ProposeResponse::new_empty(leader_id, term),
            Err(err) => ProposeResponse::new_error(leader_id, term, &err),
        };

        Ok(resp)
    }

    /// Handle `ProposeConfChange` requests
    pub(super) async fn propose_conf_change(
        &self,
        req: ProposeConfChangeRequest,
    ) -> Result<ProposeConfChangeResponse, CurpError> {
        let id = ProposeId::new(req.id.clone());
        let ((_leader_id, _term), _result) = self.curp.handle_propose_conf_change(req.into());
        let result = CommandBoard::wait_for_conf(&self.cmd_board, &id).await;
        #[allow(clippy::as_conversions)] // safe conversions
        let error = match result {
            Ok(_) => None,
            Err(e) => Some(e as i32),
        };
        let members = self.curp.cluster().members();
        Ok(ProposeConfChangeResponse { error, members })
    }

    /// Handle `AppendEntries` requests
    pub(super) fn append_entries(
        &self,
        req: &AppendEntriesRequest,
    ) -> Result<AppendEntriesResponse, CurpError> {
        let entries = req.entries()?;

        let result = self.curp.handle_append_entries(
            req.term,
            req.leader_id,
            req.prev_log_index,
            req.prev_log_term,
            entries,
            req.leader_commit,
        );
        let resp = match result {
            Ok(term) => AppendEntriesResponse::new_accept(term),
            Err((term, hint)) => AppendEntriesResponse::new_reject(term, hint),
        };

        Ok(resp)
    }

    /// Handle `Vote` requests
    pub(super) async fn vote(&self, req: VoteRequest) -> Result<VoteResponse, CurpError> {
        let result = self.curp.handle_vote(
            req.term,
            req.candidate_id,
            req.last_log_index,
            req.last_log_term,
        );
        let resp = match result {
            Ok((term, sp)) => {
                self.storage.flush_voted_for(term, req.candidate_id).await?;
                VoteResponse::new_accept(term, sp)?
            }
            Err(term) => VoteResponse::new_reject(term),
        };

        Ok(resp)
    }

    /// handle "wait synced" request
    pub(super) async fn wait_synced(
        &self,
        req: WaitSyncedRequest,
    ) -> Result<WaitSyncedResponse, CurpError> {
        let id = req.id()?;
        debug!("{} get wait synced request for cmd({id})", self.curp.id());

        let (er, asr) = CommandBoard::wait_for_er_asr(&self.cmd_board, &id).await;
        let resp = WaitSyncedResponse::new_from_result::<C>(Some(er), asr)?;

        debug!("{} wait synced for cmd({id}) finishes", self.curp.id());
        Ok(resp)
    }

    /// Handle fetch leader requests
    #[allow(clippy::unnecessary_wraps, clippy::needless_pass_by_value)] // To keep type consistent with other request handlers
    pub(super) fn fetch_leader(
        &self,
        _req: FetchLeaderRequest,
    ) -> Result<FetchLeaderResponse, CurpError> {
        let (leader_id, term) = self.curp.leader();
        Ok(FetchLeaderResponse::new(leader_id, term))
    }

    /// Handle fetch cluster requests
    #[allow(clippy::unnecessary_wraps, clippy::needless_pass_by_value)] // To keep type consistent with other request handlers
    pub(super) fn fetch_cluster(
        &self,
        _req: FetchClusterRequest,
    ) -> Result<FetchClusterResponse, CurpError> {
        let (leader_id, term) = self.curp.leader();
        let all_members = self.curp.cluster().all_members_addrs();
        Ok(FetchClusterResponse::new(leader_id, all_members, term))
    }

    /// Install snapshot
    #[allow(clippy::integer_arithmetic)] // can't overflow
    pub(super) async fn install_snapshot(
        &self,
        req_stream: impl Stream<Item = Result<InstallSnapshotRequest, String>>,
    ) -> Result<InstallSnapshotResponse, CurpError> {
        pin_mut!(req_stream);
        let mut snapshot = self
            .snapshot_allocator
            .allocate_new_snapshot()
            .await
            .map_err(|err| {
                error!("failed to allocate a new snapshot, {err:?}");
                CurpError::Internal("failed to allocate a new snapshot".to_owned())
            })?;
        while let Some(req) = req_stream.next().await {
            let req = req.map_err(|e| {
                warn!("snapshot stream error, {e}");
                CurpError::Transport(e)
            })?;
            if !self.curp.verify_install_snapshot(
                req.term,
                req.leader_id,
                req.last_included_index,
                req.last_included_term,
            ) {
                return Ok(InstallSnapshotResponse::new(self.curp.term()));
            }
            let req_data_len = req.data.len().numeric_cast::<u64>();
            snapshot.write_all(req.data).await.map_err(|err| {
                error!("can't write snapshot data, {err:?}");
                err
            })?;
            if req.done {
                debug_assert_eq!(
                    snapshot.size(),
                    req.offset + req_data_len,
                    "snapshot corrupted"
                );
                let meta = SnapshotMeta {
                    last_included_index: req.last_included_index,
                    last_included_term: req.last_included_term,
                };
                let snapshot = Snapshot::new(meta, snapshot);
                info!(
                    "{} successfully received a snapshot, {snapshot:?}",
                    self.curp.id(),
                );
                self.ce_event_tx
                    .send_reset(Some(snapshot))
                    .await
                    .map_err(|err| {
                        let err = CurpError::Internal(format!(
                            "failed to reset the command executor by snapshot, {err}"
                        ));
                        error!("{err}");
                        err
                    })?;
                return Ok(InstallSnapshotResponse::new(self.curp.term()));
            }
        }
        Err(CurpError::Transport(
            "failed to receive a complete snapshot".to_owned(),
        ))
    }

    /// Handle fetch read state requests
    #[allow(clippy::needless_pass_by_value)] // To keep type consistent with other request handlers
    pub(super) fn fetch_read_state(
        &self,
        req: FetchReadStateRequest,
    ) -> Result<FetchReadStateResponse, CurpError> {
        let cmd = req.cmd()?;
        let state = self.curp.handle_fetch_read_state(&cmd)?;
        Ok(FetchReadStateResponse::new(state))
    }
}

/// Spawned tasks
impl<C: 'static + Command, RC: RoleChange + 'static> CurpNode<C, RC> {
    /// Tick periodically
    async fn election_task(curp: Arc<RawCurp<C, RC>>) {
        let heartbeat_interval = curp.cfg().heartbeat_interval;
        // wait for some random time before tick starts to minimize vote split possibility
        let rand = thread_rng()
            .gen_range(0..heartbeat_interval.as_millis())
            .numeric_cast();
        tokio::time::sleep(Duration::from_millis(rand)).await;

        let mut ticker = tokio::time::interval(heartbeat_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let _now = ticker.tick().await;
            if let Some(vote) = curp.tick_election() {
                Self::bcast_vote(curp.as_ref(), vote).await;
            }
        }
    }

    /// Responsible for bringing up `sync_follower_task` when self becomes leader
    async fn sync_followers_daemon(curp: Arc<RawCurp<C, RC>>) {
        let leader_event = curp.leader_event();
        loop {
            if !curp.is_leader() {
                leader_event.listen().await;
            }

            let (sync_task_tx, mut sync_task_rx) = mpsc::channel(128);
            let mut shutdown_events = HashMap::new();

            for c in curp.connects().iter() {
                let sync_event = curp.sync_event(c.id());
                let shutdown_event = Arc::new(Event::new());
                let fut = Self::sync_follower_task(
                    Arc::clone(&curp),
                    Arc::clone(c.value()),
                    sync_event,
                    Arc::clone(&shutdown_event),
                );
                sync_task_tx
                    .send(tokio::spawn(fut))
                    .await
                    .unwrap_or_else(|_e| unreachable!("receiver should not be closed"));
                _ = shutdown_events.insert(c.id(), shutdown_event);
            }

            let handle = tokio::spawn(Self::conf_change_handler(
                Arc::clone(&curp),
                shutdown_events,
                sync_task_tx,
            ));

            while let Some(h) = sync_task_rx.recv().await {
                h.await
                    .unwrap_or_else(|e| unreachable!("sync follower task panicked: {e}"));
            }
            handle.abort();
        }
    }

    /// Handler of conf
    async fn conf_change_handler(
        curp: Arc<RawCurp<C, RC>>,
        mut shutdown_events: HashMap<ServerId, Arc<Event>>,
        sender: mpsc::Sender<JoinHandle<()>>,
    ) {
        let change_rx = curp.change_rx();
        while let Ok(change) = change_rx.recv_async().await {
            #[allow(clippy::unwrap_used)] // change.change_type must be valid
            match ConfChangeType::from_i32(change.change_type).unwrap() {
                ConfChangeType::Add | ConfChangeType::AddLearner => {
                    let connect = curp.connect(change.node_id);
                    let sync_event = curp.sync_event(change.node_id);
                    let shutdown_event = Arc::new(Event::new());
                    sender
                        .send(tokio::spawn(Self::sync_follower_task(
                            Arc::clone(&curp),
                            connect.into_inner(),
                            sync_event,
                            Arc::clone(&shutdown_event),
                        )))
                        .await
                        .unwrap();
                    _ = shutdown_events.insert(change.node_id, shutdown_event);
                }
                ConfChangeType::Remove => {
                    let Some(event) = shutdown_events.remove(&change.node_id) else {
                        unreachable!("shutdown_event of removed follower should exist");
                    };
                    event.notify(1);
                }
                ConfChangeType::Update => {
                    // TODO: update connect when #423 merged
                }
            }
        }
    }

    /// Leader use this task to keep a follower up-to-date, will return if self is no longer leader
    async fn sync_follower_task(
        curp: Arc<RawCurp<C, RC>>,
        connect: Arc<impl ConnectApi + ?Sized>,
        sync_event: Arc<Event>,
        shutdown_event: Arc<Event>,
    ) {
        debug!("{} to {} sync follower task start", curp.id(), connect.id());
        let mut hb_opt = false;
        let mut ticker = tokio::time::interval(curp.cfg().heartbeat_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let id = connect.id();
        let batch_timeout = curp.cfg().batch_timeout;

        #[allow(clippy::integer_arithmetic, clippy::unwrap_used)]
        // tokio select internal triggered,
        loop {
            // grab the listener in the beginning to prevent missing sync events
            let listener: event_listener::EventListener = sync_event.listen();

            let Ok(sync_action) = curp.sync(id) else {
                return;
            };

            match sync_action {
                SyncAction::AppendEntries(ae) => {
                    // (hb_opt, entries) status combination
                    // (false, empty) => send heartbeat to followers
                    // (true, empty) => indicates that `batch_timeout` expired, and during this period there is not any log generated. Do nothing
                    // (true | false, not empty) => send append entries
                    if !hb_opt || !ae.entries.is_empty() {
                        let result = Self::send_ae(connect.as_ref(), curp.as_ref(), ae).await;
                        if let Err(err) = result {
                            warn!("ae to {} failed, {err}", connect.id());
                            if matches!(err, SendAEError::NotLeader) {
                                return;
                            }
                        } else {
                            hb_opt = true;
                        }
                    }
                }
                SyncAction::Snapshot(rx) => match rx.await {
                    Ok(snapshot) => {
                        let result =
                            Self::send_snapshot(connect.as_ref(), curp.as_ref(), snapshot).await;
                        if let Err(err) = result {
                            warn!("snapshot to {} failed, {err}", connect.id());
                            if matches!(err, SendSnapshotError::NotLeader) {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        warn!("failed to receive snapshot result, {err}");
                    }
                },
            }

            // a sync is either triggered by an heartbeat timeout event or when new log entries arrive
            tokio::select! {
                _now = ticker.tick() => {
                    hb_opt = false;
                }
                _ = shutdown_event.listen() => {
                    break;
                }
                res = tokio::time::timeout(batch_timeout, listener) => {
                    if let Err(_e) = res {
                        hb_opt = true;
                    }
                }
            }
        }
        debug!("{} to {} sync follower task exits", curp.id(), connect.id());
    }

    /// Log persist task
    pub(super) async fn log_persist_task(
        mut log_rx: mpsc::UnboundedReceiver<Arc<LogEntry<C>>>,
        storage: Arc<dyn StorageApi<Command = C>>,
    ) {
        while let Some(e) = log_rx.recv().await {
            if let Err(err) = storage.put_log_entry(e.as_ref()).await {
                error!("storage error, {err}");
            }
        }
        error!("log persist task exits unexpectedly");
    }
}

// utils
impl<C: 'static + Command, RC: RoleChange + 'static> CurpNode<C, RC> {
    /// Create a new server instance
    #[inline]
    pub(super) async fn new<CE: CommandExecutor<C> + 'static>(
        cluster_info: Arc<ClusterInfo>,
        is_leader: bool,
        cmd_executor: Arc<CE>,
        snapshot_allocator: Box<dyn SnapshotAllocator>,
        role_change: RC,
        curp_cfg: Arc<CurpConfig>,
    ) -> Result<Self, CurpError> {
        let sync_events = cluster_info
            .peers_ids()
            .into_iter()
            .map(|server_id| (server_id, Arc::new(Event::new())))
            .collect();
        let connects = rpc::connect(cluster_info.peers_addrs()).await.collect();
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        let shutdown_trigger = Arc::new(Event::new());
        let cmd_board = Arc::new(RwLock::new(CommandBoard::new()));
        let spec_pool = Arc::new(Mutex::new(SpeculativePool::new()));
        let uncommitted_pool = Arc::new(Mutex::new(UncommittedPool::new()));
        let last_applied = cmd_executor
            .last_applied()
            .map_err(|e| CurpError::Internal(format!("get applied index error, {e}")))?;
        let (ce_event_tx, task_rx, done_tx) =
            conflict_checked_mpmc::channel(Arc::clone(&cmd_executor));
        let ce_event_tx: Arc<dyn CEEventTxApi<C>> = Arc::new(ce_event_tx);
        let storage = Arc::new(DB::open(&curp_cfg.storage_cfg)?);

        // create curp state machine
        let (voted_for, entries) = storage.recover().await?;
        let curp = if voted_for.is_none() && entries.is_empty() {
            Arc::new(RawCurp::new(
                Arc::clone(&cluster_info),
                is_leader,
                Arc::clone(&cmd_board),
                Arc::clone(&spec_pool),
                uncommitted_pool,
                Arc::clone(&curp_cfg),
                Arc::clone(&ce_event_tx),
                sync_events,
                log_tx,
                role_change,
                connects,
            ))
        } else {
            info!(
                "{} recovered voted_for({voted_for:?}), entries from {:?} to {:?}",
                cluster_info.self_id(),
                entries.first(),
                entries.last()
            );
            Arc::new(RawCurp::recover_from(
                Arc::clone(&cluster_info),
                is_leader,
                Arc::clone(&cmd_board),
                Arc::clone(&spec_pool),
                uncommitted_pool,
                &curp_cfg,
                Arc::clone(&ce_event_tx),
                sync_events,
                log_tx,
                voted_for,
                entries,
                last_applied,
                role_change,
                connects,
            ))
        };

        start_cmd_workers(
            &cmd_executor,
            Arc::clone(&curp),
            task_rx,
            done_tx,
            Arc::clone(&shutdown_trigger),
        );
        run_gc_tasks(
            Arc::clone(&cmd_board),
            Arc::clone(&spec_pool),
            curp_cfg.gc_interval,
            Arc::clone(&shutdown_trigger),
        );

        Self::run_bg_tasks(
            Arc::clone(&curp),
            Arc::clone(&storage),
            Arc::clone(&shutdown_trigger),
            log_rx,
        )
        .await;

        Ok(Self {
            curp,
            spec_pool,
            cmd_board,
            shutdown_trigger,
            ce_event_tx,
            storage,
            snapshot_allocator,
        })
    }

    /// Run background tasks for Curp server
    async fn run_bg_tasks(
        curp: Arc<RawCurp<C, RC>>,
        storage: Arc<impl StorageApi<Command = C> + 'static>,
        shutdown_trigger: Arc<Event>,
        log_rx: mpsc::UnboundedReceiver<Arc<LogEntry<C>>>,
    ) {
        let election_task = tokio::spawn(Self::election_task(Arc::clone(&curp)));
        let sync_followers_daemon = tokio::spawn(Self::sync_followers_daemon(Arc::clone(&curp)));
        let log_persist_task = tokio::spawn(Self::log_persist_task(log_rx, storage));

        let _ig = tokio::spawn(async move {
            shutdown_trigger.listen().await;
            election_task.abort();
            sync_followers_daemon.abort();
            log_persist_task.abort();
        });
    }

    /// Candidate broadcasts votes
    async fn bcast_vote(curp: &RawCurp<C, RC>, vote: Vote) {
        debug!("{} broadcasts votes to all servers", curp.id());
        let rpc_timeout = curp.cfg().rpc_timeout;
        let resps = curp
            .connects()
            .iter()
            .map(|c| {
                let req = VoteRequest::new(
                    vote.term,
                    vote.candidate_id,
                    vote.last_log_index,
                    vote.last_log_term,
                );
                let connect = Arc::clone(c.value());
                async move {
                    let resp = connect.vote(req, rpc_timeout).await;
                    (connect.id(), resp)
                }
            })
            .collect::<FuturesUnordered<_>>()
            .filter_map(|(id, resp)| async move {
                match resp {
                    Err(e) => {
                        warn!("request vote from {id} failed, {e}");
                        None
                    }
                    Ok(resp) => Some((id, resp.into_inner())),
                }
            });
        pin_mut!(resps);
        while let Some((id, resp)) = resps.next().await {
            // collect follower spec pool
            let follower_spec_pool = match resp.spec_pool() {
                Err(e) => {
                    error!("can't deserialize spec_pool from vote response, {e}");
                    continue;
                }
                Ok(spec_pool) => spec_pool.into_iter().map(|cmd| Arc::new(cmd)).collect(),
            };
            let result =
                curp.handle_vote_resp(id, resp.term, resp.vote_granted, follower_spec_pool);
            match result {
                Ok(false) => {}
                Ok(true) | Err(()) => return,
            }
        }
    }

    /// Get a rx for leader changes
    pub(super) fn leader_rx(&self) -> broadcast::Receiver<Option<ServerId>> {
        self.curp.leader_rx()
    }

    /// Send `append_entries` request
    #[allow(clippy::integer_arithmetic)] // won't overflow
    async fn send_ae(
        connect: &(impl ConnectApi + ?Sized),
        curp: &RawCurp<C, RC>,
        ae: AppendEntries<C>,
    ) -> Result<(), SendAEError> {
        let last_sent_index = (!ae.entries.is_empty())
            .then(|| ae.prev_log_index + ae.entries.len().numeric_cast::<u64>());
        let is_heartbeat = ae.entries.is_empty();
        let req = AppendEntriesRequest::new(
            ae.term,
            ae.leader_id,
            ae.prev_log_index,
            ae.prev_log_term,
            ae.entries,
            ae.leader_commit,
        )?;

        if is_heartbeat {
            trace!("{} send heartbeat to {}", curp.id(), connect.id());
        } else {
            debug!("{} send append_entries to {}", curp.id(), connect.id());
        }

        let resp = connect
            .append_entries(req, curp.cfg().rpc_timeout)
            .await?
            .into_inner();

        let succeeded = curp
            .handle_append_entries_resp(
                connect.id(),
                last_sent_index,
                resp.term,
                resp.success,
                resp.hint_index,
            )
            .map_err(|_e| SendAEError::NotLeader)?;

        if succeeded {
            Ok(())
        } else {
            Err(SendAEError::Rejected)
        }
    }

    /// Send snapshot
    async fn send_snapshot(
        connect: &(impl ConnectApi + ?Sized),
        curp: &RawCurp<C, RC>,
        snapshot: Snapshot,
    ) -> Result<(), SendSnapshotError> {
        let meta = snapshot.meta;
        let resp = connect
            .install_snapshot(curp.term(), curp.id(), snapshot)
            .await?
            .into_inner();
        curp.handle_snapshot_resp(connect.id(), meta, resp.term)
            .map_err(|_e| SendSnapshotError::NotLeader)
    }
}

impl<C: Command, RC: RoleChange> Drop for CurpNode<C, RC> {
    #[inline]
    fn drop(&mut self) {
        self.shutdown_trigger.notify(usize::MAX);
    }
}

impl<C: Command, RC: RoleChange> Debug for CurpNode<C, RC> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurpNode")
            .field("raw_curp", &self.curp)
            .field("spec_pool", &self.spec_pool)
            .field("cmd_board", &self.cmd_board)
            .field("shutdown_trigger", &self.shutdown_trigger)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use curp_test_utils::{mock_role_change, sleep_secs, test_cmd::TestCommand};
    use tracing_test::traced_test;

    use super::*;
    use crate::{rpc::connect::MockConnectApi, server::cmd_worker::MockCEEventTxApi};

    #[traced_test]
    #[tokio::test]
    async fn sync_task_will_send_hb() {
        let curp = Arc::new(RawCurp::new_test(
            3,
            MockCEEventTxApi::<TestCommand>::default(),
            mock_role_change(),
        ));
        let mut mock_connect1 = MockConnectApi::default();
        mock_connect1
            .expect_append_entries()
            .times(1..)
            .returning(|_, _| Ok(tonic::Response::new(AppendEntriesResponse::new_accept(0))));
        let id = curp.cluster().get_id_by_name("S1").unwrap();
        mock_connect1.expect_id().return_const(id);
        tokio::spawn(CurpNode::sync_follower_task(
            curp,
            Arc::new(mock_connect1),
            Arc::new(Event::new()),
            Arc::new(Event::new()),
        ));
        sleep_secs(2).await;
    }
}
