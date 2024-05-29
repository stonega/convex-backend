use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use common::{
    bootstrap_model::index::{
        vector_index::{
            DeveloperVectorIndexConfig,
            FragmentedVectorSegment,
            VectorIndexBackfillState,
            VectorIndexSnapshot,
            VectorIndexSnapshotData,
            VectorIndexState,
        },
        IndexConfig,
        TabletIndexMetadata,
    },
    document::{
        ParsedDocument,
        ResolvedDocument,
    },
    persistence::{
        DocumentStream,
        RepeatablePersistence,
    },
    runtime::{
        try_join_buffer_unordered,
        Runtime,
    },
    types::IndexId,
};
use search::{
    disk_index::upload_vector_segment,
    fragmented_segment::{
        MutableFragmentedSegmentMetadata,
        PreviousVectorSegments,
    },
    metrics::SearchType,
};
use storage::Storage;
use value::InternalId;
use vector::{
    qdrant_segments::VectorDiskSegmentValues,
    QdrantSchema,
};

use crate::{
    index_workers::index_meta::{
        BackfillState,
        PreviousSegmentsType,
        SearchIndex,
        SearchIndexConfig,
        SearchIndexConfigParser,
        SearchOnDiskState,
        SearchSnapshot,
        SegmentStatistics,
        SegmentType,
        SnapshotData,
    },
    Snapshot,
};

pub struct VectorIndexConfigParser;

impl SearchIndexConfigParser for VectorIndexConfigParser {
    type IndexType = VectorSearchIndex;

    fn get_config(config: IndexConfig) -> Option<SearchIndexConfig<Self::IndexType>> {
        let IndexConfig::Vector {
            on_disk_state,
            developer_config,
        } = config
        else {
            return None;
        };
        Some(SearchIndexConfig {
            developer_config,
            on_disk_state: SearchOnDiskState::from(on_disk_state),
        })
    }
}

impl From<VectorIndexState> for SearchOnDiskState<VectorSearchIndex> {
    fn from(value: VectorIndexState) -> Self {
        match value {
            VectorIndexState::Backfilling(backfill_state) => {
                SearchOnDiskState::Backfilling(backfill_state.into())
            },
            VectorIndexState::Backfilled(snapshot) => {
                SearchOnDiskState::Backfilled(snapshot.into())
            },
            VectorIndexState::SnapshottedAt(snapshot) => {
                SearchOnDiskState::SnapshottedAt(snapshot.into())
            },
        }
    }
}

impl TryFrom<SearchOnDiskState<VectorSearchIndex>> for VectorIndexState {
    type Error = anyhow::Error;

    fn try_from(value: SearchOnDiskState<VectorSearchIndex>) -> anyhow::Result<Self> {
        Ok(match value {
            SearchOnDiskState::Backfilling(state) => Self::Backfilling(state.into()),
            SearchOnDiskState::Backfilled(snapshot) => Self::Backfilled(snapshot.try_into()?),
            SearchOnDiskState::SnapshottedAt(snapshot) => Self::SnapshottedAt(snapshot.try_into()?),
        })
    }
}

impl SegmentType<VectorSearchIndex> for FragmentedVectorSegment {
    fn id(&self) -> &str {
        &self.id
    }

    fn num_deleted(&self) -> u64 {
        self.num_deleted as u64
    }

    fn statistics(&self) -> anyhow::Result<VectorStatistics> {
        let non_deleted_vectors = self.non_deleted_vectors()?;
        Ok(VectorStatistics {
            non_deleted_vectors,
            num_vectors: self.num_vectors,
        })
    }
}

#[derive(Clone, Debug)]
pub struct VectorSearchIndex;

impl PreviousSegmentsType for PreviousVectorSegments {
    fn maybe_delete_document(&mut self, convex_id: InternalId) -> anyhow::Result<()> {
        self.maybe_delete_convex(convex_id)
    }
}

#[derive(Clone)]
pub struct BuildVectorIndexArgs {
    /// The maximum vector segment size at which it's reasonable to search the
    /// segment by simply iterating over every item individually.
    ///
    /// This is only used for vector search where:
    /// 1. We want to avoid the CPU  cost of building an expensive HNSW segment
    ///    for small segments
    /// 2. It's more accurate/efficient to perform a linear scan than use HNSW
    ///    anyway.
    pub full_scan_threshold_bytes: usize,
}

#[async_trait]
impl SearchIndex for VectorSearchIndex {
    type BuildIndexArgs = BuildVectorIndexArgs;
    type DeveloperConfig = DeveloperVectorIndexConfig;
    type NewSegment = VectorDiskSegmentValues;
    type PreviousSegments = PreviousVectorSegments;
    type Schema = QdrantSchema;
    type Segment = FragmentedVectorSegment;
    type Statistics = VectorStatistics;

    fn get_index_sizes(snapshot: Snapshot) -> anyhow::Result<BTreeMap<IndexId, usize>> {
        Ok(snapshot
            .vector_indexes
            .backfilled_and_enabled_index_sizes()?
            .collect())
    }

    fn is_version_current(snapshot: &SearchSnapshot<Self>) -> bool {
        snapshot.data.is_version_current()
    }

    fn new_schema(config: &Self::DeveloperConfig) -> Self::Schema {
        QdrantSchema::new(config)
    }

    async fn download_previous_segments<RT: Runtime>(
        rt: RT,
        storage: Arc<dyn Storage>,
        segments: Vec<Self::Segment>,
    ) -> anyhow::Result<Self::PreviousSegments> {
        let segments = try_join_buffer_unordered(
            rt,
            "upload_vector_metadata",
            segments.into_iter().map(move |segment| {
                MutableFragmentedSegmentMetadata::download(segment, storage.clone())
            }),
        )
        .await?;
        Ok(PreviousVectorSegments(segments))
    }

    async fn upload_previous_segments<RT: Runtime>(
        rt: RT,
        storage: Arc<dyn Storage>,
        segments: Self::PreviousSegments,
    ) -> anyhow::Result<Vec<Self::Segment>> {
        try_join_buffer_unordered(
            rt,
            "upload_vector_metadata",
            segments
                .0
                .into_iter()
                .map(move |segment| segment.upload_deleted_bitset(storage.clone())),
        )
        .await
    }

    fn estimate_document_size(schema: &Self::Schema, _doc: &ResolvedDocument) -> u64 {
        schema.estimate_vector_size() as u64
    }

    async fn build_disk_index(
        schema: &Self::Schema,
        index_path: &PathBuf,
        documents: DocumentStream<'_>,
        _reader: RepeatablePersistence,
        previous_segments: &mut Self::PreviousSegments,
        BuildVectorIndexArgs {
            full_scan_threshold_bytes,
        }: Self::BuildIndexArgs,
    ) -> anyhow::Result<Option<Self::NewSegment>> {
        schema
            .build_disk_index(
                index_path,
                documents,
                full_scan_threshold_bytes,
                previous_segments,
            )
            .await
    }

    async fn upload_new_segment<RT: Runtime>(
        rt: &RT,
        storage: Arc<dyn Storage>,
        new_segment: Self::NewSegment,
    ) -> anyhow::Result<Self::Segment> {
        upload_vector_segment(rt, storage, new_segment).await
    }

    fn extract_metadata(
        metadata: ParsedDocument<TabletIndexMetadata>,
    ) -> anyhow::Result<(Self::DeveloperConfig, SearchOnDiskState<Self>)> {
        let (on_disk_state, developer_config) = match metadata.into_value().config {
            IndexConfig::Database { .. } | IndexConfig::Search { .. } => {
                anyhow::bail!("Index type changed!");
            },
            IndexConfig::Vector {
                on_disk_state,
                developer_config,
            } => (on_disk_state, developer_config),
        };

        Ok((developer_config, SearchOnDiskState::from(on_disk_state)))
    }

    fn new_index_config(
        developer_config: Self::DeveloperConfig,
        new_state: SearchOnDiskState<Self>,
    ) -> anyhow::Result<IndexConfig> {
        let on_disk_state = VectorIndexState::try_from(new_state)?;
        Ok(IndexConfig::Vector {
            on_disk_state,
            developer_config,
        })
    }

    fn search_type() -> SearchType {
        SearchType::Vector
    }
}

#[derive(Debug, Default)]
pub struct VectorStatistics {
    pub num_vectors: u32,
    pub non_deleted_vectors: u64,
}

impl SegmentStatistics for VectorStatistics {
    fn add(lhs: anyhow::Result<Self>, rhs: anyhow::Result<Self>) -> anyhow::Result<Self> {
        let rhs = rhs?;
        let lhs = lhs?;
        Ok(Self {
            num_vectors: lhs.num_vectors + rhs.num_vectors,
            non_deleted_vectors: lhs.non_deleted_vectors + rhs.non_deleted_vectors,
        })
    }

    fn num_documents(&self) -> u64 {
        self.num_vectors as u64
    }

    fn num_non_deleted_documents(&self) -> u64 {
        self.non_deleted_vectors
    }
}

impl From<VectorIndexBackfillState> for BackfillState<VectorSearchIndex> {
    fn from(value: VectorIndexBackfillState) -> Self {
        Self {
            segments: value.segments,
            cursor: value.cursor,
            backfill_snapshot_ts: value.backfill_snapshot_ts,
        }
    }
}

impl From<BackfillState<VectorSearchIndex>> for VectorIndexBackfillState {
    fn from(value: BackfillState<VectorSearchIndex>) -> Self {
        Self {
            segments: value.segments,
            cursor: value.cursor,
            backfill_snapshot_ts: value.backfill_snapshot_ts,
        }
    }
}

impl From<VectorIndexSnapshot> for SearchSnapshot<VectorSearchIndex> {
    fn from(snapshot: VectorIndexSnapshot) -> Self {
        Self {
            ts: snapshot.ts,
            data: SnapshotData::from(snapshot.data),
        }
    }
}

// TODO(CX-6589): Make this infallible
impl TryFrom<SearchSnapshot<VectorSearchIndex>> for VectorIndexSnapshot {
    type Error = anyhow::Error;

    fn try_from(value: SearchSnapshot<VectorSearchIndex>) -> anyhow::Result<Self> {
        Ok(VectorIndexSnapshot {
            data: value.data.try_into()?,
            ts: value.ts,
        })
    }
}

impl From<VectorIndexSnapshotData> for SnapshotData<FragmentedVectorSegment> {
    fn from(value: VectorIndexSnapshotData) -> Self {
        match value {
            VectorIndexSnapshotData::MultiSegment(values) => SnapshotData::MultiSegment(values),
            VectorIndexSnapshotData::Unknown(obj) => SnapshotData::Unknown(obj),
        }
    }
}

// TODO(CX-6589): Make this infallible
impl TryFrom<SnapshotData<FragmentedVectorSegment>> for VectorIndexSnapshotData {
    type Error = anyhow::Error;

    fn try_from(value: SnapshotData<FragmentedVectorSegment>) -> anyhow::Result<Self> {
        Ok(match value {
            SnapshotData::Unknown(obj) => Self::Unknown(obj),
            SnapshotData::SingleSegment(_) => {
                anyhow::bail!("Vector search can't have single segment indexes!")
            },
            SnapshotData::MultiSegment(data) => Self::MultiSegment(data),
        })
    }
}
