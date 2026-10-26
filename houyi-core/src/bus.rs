use crate::interface::{BuildTask, PersistTask, QueryTask};
use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub trait TaskBus<T>: Send + Sync {
    fn publish(&self, task: T) -> Result<(), T>;
    fn try_consume(&self) -> Option<T>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn capacity(&self) -> usize;
}

/// 无锁 RingBuffer 总线（基于有界 lock-free 队列）.
/// Lock-free ring-buffer bus backed by bounded array queue.
pub struct LockFreeRingBufferBus<T> {
    queue: Arc<ArrayQueue<T>>,
}

impl<T> LockFreeRingBufferBus<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(capacity)),
        }
    }
}

impl<T: Send> TaskBus<T> for LockFreeRingBufferBus<T> {
    fn publish(&self, task: T) -> Result<(), T> {
        self.queue.push(task)
    }

    fn try_consume(&self) -> Option<T> {
        self.queue.pop()
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn capacity(&self) -> usize {
        self.queue.capacity()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterMetricsSnapshot {
    pub published_query: usize,
    pub published_build: usize,
    pub published_persist: usize,
    pub consumed_query: usize,
    pub consumed_build: usize,
    pub consumed_persist: usize,
}

#[derive(Default)]
struct RouterMetrics {
    published_query: AtomicUsize,
    published_build: AtomicUsize,
    published_persist: AtomicUsize,
    consumed_query: AtomicUsize,
    consumed_build: AtomicUsize,
    consumed_persist: AtomicUsize,
}

/// 多任务路由器：将 Query/Build/Persist 分发到独立无锁队列。
/// Multi-task router dispatching to dedicated lock-free queues.
pub struct MultiTaskBusRouter {
    query_bus: LockFreeRingBufferBus<QueryTask>,
    build_bus: LockFreeRingBufferBus<BuildTask>,
    persist_bus: LockFreeRingBufferBus<PersistTask>,
    metrics: RouterMetrics,
}

impl MultiTaskBusRouter {
    pub fn new(capacity_per_bus: usize) -> Self {
        Self {
            query_bus: LockFreeRingBufferBus::new(capacity_per_bus),
            build_bus: LockFreeRingBufferBus::new(capacity_per_bus),
            persist_bus: LockFreeRingBufferBus::new(capacity_per_bus),
            metrics: RouterMetrics::default(),
        }
    }

    pub fn publish_query(&self, task: QueryTask) -> Result<(), QueryTask> {
        self.query_bus.publish(task)?;
        self.metrics.published_query.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn publish_build(&self, task: BuildTask) -> Result<(), BuildTask> {
        self.build_bus.publish(task)?;
        self.metrics.published_build.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn publish_persist(&self, task: PersistTask) -> Result<(), PersistTask> {
        self.persist_bus.publish(task)?;
        self.metrics
            .published_persist
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn try_consume_query(&self) -> Option<QueryTask> {
        let task = self.query_bus.try_consume();
        if task.is_some() {
            self.metrics.consumed_query.fetch_add(1, Ordering::Relaxed);
        }
        task
    }

    pub fn try_consume_build(&self) -> Option<BuildTask> {
        let task = self.build_bus.try_consume();
        if task.is_some() {
            self.metrics.consumed_build.fetch_add(1, Ordering::Relaxed);
        }
        task
    }

    pub fn try_consume_persist(&self) -> Option<PersistTask> {
        let task = self.persist_bus.try_consume();
        if task.is_some() {
            self.metrics
                .consumed_persist
                .fetch_add(1, Ordering::Relaxed);
        }
        task
    }

    pub fn metrics_snapshot(&self) -> RouterMetricsSnapshot {
        RouterMetricsSnapshot {
            published_query: self.metrics.published_query.load(Ordering::Relaxed),
            published_build: self.metrics.published_build.load(Ordering::Relaxed),
            published_persist: self.metrics.published_persist.load(Ordering::Relaxed),
            consumed_query: self.metrics.consumed_query.load(Ordering::Relaxed),
            consumed_build: self.metrics.consumed_build.load(Ordering::Relaxed),
            consumed_persist: self.metrics.consumed_persist.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LockFreeRingBufferBus, MultiTaskBusRouter, TaskBus};
    use crate::interface::{BuildTask, PersistTask, QueryTask};

    #[test]
    fn bus_publish_and_consume_roundtrip() {
        let bus = LockFreeRingBufferBus::new(4);
        assert_eq!(bus.capacity(), 4);
        assert_eq!(bus.len(), 0);

        bus.publish(11_u64).expect("publish should succeed");
        bus.publish(12_u64).expect("publish should succeed");
        assert_eq!(bus.len(), 2);

        assert_eq!(bus.try_consume(), Some(11));
        assert_eq!(bus.try_consume(), Some(12));
        assert_eq!(bus.try_consume(), None);
    }

    #[test]
    fn multi_task_router_routes_and_tracks_metrics() {
        let router = MultiTaskBusRouter::new(8);
        router
            .publish_query(QueryTask {
                task_id: 1,
                sql: "SELECT 1".to_string(),
                query_vector: vec![0.1, 0.2],
                k: 1,
                filter_mask: None,
                ef_search_override: None,
            })
            .unwrap();
        router
            .publish_build(BuildTask {
                task_id: 2,
                vector_ids: vec![11, 12],
            })
            .unwrap();
        router
            .publish_persist(PersistTask {
                task_id: 3,
                wal_payload: b"wal".to_vec(),
            })
            .unwrap();

        assert!(router.try_consume_query().is_some());
        assert!(router.try_consume_build().is_some());
        assert!(router.try_consume_persist().is_some());
        let m = router.metrics_snapshot();
        assert_eq!(m.published_query, 1);
        assert_eq!(m.published_build, 1);
        assert_eq!(m.published_persist, 1);
        assert_eq!(m.consumed_query, 1);
        assert_eq!(m.consumed_build, 1);
        assert_eq!(m.consumed_persist, 1);
    }
}
