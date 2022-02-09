//! Data for the lifecycle of the Ingester

use crate::compact::compact_persisting_batch;
use crate::persist::persist;
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use chrono::{format::StrftimeItems, TimeZone, Utc};
use data_types::delete_predicate::DeletePredicate;
use dml::DmlOperation;
use generated_types::{google::FieldViolation, influxdata::iox::ingester::v1 as proto};
use iox_catalog::interface::{
    Catalog, KafkaPartition, NamespaceId, PartitionId, PartitionInfo, SequenceNumber, SequencerId,
    TableId, Timestamp, Tombstone,
};
use mutable_batch::column::ColumnData;
use mutable_batch::MutableBatch;
use object_store::ObjectStore;
use observability_deps::tracing::{error, warn};
use parking_lot::RwLock;
use predicate::Predicate;
use query::exec::Executor;
use schema::selection::Selection;
use schema::TIME_COLUMN_NAME;
use snafu::{OptionExt, ResultExt, Snafu};
use std::convert::TryFrom;
use std::ops::DerefMut;
use std::time::Duration;
use std::{collections::BTreeMap, sync::Arc};
use time::SystemProvider;
use uuid::Uuid;

#[derive(Debug, Snafu)]
#[allow(missing_copy_implementations, missing_docs)]
pub enum Error {
    #[snafu(display("Error while reading Topic {}", name))]
    ReadTopic {
        source: iox_catalog::interface::Error,
        name: String,
    },

    #[snafu(display("Error while reading Kafka Partition id {}", id.get()))]
    ReadSequencer {
        source: iox_catalog::interface::Error,
        id: KafkaPartition,
    },

    #[snafu(display("Sequencer {} not found in data map", sequencer_id))]
    SequencerNotFound { sequencer_id: SequencerId },

    #[snafu(display("Namespace {} not found in catalog", namespace))]
    NamespaceNotFound { namespace: String },

    #[snafu(display("Table {} not found in buffer", table_name))]
    TableNotFound { table_name: String },

    #[snafu(display("Table must be specified in delete"))]
    TableNotPresent,

    #[snafu(display("Error accessing catalog: {}", source))]
    Catalog {
        source: iox_catalog::interface::Error,
    },

    #[snafu(display("The persisting is in progress. Cannot accept more persisting batch"))]
    PersistingNotEmpty,

    #[snafu(display("Nothing in the Persisting list to get removed"))]
    PersistingEmpty,

    #[snafu(display("The given batch does not match any in the Persisting list. Nothing is removed from the Persisting list"))]
    PersistingNotMatch,

    #[snafu(display("Time column not present"))]
    TimeColumnNotPresent,

    #[snafu(display("Snapshot error: {}", source))]
    Snapshot { source: mutable_batch::Error },

    #[snafu(display("Partition not found: {}", partition_id))]
    PartitionNotFound { partition_id: PartitionId },
}

/// A specialized `Error` for Ingester Data errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Contains all buffered and cached data for the ingester.
pub struct IngesterData {
    /// Object store for persistence of parquet files
    pub(crate) object_store: Arc<ObjectStore>,
    /// The global catalog for schema, parquet files and tombstones
    pub(crate) catalog: Arc<dyn Catalog>,
    // This map gets set up on initialization of the ingester so it won't ever be modified.
    // The content of each SequenceData will get changed when more namespaces and tables
    // get ingested.
    pub(crate) sequencers: BTreeMap<SequencerId, SequencerData>,
}

impl IngesterData {
    /// Store the write or delete in the in memory buffer. Deletes will
    /// be written into the catalog before getting stored in the buffer.
    /// Any writes that create new IOx partitions will have those records
    /// created in the catalog before putting into the buffer.
    pub async fn buffer_operation(
        &self,
        sequencer_id: SequencerId,
        dml_operation: DmlOperation,
    ) -> Result<()> {
        let sequencer_data = self
            .sequencers
            .get(&sequencer_id)
            .context(SequencerNotFoundSnafu { sequencer_id })?;
        sequencer_data
            .buffer_operation(dml_operation, sequencer_id, self.catalog.as_ref())
            .await
    }
}

/// The Persister has a single function that will persist a given partition Id. It is expected
/// that the persist function will retry forever until it succeeds.
#[async_trait]
pub(crate) trait Persister: Send + Sync + 'static {
    async fn persist(&self, partition_id: PartitionId);
}

#[async_trait]
impl Persister for IngesterData {
    async fn persist(&self, partition_id: PartitionId) {
        let retry_time = Duration::from_secs(1);

        // lookup the partition_info from the catalog
        let partition_info: Option<PartitionInfo> = loop {
            match self.catalog.start_transaction().await {
                Ok(mut txn) => match txn.partitions().partition_info_by_id(partition_id).await {
                    Ok(p) => break p,
                    Err(e) => {
                        error!(%e, "error getting partition_info_by_id");
                        tokio::time::sleep(retry_time).await;
                    }
                },
                Err(e) => {
                    error!(%e, "error starting transaction");
                    tokio::time::sleep(retry_time).await;
                }
            }
        };

        // lookup the state from the ingester data. If something isn't found, it's unexpected. Crash
        // so someone can take a look.
        let partition_info = partition_info
            .unwrap_or_else(|| panic!("partition {} not found in catalog", partition_id));
        let sequencer_data = self
            .sequencers
            .get(&partition_info.partition.sequencer_id)
            .unwrap_or_else(|| {
                panic!(
                    "sequencer state for {} not in ingester data",
                    partition_info.partition.sequencer_id
                )
            }); //{
        let namespace = sequencer_data
            .namespace(&partition_info.namespace_name)
            .unwrap_or_else(|| {
                panic!(
                    "namespace {} not in sequencer {} state",
                    partition_info.namespace_name, partition_info.partition.sequencer_id
                )
            });
        let table_data = namespace
            .table_data(&partition_info.table_name)
            .unwrap_or_else(|| {
                panic!(
                    "table {} for namespace {} not in sequencer {} state",
                    partition_info.table_name,
                    partition_info.namespace_name,
                    partition_info.partition.sequencer_id
                )
            });
        let partition_data = table_data
            .partition_data(&partition_info.partition.partition_key)
            .unwrap_or_else(|| {
                panic!(
                    "partition {} not in table {} for namespace {} in sequencer {} state",
                    partition_info.partition.partition_key,
                    partition_info.table_name,
                    partition_info.namespace_name,
                    partition_info.partition.sequencer_id
                )
            });

        // snapshot and make arc clones of the data.
        let persisting_batch = partition_data.snapshot_to_persisting_batch(
            partition_info.partition.sequencer_id,
            partition_info.partition.table_id,
            partition_info.partition.id,
            &partition_info.table_name,
        );

        // do the CPU intensive work of compaction, de-duplication and sorting
        let exec = Executor::new(1);
        let (record_batches, iox_meta) = match compact_persisting_batch(
            Arc::new(SystemProvider::new()),
            &exec,
            namespace.namespace_id.get(),
            &partition_info.namespace_name,
            &partition_info.table_name,
            &partition_info.partition.partition_key,
            Arc::clone(&persisting_batch),
        )
        .await
        {
            Err(e) => {
                // this should never error out. if it does, we need to crash hard so
                // someone can take a look.
                panic!("unable to compact persisting batch with error: {:?}", e);
            }
            Ok(Some(r)) => r,
            Ok(None) => {
                warn!("persist called with no data");
                return;
            }
        };

        // save the compacted data to a parquet file in object storage
        loop {
            match persist(&iox_meta, record_batches.to_vec(), &self.object_store).await {
                Ok(_) => break,
                Err(e) => {
                    error!(%e, "error persisting parquet file to object store");
                    tokio::time::sleep(retry_time).await;
                }
            }
        }

        // Commit the parquet file and tombstones to the catalog. This is pretty ugly because of all
        // the failures that might happen where we just want to keep retrying it.
        // TODO: clean this up when updating the min_sequence_number is added in.
        let parquet_file = iox_meta.to_parquet_file();
        loop {
            match self.catalog.start_transaction().await {
                Ok(mut txn) => {
                    match iox_catalog::add_parquet_file_with_tombstones(
                        &parquet_file,
                        &persisting_batch.data.deletes,
                        txn.deref_mut(),
                    )
                    .await
                    {
                        Ok(_) => match txn.commit().await {
                            Ok(_) => break,
                            Err(e) => {
                                error!(%e, "error commiting transaction to catalog");
                                tokio::time::sleep(retry_time).await;
                            }
                        },
                        Err(e) => {
                            error!(%e, "error from catalog adding parquet file and processed tombstones");
                            if let Err(e) = txn.abort().await {
                                error!(%e, "error aborting failed transaction to add parquet file and tombstones");
                            }
                            tokio::time::sleep(retry_time).await;
                        }
                    }
                }
                Err(e) => {
                    error!(%e, "error starting catalog transaction");
                    tokio::time::sleep(retry_time).await;
                }
            }
        }

        // and remove the persisted data from memory
        namespace.mark_persisted_and_remove_if_empty(
            &partition_info.table_name,
            &partition_info.partition.partition_key,
        );
    }
}

/// Data of a Shard
#[derive(Default)]
pub struct SequencerData {
    // New namespaces can come in at any time so we need to be able to add new ones
    namespaces: RwLock<BTreeMap<String, Arc<NamespaceData>>>,
}

impl SequencerData {
    /// Store the write or delete in the sequencer. Deletes will
    /// be written into the catalog before getting stored in the buffer.
    /// Any writes that create new IOx partitions will have those records
    /// created in the catalog before putting into the buffer.
    pub async fn buffer_operation(
        &self,
        dml_operation: DmlOperation,
        sequencer_id: SequencerId,
        catalog: &dyn Catalog,
    ) -> Result<()> {
        let namespace_data = match self.namespace(dml_operation.namespace()) {
            Some(d) => d,
            None => {
                self.insert_namespace(dml_operation.namespace(), catalog)
                    .await?
            }
        };

        namespace_data
            .buffer_operation(dml_operation, sequencer_id, catalog)
            .await
    }

    /// Gets the namespace data out of the map
    pub fn namespace(&self, namespace: &str) -> Option<Arc<NamespaceData>> {
        let n = self.namespaces.read();
        n.get(namespace).cloned()
    }

    /// Retrieves the namespace from the catalog and initializes an empty buffer, or
    /// retrieves the buffer if some other caller gets it first
    async fn insert_namespace(
        &self,
        namespace: &str,
        catalog: &dyn Catalog,
    ) -> Result<Arc<NamespaceData>> {
        let mut txn = catalog.start_transaction().await.context(CatalogSnafu)?;
        let namespace = txn
            .namespaces()
            .get_by_name(namespace)
            .await
            .context(CatalogSnafu)?
            .context(NamespaceNotFoundSnafu { namespace })?;
        txn.commit().await.context(CatalogSnafu)?;

        let mut n = self.namespaces.write();
        let data = Arc::clone(
            n.entry(namespace.name)
                .or_insert_with(|| Arc::new(NamespaceData::new(namespace.id))),
        );

        Ok(data)
    }
}

/// Data of a Namespace that belongs to a given Shard
pub struct NamespaceData {
    namespace_id: NamespaceId,
    tables: RwLock<BTreeMap<String, Arc<TableData>>>,
}

impl NamespaceData {
    /// Initialize new tables with default partition template of daily
    pub fn new(namespace_id: NamespaceId) -> Self {
        Self {
            namespace_id,
            tables: Default::default(),
        }
    }

    /// Buffer the operation in the cache, adding any new partitions or delete tombstones to the caatalog
    pub async fn buffer_operation(
        &self,
        dml_operation: DmlOperation,
        sequencer_id: SequencerId,
        catalog: &dyn Catalog,
    ) -> Result<()> {
        let sequence_number = dml_operation
            .meta()
            .sequence()
            .expect("must have sequence number")
            .number;
        let sequence_number = i64::try_from(sequence_number).expect("sequence out of bounds");
        let sequence_number = SequenceNumber::new(sequence_number);

        match dml_operation {
            DmlOperation::Write(write) => {
                for (t, b) in write.into_tables() {
                    let table_data = match self.table_data(&t) {
                        Some(t) => t,
                        None => self.insert_table(&t, catalog).await?,
                    };
                    table_data
                        .buffer_table_write(sequence_number, b, sequencer_id, catalog)
                        .await?;
                }

                Ok(())
            }
            DmlOperation::Delete(delete) => {
                let table_name = delete.table_name().context(TableNotPresentSnafu)?;
                let table_data = match self.table_data(table_name) {
                    Some(t) => t,
                    None => self.insert_table(table_name, catalog).await?,
                };

                table_data
                    .buffer_delete(delete.predicate(), sequencer_id, sequence_number, catalog)
                    .await
            }
        }
    }

    /// Gets the buffered table data
    pub fn table_data(&self, table_name: &str) -> Option<Arc<TableData>> {
        let t = self.tables.read();
        t.get(table_name).cloned()
    }

    /// Inserts the table or returns it if it happens to be inserted by some other thread
    async fn insert_table(
        &self,
        table_name: &str,
        catalog: &dyn Catalog,
    ) -> Result<Arc<TableData>> {
        let mut txn = catalog.start_transaction().await.context(CatalogSnafu)?;
        let table = txn
            .tables()
            .create_or_get(table_name, self.namespace_id)
            .await
            .context(CatalogSnafu)?;
        txn.commit().await.context(CatalogSnafu)?;

        let mut t = self.tables.write();
        let data = Arc::clone(
            t.entry(table.name)
                .or_insert_with(|| Arc::new(TableData::new(table.id))),
        );

        Ok(data)
    }

    /// Walks down the table and partition and clears the persisting batch. If there is no
    /// data buffered in the partition, it is removed. If there are no other partitions in
    /// the table, it is removed.
    fn mark_persisted_and_remove_if_empty(&self, table_name: &str, partition_key: &str) {
        let mut tables = self.tables.write();
        let table = tables.get(table_name).cloned();

        if let Some(t) = table {
            let mut partitions = t.partition_data.write();
            let partition = partitions.get(partition_key).cloned();

            if let Some(p) = partition {
                let mut data = p.inner.write();
                data.persisting = None;
                if data.is_empty() {
                    partitions.remove(partition_key);
                }
            }

            if partitions.is_empty() {
                tables.remove(table_name);
            }
        }
    }
}

/// Data of a Table in a given Namesapce that belongs to a given Shard
pub struct TableData {
    table_id: TableId,
    // Map pf partition key to its data
    partition_data: RwLock<BTreeMap<String, Arc<PartitionData>>>,
}

impl TableData {
    /// Initialize new table buffer
    pub fn new(table_id: TableId) -> Self {
        Self {
            table_id,
            partition_data: Default::default(),
        }
    }

    async fn buffer_table_write(
        &self,
        sequence_number: SequenceNumber,
        batch: MutableBatch,
        sequencer_id: SequencerId,
        catalog: &dyn Catalog,
    ) -> Result<()> {
        let (_, col) = batch
            .columns()
            .find(|(name, _)| *name == TIME_COLUMN_NAME)
            .unwrap();
        let timestamp = match col.data() {
            ColumnData::I64(_, s) => s.min.unwrap(),
            _ => return Err(Error::TimeColumnNotPresent),
        };

        let partition_key = format!(
            "{}",
            Utc.timestamp_nanos(timestamp)
                .format_with_items(StrftimeItems::new("%Y-%m-%d"))
        );

        let partition_data = match self.partition_data(&partition_key) {
            Some(p) => p,
            None => {
                self.insert_partition(&partition_key, sequencer_id, catalog)
                    .await?
            }
        };

        partition_data.buffer_write(sequence_number, batch);

        Ok(())
    }

    async fn buffer_delete(
        &self,
        predicate: &DeletePredicate,
        sequencer_id: SequencerId,
        sequence_number: SequenceNumber,
        catalog: &dyn Catalog,
    ) -> Result<()> {
        let min_time = Timestamp::new(predicate.range.start());
        let max_time = Timestamp::new(predicate.range.end());

        let mut txn = catalog.start_transaction().await.context(CatalogSnafu)?;
        let tombstone = txn
            .tombstones()
            .create_or_get(
                self.table_id,
                sequencer_id,
                sequence_number,
                min_time,
                max_time,
                &predicate.expr_sql_string(),
            )
            .await
            .context(CatalogSnafu)?;
        txn.commit().await.context(CatalogSnafu)?;

        let partitions = self.partition_data.read();
        for data in partitions.values() {
            data.buffer_tombstone(tombstone.clone());
        }

        Ok(())
    }

    /// Gets the buffered partition data
    pub fn partition_data(&self, partition_key: &str) -> Option<Arc<PartitionData>> {
        let p = self.partition_data.read();
        p.get(partition_key).cloned()
    }

    async fn insert_partition(
        &self,
        partition_key: &str,
        sequencer_id: SequencerId,
        catalog: &dyn Catalog,
    ) -> Result<Arc<PartitionData>> {
        let mut txn = catalog.start_transaction().await.context(CatalogSnafu)?;
        let partition = txn
            .partitions()
            .create_or_get(partition_key, sequencer_id, self.table_id)
            .await
            .context(CatalogSnafu)?;
        txn.commit().await.context(CatalogSnafu)?;
        let mut p = self.partition_data.write();
        let data = Arc::new(PartitionData::new(partition.id));
        p.insert(partition.partition_key, Arc::clone(&data));

        Ok(data)
    }
}

/// Data of an IOx Partition of a given Table of a Namesapce that belongs to a given Shard
pub struct PartitionData {
    id: PartitionId,
    inner: RwLock<DataBuffer>,
}

impl PartitionData {
    /// Initialize a new partition data buffer
    pub fn new(id: PartitionId) -> Self {
        Self {
            id,
            inner: Default::default(),
        }
    }

    /// Snapshot anything in the buffer and move all snapshot data into a persisting batch
    pub fn snapshot_to_persisting_batch(
        &self,
        sequencer_id: SequencerId,
        table_id: TableId,
        partition_id: PartitionId,
        table_name: &str,
    ) -> Arc<PersistingBatch> {
        let mut data = self.inner.write();
        data.snapshot_to_persisting(sequencer_id, table_id, partition_id, table_name)
    }

    /// Clears the persisting batch and returns true if there is no other data in the partition.
    fn clear_persisting(&self) -> bool {
        let mut d = self.inner.write();
        d.persisting = None;

        d.snapshots.is_empty() && d.buffer.is_empty()
    }

    /// Snapshot whatever is in the buffer and return a new vec of the
    /// arc cloned snapshots
    pub fn snapshot(&self) -> Result<Vec<Arc<SnapshotBatch>>> {
        let mut data = self.inner.write();
        data.snapshot().context(SnapshotSnafu)?;
        Ok(data.snapshots.to_vec())
    }

    fn buffer_write(&self, sequencer_number: SequenceNumber, mb: MutableBatch) {
        let mut data = self.inner.write();
        data.buffer.push(BufferBatch {
            sequencer_number,
            data: mb,
        })
    }

    fn buffer_tombstone(&self, tombstone: Tombstone) {
        let mut data = self.inner.write();
        data.deletes.push(tombstone);
    }
}

/// Data of an IOx partition split into batches
/// ┌────────────────────────┐        ┌────────────────────────┐      ┌─────────────────────────┐
/// │         Buffer         │        │       Snapshots        │      │       Persisting        │
/// │  ┌───────────────────┐ │        │                        │      │                         │
/// │  │  ┌───────────────┐│ │        │ ┌───────────────────┐  │      │  ┌───────────────────┐  │
/// │  │ ┌┴──────────────┐│├─┼────────┼─┼─▶┌───────────────┐│  │      │  │  ┌───────────────┐│  │
/// │  │┌┴──────────────┐├┘│ │        │ │ ┌┴──────────────┐││  │      │  │ ┌┴──────────────┐││  │
/// │  ││  BufferBatch  ├┘ │ │        │ │┌┴──────────────┐├┘│──┼──────┼─▶│┌┴──────────────┐├┘│  │
/// │  │└───────────────┘  │ │    ┌───┼─▶│ SnapshotBatch ├┘ │  │      │  ││ SnapshotBatch ├┘ │  │
/// │  └───────────────────┘ │    │   │ │└───────────────┘  │  │      │  │└───────────────┘  │  │
/// │          ...           │    │   │ └───────────────────┘  │      │  └───────────────────┘  │
/// │  ┌───────────────────┐ │    │   │                        │      │                         │
/// │  │  ┌───────────────┐│ │    │   │          ...           │      │           ...           │
/// │  │ ┌┴──────────────┐││ │    │   │                        │      │                         │
/// │  │┌┴──────────────┐├┘│─┼────┘   │ ┌───────────────────┐  │      │  ┌───────────────────┐  │
/// │  ││  BufferBatch  ├┘ │ │        │ │  ┌───────────────┐│  │      │  │  ┌───────────────┐│  │
/// │  │└───────────────┘  │ │        │ │ ┌┴──────────────┐││  │      │  │ ┌┴──────────────┐││  │
/// │  └───────────────────┘ │        │ │┌┴──────────────┐├┘│──┼──────┼─▶│┌┴──────────────┐├┘│  │
/// │                        │        │ ││ SnapshotBatch ├┘ │  │      │  ││ SnapshotBatch ├┘ │  │
/// │          ...           │        │ │└───────────────┘  │  │      │  │└───────────────┘  │  │
/// │                        │        │ └───────────────────┘  │      │  └───────────────────┘  │
/// └────────────────────────┘        └────────────────────────┘      └─────────────────────────┘
#[derive(Default)]
pub struct DataBuffer {
    /// Buffer of incoming writes
    pub buffer: Vec<BufferBatch>,

    /// Buffer of tombstones whose time range may overlap with this partition.
    /// These tombstone first will be written into the Catalog and then here.
    /// When a persist is called, these tombstones will be moved into the
    /// PersistingBatch to get applied in those data.
    pub deletes: Vec<Tombstone>,

    /// Data in `buffer` will be moved to a `snapshot` when one of these happens:
    ///  . A background persist is called
    ///  . A read request from Querier
    /// The `buffer` will be empty when this happens.
    pub snapshots: Vec<Arc<SnapshotBatch>>,
    /// When a persist is called, data in `buffer` will be moved to a `snapshot`
    /// and then all `snapshots` will be moved to a `persisting`.
    /// Both `buffer` and 'snaphots` will be empty when this happens.
    pub persisting: Option<Arc<PersistingBatch>>,
    // Extra Notes:
    //  . In MVP, we will only persist a set of sanpshots at a time.
    //    In later version, multiple perssiting operations may be happenning concurrently but
    //    their persisted info must be added into the Catalog in thier data
    //    ingesting order.
    //  . When a read request comes from a Querier, all data from `snaphots`
    //    and `persisting` must be sent to the Querier.
    //  . After the `persiting` data is persisted and successfully added
    //    into the Catalog, it will be removed from this Data Buffer.
    //    This data might be added into an extra cache to serve up to
    //    Queriers that may not have loaded the parquet files from object
    //    storage yet. But this will be decided after MVP.
}

impl DataBuffer {
    /// Move `BufferBatch`es to a `SnapshotBatch`.
    pub fn snapshot(&mut self) -> Result<(), mutable_batch::Error> {
        if !self.buffer.is_empty() {
            let min_sequencer_number = self
                .buffer
                .first()
                .expect("Buffer isn't empty in this block")
                .sequencer_number;
            let max_sequencer_number = self
                .buffer
                .last()
                .expect("Buffer isn't empty in this block")
                .sequencer_number;
            assert!(min_sequencer_number <= max_sequencer_number);

            let mut batches = self.buffer.iter();
            let first_batch = batches.next().expect("Buffer isn't empty in this block");
            let mut mutable_batch = first_batch.data.clone();

            for batch in batches {
                mutable_batch.extend_from(&batch.data)?;
            }

            self.snapshots.push(Arc::new(SnapshotBatch {
                min_sequencer_number,
                max_sequencer_number,
                data: Arc::new(mutable_batch.to_arrow(Selection::All)?),
            }));

            self.buffer.clear();
        }

        Ok(())
    }

    /// Returns true if there are no batches in the buffer or snapshots or persisting data
    fn is_empty(&self) -> bool {
        self.snapshots.is_empty() && self.buffer.is_empty() && self.persisting.is_none()
    }

    /// Snapshots the buffer and moves snapshots over to the `PersistingBatch`. Returns error
    /// if there is already a persisting batch.
    pub fn snapshot_to_persisting(
        &mut self,
        sequencer_id: SequencerId,
        table_id: TableId,
        partition_id: PartitionId,
        table_name: &str,
    ) -> Arc<PersistingBatch> {
        if self.persisting.is_some() {
            panic!("Unable to snapshot while persisting. This is an unexpected state.")
        }

        self.snapshot()
            .expect("This mutable batch snapshot error should be impossible.");

        let mut data = vec![];
        std::mem::swap(&mut data, &mut self.snapshots);
        let mut deletes = vec![];
        std::mem::swap(&mut deletes, &mut self.deletes);

        let queryable_batch = QueryableBatch::new(table_name, data, deletes);

        let persisting_batch = Arc::new(PersistingBatch {
            sequencer_id,
            table_id,
            partition_id,
            object_store_id: Uuid::new_v4(),
            data: Arc::new(queryable_batch),
        });

        self.persisting = Some(Arc::clone(&persisting_batch));

        persisting_batch
    }

    /// Add a persiting batch into the buffer persisting list
    /// Note: For now, there is at most one persisting batch at a time but
    /// the plan is to process several of them a time as needed
    pub fn add_persisting_batch(&mut self, batch: Arc<PersistingBatch>) -> Result<()> {
        if self.persisting.is_some() {
            return Err(Error::PersistingNotEmpty);
        } else {
            self.persisting = Some(batch);
        }

        Ok(())
    }

    /// Remove the given PersistingBatch that was persisted
    pub fn remove_persisting_batch(&mut self, batch: &Arc<PersistingBatch>) -> Result<()> {
        if let Some(persisting_batch) = &self.persisting {
            if persisting_batch == batch {
                // found. Remove this batch from the memory
                self.persisting = None;
            } else {
                return Err(Error::PersistingNotMatch);
            }
        } else {
            return Err(Error::PersistingEmpty);
        }

        Ok(())
    }
}

/// BufferBatch is a MutauableBatch with its ingesting order, sequencer_number, that
/// helps the ingester keep the batches of data in thier ingesting order
pub struct BufferBatch {
    /// Sequencer number of the ingesting data
    pub sequencer_number: SequenceNumber,
    /// Ingesting data
    pub data: MutableBatch,
}

/// SnapshotBatch contains data of many contiguous BufferBatches
#[derive(Debug, PartialEq)]
pub struct SnapshotBatch {
    /// Min sequencer number of its combined BufferBatches
    pub min_sequencer_number: SequenceNumber,
    /// Max sequencer number of its combined BufferBatches
    pub max_sequencer_number: SequenceNumber,
    /// Data of its comebined BufferBatches kept in one RecordBatch
    pub data: Arc<RecordBatch>,
}

/// PersistingBatch contains all needed info and data for creating
/// a parquet file for given set of SnapshotBatches
#[derive(Debug, PartialEq)]
pub struct PersistingBatch {
    /// Sesquencer id of the data
    pub sequencer_id: SequencerId,

    /// Table id of the data
    pub table_id: TableId,

    /// Parittion Id of the data
    pub partition_id: PartitionId,

    /// Id of to-be-created parquet file of this data
    pub object_store_id: Uuid,

    /// data
    pub data: Arc<QueryableBatch>,
}

/// Queryable data used for both query and persistence
#[derive(Debug, PartialEq)]
pub struct QueryableBatch {
    /// data
    pub data: Vec<Arc<SnapshotBatch>>,

    /// Tomstones to be applied on data
    pub deletes: Vec<Tombstone>,

    /// Delete predicates of the tombstones
    /// Note: this is needed here to return its reference for a trait function
    pub delete_predicates: Vec<Arc<DeletePredicate>>,

    /// This is needed to return a reference for a trait function
    pub table_name: String,
}

/// Request received from the query service for data the ingester has
#[derive(Debug)]
pub struct IngesterQueryRequest {
    /// Table to search
    table: String,
    /// Columns the query service is interested in
    columns: Vec<String>,
    /// Start time of the query
    min_time: i64,
    /// End time of the query
    max_time: i64,
    /// Predicate for filtering
    predicate: Option<Predicate>,
    /// Optionally only return rows with a sequence number greater than this
    greater_than_sequence_number: Option<u64>,
}

impl TryFrom<proto::IngesterQueryRequest> for IngesterQueryRequest {
    type Error = FieldViolation;

    fn try_from(proto: proto::IngesterQueryRequest) -> Result<Self, Self::Error> {
        let proto::IngesterQueryRequest {
            table,
            columns,
            min_time,
            max_time,
            // predicate,
            greater_than_sequence_number,
        } = proto;

        // let predicate = predicate.try_into()?;

        Ok(Self {
            table,
            columns,
            min_time,
            max_time,
            predicate: None,
            greater_than_sequence_number,
        })
    }
}

/// Struct to manage returning data to a query request
#[derive(Debug)]
#[allow(missing_copy_implementations)]
pub struct QueryData {}

#[cfg(test)]
mod tests {
    use super::*;
    use data_types::sequence::Sequence;
    use dml::{DmlMeta, DmlWrite};
    use futures::TryStreamExt;
    use iox_catalog::interface::NamespaceSchema;
    use iox_catalog::mem::MemCatalog;
    use iox_catalog::validate_or_insert_schema;
    use mutable_batch_lp::lines_to_batches;
    use mutable_batch_lp::test_helpers::lp_to_mutable_batch;
    use object_store::ObjectStoreApi;
    use std::ops::DerefMut;
    use test_helpers::assert_error;
    use time::Time;

    #[test]
    fn convert_query_proto_to_rust() {
        let proto = proto::IngesterQueryRequest {
            table: "cpu".into(),
            columns: vec!["usage".into(), "time".into()],
            min_time: 1,
            max_time: 20,
            // predicate: None,
            greater_than_sequence_number: None,
        };

        let _query: IngesterQueryRequest = proto.try_into().unwrap();
    }

    #[test]
    fn snapshot_empty_buffer_adds_no_snapshots() {
        let mut data_buffer = DataBuffer::default();

        data_buffer.snapshot().unwrap();

        assert!(data_buffer.snapshots.is_empty());
    }

    #[test]
    fn snapshot_buffer_one_buffer_batch_moves_to_snapshots() {
        let mut data_buffer = DataBuffer::default();

        let seq_num1 = SequenceNumber::new(1);
        let (_, mutable_batch1) =
            lp_to_mutable_batch(r#"foo,t1=asdf iv=1i,uv=774u,fv=1.0,bv=true,sv="hi" 1"#);
        let buffer_batch1 = BufferBatch {
            sequencer_number: seq_num1,
            data: mutable_batch1,
        };
        let record_batch1 = buffer_batch1.data.to_arrow(Selection::All).unwrap();
        data_buffer.buffer.push(buffer_batch1);

        data_buffer.snapshot().unwrap();

        assert!(data_buffer.buffer.is_empty());
        assert_eq!(data_buffer.snapshots.len(), 1);

        let snapshot = &data_buffer.snapshots[0];
        assert_eq!(snapshot.min_sequencer_number, seq_num1);
        assert_eq!(snapshot.max_sequencer_number, seq_num1);
        assert_eq!(&*snapshot.data, &record_batch1);
    }

    #[test]
    fn snapshot_buffer_multiple_buffer_batches_combines_into_a_snapshot() {
        let mut data_buffer = DataBuffer::default();

        let seq_num1 = SequenceNumber::new(1);
        let (_, mut mutable_batch1) =
            lp_to_mutable_batch(r#"foo,t1=asdf iv=1i,uv=774u,fv=1.0,bv=true,sv="hi" 1"#);
        let buffer_batch1 = BufferBatch {
            sequencer_number: seq_num1,
            data: mutable_batch1.clone(),
        };
        data_buffer.buffer.push(buffer_batch1);

        let seq_num2 = SequenceNumber::new(2);
        let (_, mutable_batch2) =
            lp_to_mutable_batch(r#"foo,t1=aoeu iv=2i,uv=1u,fv=12.0,bv=false,sv="bye" 10000"#);
        let buffer_batch2 = BufferBatch {
            sequencer_number: seq_num2,
            data: mutable_batch2.clone(),
        };
        data_buffer.buffer.push(buffer_batch2);

        data_buffer.snapshot().unwrap();

        assert!(data_buffer.buffer.is_empty());
        assert_eq!(data_buffer.snapshots.len(), 1);

        let snapshot = &data_buffer.snapshots[0];
        assert_eq!(snapshot.min_sequencer_number, seq_num1);
        assert_eq!(snapshot.max_sequencer_number, seq_num2);

        mutable_batch1.extend_from(&mutable_batch2).unwrap();
        let combined_record_batch = mutable_batch1.to_arrow(Selection::All).unwrap();
        assert_eq!(&*snapshot.data, &combined_record_batch);
    }

    #[test]
    fn snapshot_buffer_different_but_compatible_schemas() {
        let mut data_buffer = DataBuffer::default();

        let seq_num1 = SequenceNumber::new(1);
        // Missing tag `t1`
        let (_, mut mutable_batch1) =
            lp_to_mutable_batch(r#"foo iv=1i,uv=774u,fv=1.0,bv=true,sv="hi" 1"#);
        let buffer_batch1 = BufferBatch {
            sequencer_number: seq_num1,
            data: mutable_batch1.clone(),
        };
        data_buffer.buffer.push(buffer_batch1);

        let seq_num2 = SequenceNumber::new(2);
        // Missing field `iv`
        let (_, mutable_batch2) =
            lp_to_mutable_batch(r#"foo,t1=aoeu uv=1u,fv=12.0,bv=false,sv="bye" 10000"#);
        let buffer_batch2 = BufferBatch {
            sequencer_number: seq_num2,
            data: mutable_batch2.clone(),
        };
        data_buffer.buffer.push(buffer_batch2);

        data_buffer.snapshot().unwrap();

        assert!(data_buffer.buffer.is_empty());
        assert_eq!(data_buffer.snapshots.len(), 1);

        let snapshot = &data_buffer.snapshots[0];
        assert_eq!(snapshot.min_sequencer_number, seq_num1);
        assert_eq!(snapshot.max_sequencer_number, seq_num2);

        mutable_batch1.extend_from(&mutable_batch2).unwrap();
        let combined_record_batch = mutable_batch1.to_arrow(Selection::All).unwrap();
        assert_eq!(&*snapshot.data, &combined_record_batch);
    }

    #[test]
    fn snapshot_buffer_error_leaves_data_buffer_as_is() {
        let mut data_buffer = DataBuffer::default();

        let seq_num1 = SequenceNumber::new(1);
        let (_, mutable_batch1) =
            lp_to_mutable_batch(r#"foo,t1=asdf iv=1i,uv=774u,fv=1.0,bv=true,sv="hi" 1"#);
        let buffer_batch1 = BufferBatch {
            sequencer_number: seq_num1,
            data: mutable_batch1,
        };
        data_buffer.buffer.push(buffer_batch1);

        let seq_num2 = SequenceNumber::new(2);
        // Create a type mismatch
        let (_, mutable_batch2) = lp_to_mutable_batch(r#"foo iv=false 10000"#);
        let buffer_batch2 = BufferBatch {
            sequencer_number: seq_num2,
            data: mutable_batch2,
        };
        data_buffer.buffer.push(buffer_batch2);

        assert_error!(
            data_buffer.snapshot(),
            mutable_batch::Error::WriterError {
                source: mutable_batch::writer::Error::TypeMismatch { .. }
            }
        );

        assert_eq!(data_buffer.buffer.len(), 2);
        assert!(data_buffer.snapshots.is_empty());
    }

    #[tokio::test]
    async fn persist() {
        let catalog: Arc<dyn Catalog> = Arc::new(MemCatalog::new());
        let mut txn = catalog.start_transaction().await.unwrap();
        let kafka_topic = txn.kafka_topics().create_or_get("whatevs").await.unwrap();
        let query_pool = txn.query_pools().create_or_get("whatevs").await.unwrap();
        let kafka_partition = KafkaPartition::new(0);
        let namespace = txn
            .namespaces()
            .create("foo", "inf", kafka_topic.id, query_pool.id)
            .await
            .unwrap();
        let sequencer1 = txn
            .sequencers()
            .create_or_get(&kafka_topic, kafka_partition)
            .await
            .unwrap();
        let sequencer2 = txn
            .sequencers()
            .create_or_get(&kafka_topic, kafka_partition)
            .await
            .unwrap();
        let mut sequencers = BTreeMap::new();
        sequencers.insert(sequencer1.id, SequencerData::default());
        sequencers.insert(sequencer2.id, SequencerData::default());

        let object_store = Arc::new(ObjectStore::new_in_memory());

        let data = Arc::new(IngesterData {
            object_store: Arc::clone(&object_store),
            catalog: Arc::clone(&catalog),
            sequencers,
        });

        let schema = NamespaceSchema::new(namespace.id, kafka_topic.id, query_pool.id);

        let ignored_ts = Time::from_timestamp_millis(42);

        let w1 = DmlWrite::new(
            "foo",
            lines_to_batches("mem foo=1 10", 0).unwrap(),
            DmlMeta::sequenced(Sequence::new(1, 1), ignored_ts, None, 50),
        );
        let schema = validate_or_insert_schema(w1.tables(), &schema, txn.deref_mut())
            .await
            .unwrap()
            .unwrap();

        let w2 = DmlWrite::new(
            "foo",
            lines_to_batches("cpu foo=1 10", 1).unwrap(),
            DmlMeta::sequenced(Sequence::new(2, 1), ignored_ts, None, 50),
        );
        let _ = validate_or_insert_schema(w2.tables(), &schema, txn.deref_mut())
            .await
            .unwrap()
            .unwrap();

        let w3 = DmlWrite::new(
            "foo",
            lines_to_batches("mem foo=1 30", 2).unwrap(),
            DmlMeta::sequenced(Sequence::new(1, 2), ignored_ts, None, 50),
        );

        // close out the transaction to the mem catalog won't deadlock.
        txn.commit().await.unwrap();

        data.buffer_operation(sequencer1.id, DmlOperation::Write(w1))
            .await
            .unwrap();
        data.buffer_operation(sequencer2.id, DmlOperation::Write(w2))
            .await
            .unwrap();
        data.buffer_operation(sequencer1.id, DmlOperation::Write(w3))
            .await
            .unwrap();

        let sd = data.sequencers.get(&sequencer1.id).unwrap();
        let n = sd.namespace("foo").unwrap();
        let mem_table = n.table_data("mem").unwrap();
        assert!(n.table_data("cpu").is_some());

        let p = mem_table.partition_data("1970-01-01").unwrap();
        data.persist(p.id).await;

        // verify that a file got put into object store
        let file_paths: Vec<_> = object_store
            .list(None)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(file_paths.len(), 1);

        let mut txn = catalog.start_transaction().await.unwrap();
        // verify it put the record in the catalog
        let parquet_files = txn
            .parquet_files()
            .list_by_sequencer_greater_than(sequencer1.id, SequenceNumber::new(0))
            .await
            .unwrap();
        assert_eq!(parquet_files.len(), 1);
        let pf = parquet_files.first().unwrap();
        assert_eq!(pf.partition_id, p.id);
        assert_eq!(pf.table_id, mem_table.table_id);
        assert_eq!(pf.min_time, Timestamp::new(10));
        assert_eq!(pf.max_time, Timestamp::new(30));
        assert_eq!(pf.min_sequence_number, SequenceNumber::new(1));
        assert_eq!(pf.max_sequence_number, SequenceNumber::new(2));
        assert_eq!(pf.sequencer_id, sequencer1.id);
        assert!(!pf.to_delete);

        // verify that the partition got removed from the table because it is now empty
        assert!(mem_table.partition_data("1970-01-01").is_none());
    }
}
