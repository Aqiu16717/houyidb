pub type TaskId = u64;
pub type VectorId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Query,
    Build,
    Persist,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryTask {
    pub task_id: TaskId,
    pub sql: String,
    pub query_vector: Vec<f32>,
    pub k: usize,
    pub filter_mask: Option<Vec<VectorId>>,
    /// 单次查询覆盖 HNSW `ef_search`（与 HTTP `ef_search` 对齐）；`None` 用索引默认。
    /// Per-query beam override; `None` uses index default.
    pub ef_search_override: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BuildTask {
    pub task_id: TaskId,
    pub vector_ids: Vec<VectorId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersistTask {
    pub task_id: TaskId,
    pub wal_payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BusTask {
    Query(QueryTask),
    Build(BuildTask),
    Persist(PersistTask),
}

impl BusTask {
    pub fn kind(&self) -> TaskKind {
        match self {
            BusTask::Query(_) => TaskKind::Query,
            BusTask::Build(_) => TaskKind::Build,
            BusTask::Persist(_) => TaskKind::Persist,
        }
    }
}

pub trait VectorStore: Send + Sync {
    fn search(
        &self,
        query_vector: &[f32],
        k: usize,
        filter_mask: Option<&[VectorId]>,
    ) -> Vec<VectorId>;
}
