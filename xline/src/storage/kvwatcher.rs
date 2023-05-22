use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

use clippy_utilities::OverflowArithmetic;
use log::warn;
use parking_lot::RwLock;
use tokio::sync::mpsc;
use utils::parking_lot_lock::RwLockMap;

use super::storage_api::StorageApi;
use crate::{rpc::Event, server::command::KeyRange, storage::kv_store::KvStore};

/// Watch ID
pub(crate) type WatchId = i64;

/// Watch ID generator
#[derive(Debug)]
pub(crate) struct WatchIdGenerator(AtomicI64);

impl WatchIdGenerator {
    /// Create a new `WatchIdGenerator`
    pub(crate) fn new(rev: i64) -> Self {
        Self(AtomicI64::new(rev))
    }

    /// Get the next revision number
    pub(crate) fn next(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }
}

/// Watcher
#[derive(Debug)]
struct Watcher {
    /// Key Range
    key_range: KeyRange,
    /// Watch ID
    watch_id: WatchId,
    /// Start revision of this watcher
    start_rev: i64,
    /// Event filters
    filters: Vec<i32>,
    /// Sender of watch event
    event_tx: mpsc::Sender<WatchEvent>,
}

impl PartialEq for Watcher {
    fn eq(&self, other: &Self) -> bool {
        self.watch_id == other.watch_id
    }
}

impl Eq for Watcher {}

impl Hash for Watcher {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.watch_id.hash(state);
    }
}

impl Watcher {
    /// New `WatcherInner`
    fn new(
        key_range: KeyRange,
        watch_id: WatchId,
        start_rev: i64,
        filters: Vec<i32>,
        event_tx: mpsc::Sender<WatchEvent>,
    ) -> Self {
        Self {
            key_range,
            watch_id,
            start_rev,
            filters,
            event_tx,
        }
    }

    /// Get watch id
    fn watch_id(&self) -> i64 {
        self.watch_id
    }

    /// Get key range
    fn key_range(&self) -> &KeyRange {
        &self.key_range
    }

    /// Get start revision
    fn start_rev(&self) -> i64 {
        self.start_rev
    }

    /// Notify events
    fn notify(&self, (revision, mut events): (i64, Vec<Event>)) {
        if revision < self.start_rev() {
            return;
        }
        events.retain(|event| self.filters.iter().all(|filter| filter != &event.r#type));
        let watch_event = WatchEvent {
            id: self.watch_id(),
            events,
            revision,
        };
        #[allow(clippy::todo)]
        match self.event_tx.try_send(watch_event) {
            Ok(_) => {}
            Err(_) => todo!(), // TODO: send error will move this watcher to victims
        }
    }
}

/// KV watcher
#[derive(Debug)]
pub(crate) struct KvWatcher<S>
where
    S: StorageApi,
{
    /// KV storage
    storage: Arc<KvStore<S>>,
    /// Watch indexes
    watcher_map: RwLock<WatcherMap>,
}

/// Store all watchers
#[derive(Debug)]
struct WatcherMap {
    /// All watchers
    watchers: HashMap<WatchId, Arc<Watcher>>,
    /// Index for watchers
    index: HashMap<KeyRange, HashSet<Arc<Watcher>>>,
}

impl WatcherMap {
    /// Create a new `WatcherMap`
    fn new() -> Self {
        Self {
            watchers: HashMap::new(),
            index: HashMap::new(),
        }
    }

    /// Insert a new watcher to the map and create. Internally, it will create a index for this watcher.
    fn insert(&mut self, watcher: Arc<Watcher>) {
        let key_range = watcher.key_range().clone();
        let watch_id = watcher.watch_id();
        assert!(
            self.watchers
                .insert(watch_id, Arc::clone(&watcher))
                .is_none(),
            "can't insert a watcher twice"
        );
        assert!(
            self.index
                .entry(key_range)
                .or_insert_with(HashSet::new)
                .insert(watcher),
            "can't insert a watcher twice"
        );
    }

    /// Remove a watcher
    #[allow(clippy::expect_used)] // the logic is managed internally
    fn remove(&mut self, watch_id: WatchId) {
        let watcher = self.watchers.remove(&watch_id).expect("no such watcher");
        let key_range = watcher.key_range();
        let is_empty = {
            let watchers = self
                .index
                .get_mut(key_range)
                .expect("no such watcher in index");
            assert!(watchers.remove(&watcher), "no such watcher in index");
            watchers.is_empty()
        };
        if is_empty {
            assert!(self.index.remove(key_range).is_some());
        }
    }
}

/// Operations of KV watcher
#[allow(clippy::integer_arithmetic, clippy::indexing_slicing)] // Introduced by mockall::automock
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub(crate) trait KvWatcherOps {
    /// Create a watch to KV store
    fn watch(
        &self,
        id: WatchId,
        key_range: KeyRange,
        start_rev: i64,
        filters: Vec<i32>,
        event_tx: mpsc::Sender<WatchEvent>,
    );

    /// Cancel a watch from KV store
    fn cancel(&self, id: WatchId) -> i64;
}

#[async_trait::async_trait]
impl<S> KvWatcherOps for KvWatcher<S>
where
    S: StorageApi,
{
    /// Create a watch to KV store
    fn watch(
        &self,
        id: WatchId,
        key_range: KeyRange,
        start_rev: i64,
        filters: Vec<i32>,
        event_tx: mpsc::Sender<WatchEvent>,
    ) {
        let mut watcher = Watcher::new(key_range.clone(), id, start_rev, filters, event_tx);
        let mut watcher_map_w = self.watcher_map.write();

        let initial_events = if start_rev == 0 {
            vec![]
        } else {
            self.storage
                .get_event_from_revision(key_range, start_rev)
                .unwrap_or_else(|e| {
                    warn!("failed to get initial events for watcher: {:?}", e);
                    vec![]
                })
        };
        if !initial_events.is_empty() {
            let last_revision = initial_events
                .last()
                .unwrap_or_else(|| unreachable!("initial_events is not empty"))
                .kv
                .as_ref()
                .unwrap_or_else(|| panic!("event.kv can't be None"))
                .mod_revision;

            watcher.notify((last_revision, initial_events));
            watcher.start_rev = last_revision.overflow_add(1);
        }
        watcher_map_w.insert(Arc::new(watcher));
    }

    /// Cancel a watch from KV store
    fn cancel(&self, id: WatchId) -> i64 {
        let revision = self.storage.revision();
        self.watcher_map.write().remove(id);
        revision
    }
}

impl<S> KvWatcher<S>
where
    S: StorageApi,
{
    /// Create a new `Arc<KvWatcher>`
    pub(crate) fn new_arc(
        storage: Arc<KvStore<S>>,
        mut kv_update_rx: mpsc::Receiver<(i64, Vec<Event>)>,
    ) -> Arc<Self> {
        let kv_watcher = Arc::new(Self {
            storage,
            watcher_map: RwLock::new(WatcherMap::new()),
        });
        let watcher = Arc::clone(&kv_watcher);
        let _handle = tokio::spawn(async move {
            while let Some(updates) = kv_update_rx.recv().await {
                watcher.handle_kv_updates(updates);
            }
        });
        kv_watcher
    }

    /// Handle KV store updates
    fn handle_kv_updates(&self, (revision, all_events): (i64, Vec<Event>)) {
        self.watcher_map.map_read(|watcher_map_r| {
            let mut watcher_events: HashMap<&Arc<Watcher>, Vec<Event>> = HashMap::new();
            for event in all_events {
                let watchers: HashSet<&Arc<Watcher>> = watcher_map_r
                    .index
                    .iter()
                    .filter_map(|(k, v)| {
                        k.contains_key(
                            &event
                                .kv
                                .as_ref()
                                .unwrap_or_else(|| panic!("Receive Event with empty kv"))
                                .key,
                        )
                        .then_some(v)
                    })
                    .flatten()
                    .collect();
                for watcher in watchers {
                    if event
                        .kv
                        .as_ref()
                        .map_or(true, |kv| kv.mod_revision < watcher.start_rev)
                    {
                        continue;
                    }
                    #[allow(clippy::indexing_slicing)]
                    watcher_events
                        .entry(watcher)
                        .or_default()
                        .push(event.clone());
                }
            }
            for (w, es) in watcher_events {
                w.notify((revision, es));
            }
        });
    }
}

/// Watch Event
#[derive(Debug)]
pub(crate) struct WatchEvent {
    /// Watch ID
    id: WatchId,
    /// Events to be sent
    events: Vec<Event>,
    /// Revision when this event is generated
    revision: i64,
}

impl WatchEvent {
    /// Get revision
    pub(crate) fn revision(&self) -> i64 {
        self.revision
    }

    /// Get `WatchId`
    pub(crate) fn watch_id(&self) -> WatchId {
        self.id
    }

    /// Take events
    pub(crate) fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod test {

    use std::{collections::BTreeMap, time::Duration};

    use tokio::time::timeout;
    use utils::config::StorageConfig;

    use super::*;
    use crate::{
        header_gen::HeaderGenerator,
        rpc::{PutRequest, RequestWithToken},
        storage::{db::DB, index::Index, lease_store::LeaseCollection, KvStore},
    };

    fn init_empty_store() -> (Arc<KvStore<DB>>, Arc<DB>, Arc<KvWatcher<DB>>) {
        let db = DB::open(&StorageConfig::Memory).unwrap();
        let header_gen = Arc::new(HeaderGenerator::new(0, 0));
        let index = Arc::new(Index::new());
        let lease_collection = Arc::new(LeaseCollection::new(0));
        let (kv_update_tx, kv_update_rx) = mpsc::channel(128);
        let store = Arc::new(KvStore::new(
            kv_update_tx,
            lease_collection,
            header_gen,
            Arc::clone(&db),
            index,
        ));
        let kv_watcher = KvWatcher::new_arc(Arc::clone(&store), kv_update_rx);
        (store, db, kv_watcher)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 10)]
    async fn watch_should_not_lost_events() {
        let (store, db, kv_watcher) = init_empty_store();
        let mut map = BTreeMap::new();
        let handle = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                for i in 0..100_u8 {
                    let req = RequestWithToken::new(
                        PutRequest {
                            key: "foo".into(),
                            value: vec![i],
                            ..Default::default()
                        }
                        .into(),
                    );
                    let (sync_res, ops) = store.after_sync(&req).await.unwrap();
                    db.flush_ops(ops).unwrap();
                    store.mark_index_available(sync_res.revision());
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_micros(500)).await;
        let (event_tx, mut event_rx) = mpsc::channel(128);
        kv_watcher.watch(123, KeyRange::new_one_key("foo"), 1, vec![], event_tx);

        'outer: while let Some(event_batch) = timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .unwrap()
        {
            for event in event_batch.events {
                let val = event.kv.as_ref().unwrap().value[0];
                let e = map.entry(val).or_insert(0);
                *e += 1;
                if val == 99 {
                    break 'outer;
                }
            }
        }

        assert_eq!(map.len(), 100);
        for (k, count) in map {
            assert_eq!(count, 1, "key {k} should be notified once");
        }
        handle.abort();
    }
}
