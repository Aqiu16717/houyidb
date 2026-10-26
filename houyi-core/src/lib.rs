use crate::numa_vec::NumaVec;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod bus;
pub mod config;
#[cfg(feature = "cxl-memory")]
pub mod cxl_memory;
pub mod interface;
pub mod numa_vec;

/// 强制 64 字节对齐，适配 CPU Cache Line.
/// Force 64-byte alignment to fit CPU cache-line access.
pub const CACHE_LINE_SIZE: usize = 64;

#[cfg(feature = "cxl-memory")]
const MEMORY_BACKEND: &str = "cxl-memory";
#[cfg(not(feature = "cxl-memory"))]
const MEMORY_BACKEND: &str = "dram-portable";

pub fn memory_backend() -> &'static str {
    MEMORY_BACKEND
}

/// PAX 页头（第一版最小结构）.
/// Minimal PAX header for v0.1 alpha.
#[derive(Debug, Clone, Copy)]
pub struct PaxHeader {
    pub mvcc_version: u64,
    pub row_count: u32,
    pub row_bitmap_words: u32,
}

/// 标量值。
/// EN: Scalar value types.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    Text(String),
    /// Unix 时间戳（秒）。
    /// EN: Unix timestamp in seconds.
    Timestamp(i64),
    JsonText(String),
}

/// 标量谓词树。
/// EN: Scalar predicate tree (supports AND/OR nesting).
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarPredicate {
    /// 等于。
    /// EN: Equal.
    Eq(ScalarValue),
    /// 大于。
    /// EN: Greater than.
    Gt(ScalarValue),
    /// 小于。
    /// EN: Less than.
    Lt(ScalarValue),
    /// 大于等于。
    /// EN: Greater or equal.
    GtEq(ScalarValue),
    /// 小于等于。
    /// EN: Less or equal.
    LtEq(ScalarValue),
    /// IN 列表。
    /// EN: In list.
    In(Vec<ScalarValue>),
    /// 逻辑与。
    /// EN: Logical AND.
    And(Box<ScalarPredicate>, Box<ScalarPredicate>),
    /// 逻辑或。
    /// EN: Logical OR.
    Or(Box<ScalarPredicate>, Box<ScalarPredicate>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumaPolicy {
    Interleaved,
    Preferred(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionAllocation {
    pub region_id: usize,
    pub numa_node: usize,
    pub offset: usize,
    pub size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionUsage {
    pub region_id: usize,
    pub numa_node: usize,
    pub used: usize,
    pub capacity: usize,
}

struct Region {
    id: usize,
    numa_node: usize,
    capacity: usize,
    used: AtomicUsize,
}

impl Region {
    /// CAS-based lock-free allocation attempt.
    /// 无锁 CAS 分配：循环尝试更新 `used`，成功则返回偏移。
    fn try_allocate(&self, size: usize) -> Option<RegionAllocation> {
        loop {
            let current = self.used.load(Ordering::Acquire);
            let new_used = current.saturating_add(size);
            if new_used > self.capacity {
                return None;
            }
            if self
                .used
                .compare_exchange(current, new_used, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(RegionAllocation {
                    region_id: self.id,
                    numa_node: self.numa_node,
                    offset: current,
                    size,
                });
            }
        }
    }
}

pub struct RegionPool {
    policy: NumaPolicy,
    next_region_idx: AtomicUsize,
    regions: Vec<Region>,
}

#[derive(Debug, Clone)]
pub struct NumaTopologyInfo {
    pub numa_nodes: usize,
    pub source: &'static str,
}

impl RegionPool {
    pub fn new(policy: NumaPolicy, region_capacity: usize, numa_nodes: usize) -> Self {
        assert!(region_capacity > 0, "region_capacity must be > 0");
        assert!(numa_nodes > 0, "numa_nodes must be > 0");

        let mut regions = Vec::with_capacity(numa_nodes);
        for i in 0..numa_nodes {
            regions.push(Region {
                id: i,
                numa_node: i,
                capacity: region_capacity,
                used: AtomicUsize::new(0),
            });
        }
        Self {
            policy,
            next_region_idx: AtomicUsize::new(0),
            regions,
        }
    }

    /// 无锁分配：CAS 遍历 Region 直到成功或全部满。
    /// Lock-free allocation: CAS walk across regions until success or all full.
    pub fn allocate(&self, size: usize) -> Option<RegionAllocation> {
        if self.regions.is_empty() {
            return None;
        }

        match self.policy {
            NumaPolicy::Preferred(node) => self
                .regions
                .iter()
                .find(|r| r.numa_node == node)
                .and_then(|r| r.try_allocate(size)),
            NumaPolicy::Interleaved => {
                let start =
                    self.next_region_idx.fetch_add(1, Ordering::AcqRel) % self.regions.len();
                for step in 0..self.regions.len() {
                    let idx = (start + step) % self.regions.len();
                    if let Some(allocation) = self.regions[idx].try_allocate(size) {
                        return Some(allocation);
                    }
                }
                None
            }
        }
    }

    /// 无锁快照：各 Region 独立 load。
    /// Lock-free snapshot: per-region atomic loads.
    pub fn usage_snapshot(&self) -> Vec<RegionUsage> {
        self.regions
            .iter()
            .map(|r| RegionUsage {
                region_id: r.id,
                numa_node: r.numa_node,
                used: r.used.load(Ordering::Acquire),
                capacity: r.capacity,
            })
            .collect()
    }
}

/// NUMA 拓扑探测（P4.8 起不再依赖 hwloc）。
/// Detect NUMA topology without hwloc.
///
/// WHY 去掉 hwloc（S9 供应链裁决）：`hwloc` crate 已停止维护
/// （RUSTSEC-2024-0382，无安全升级版本），而我们只需要节点数量。
/// Linux 的 `/sys/devices/system/node/online` 是内核权威来源（格式
/// `0-3,8`）；其他平台（含 macOS/Android 单节点）返回 1。顺带移除
/// libhwloc 原生依赖，Docker/CI 不再需要安装系统库。
/// EN: hwloc is unmaintained (RUSTSEC-2024-0382) and we only need the node
/// count; Linux sysfs is authoritative, everything else is single-node.
pub fn detect_numa_topology() -> NumaTopologyInfo {
    #[cfg(target_os = "linux")]
    {
        if let Ok(raw) = std::fs::read_to_string("/sys/devices/system/node/online") {
            if let Some(nodes) = parse_online_list(raw.trim()) {
                return NumaTopologyInfo {
                    numa_nodes: nodes,
                    source: "sysfs",
                };
            }
        }
        NumaTopologyInfo {
            numa_nodes: 1,
            source: "fallback-single-node",
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        NumaTopologyInfo {
            numa_nodes: 1,
            source: "single-node-platform",
        }
    }
}

/// 解析内核 online 列表（`0-3,8`）→ 数量；空/非法返回 None。
/// EN: parse a kernel online list (`0-3,8`) into a count.
/// 非 Linux 构建仅测试使用（生产路径返回单节点）。
#[cfg(any(target_os = "linux", test))]
fn parse_online_list(raw: &str) -> Option<usize> {
    let mut count = 0usize;
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start.trim().parse().ok()?;
            let end: usize = end.trim().parse().ok()?;
            // 先 checked 求区间长度再累加：`end - start + 1` 在 start=0、
            // end=usize::MAX 时会先溢出（debug panic / release 回绕）。
            // EN: compute the span with checked arithmetic before adding.
            let span = end.checked_sub(start)?.checked_add(1)?;
            count = count.checked_add(span)?;
        } else {
            part.parse::<usize>().ok()?;
            count = count.checked_add(1)?;
        }
    }
    (count > 0).then_some(count)
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaxRowView {
    pub scalars: HashMap<String, ScalarValue>,
    pub vector: Option<Vec<f32>>,
}

/// 基于 PAX 逻辑的内存页.
/// Memory page abstraction for PAX layout.
pub struct HouyiPage {
    capacity: usize,
    numa_node: Option<usize>,
    header: PaxHeader,
    scalar_columns: HashMap<String, Vec<ScalarValue>>,
    /// 列式连续存储的向量数据。支持 NUMA 绑定分配。
    /// Columnar vector data; supports NUMA-bound allocation.
    vector_column: NumaVec,
    /// 向量维度（存储向量时 implicit，检索时推断）。
    /// Vector dimensions; implicit per stored row.
    vector_dim: Option<usize>,
}

impl HouyiPage {
    pub fn new(size: usize) -> Self {
        Self {
            capacity: size,
            numa_node: None,
            header: PaxHeader {
                mvcc_version: 0,
                row_count: 0,
                row_bitmap_words: 0,
            },
            scalar_columns: HashMap::new(),
            vector_column: NumaVec::new(),
            vector_dim: None,
        }
    }

    pub fn new_in_region(size: usize, allocation: RegionAllocation) -> Self {
        let mut page = Self {
            capacity: size,
            numa_node: Some(allocation.numa_node),
            header: PaxHeader {
                mvcc_version: 0,
                row_count: 0,
                row_bitmap_words: 0,
            },
            scalar_columns: HashMap::new(),
            vector_column: NumaVec::with_numa_node(allocation.numa_node, allocation.size),
            vector_dim: None,
        };
        page.numa_node = Some(allocation.numa_node);
        page
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn header(&self) -> PaxHeader {
        self.header
    }

    pub fn numa_node(&self) -> Option<usize> {
        self.numa_node
    }

    /// 返回已存储向量的维度（若无向量则返回 None）。
    /// EN: Returns the dimension of stored vectors (None if there are none).
    pub fn vector_dim(&self) -> Option<usize> {
        self.vector_dim
    }

    pub fn insert_row(
        &mut self,
        scalars: HashMap<String, ScalarValue>,
        vector: Option<Vec<f32>>,
    ) -> usize {
        let row_idx = self.header.row_count as usize;
        for (name, value) in scalars {
            let column = self.scalar_columns.entry(name).or_default();
            // 新列必须先回填再推值：本行首次出现的列此前不存在，若直接 push，
            // 值会落在行 0 槽位，整列与行号错位（过滤按位置求值、回读按位置
            // 取行，全部张冠李戴——TS SDK 集成测试抓到的真实数据腐坏）。
            // 对已有列 len == row_idx 是不变量，此循环自然空转。
            // EN: A column first seen on THIS row must be backfilled before the
            // push, or the value lands in row 0's slot and the whole column is
            // shifted (positional filter eval and read-back both go wrong).
            while column.len() < row_idx {
                column.push(ScalarValue::JsonText("null".to_string()));
            }
            column.push(value);
        }

        // 为缺失列补齐占位值，保证 PAX 列长度一致。
        // Pad columns absent from this row so every column has row_idx+1 entries.
        for column in self.scalar_columns.values_mut() {
            while column.len() < row_idx + 1 {
                column.push(ScalarValue::JsonText("null".to_string()));
            }
        }

        if let Some(v) = vector {
            if !v.is_empty() {
                let dims = v.len();
                if let Some(existing) = self.vector_dim {
                    debug_assert_eq!(existing, dims, "vector dims mismatch");
                } else {
                    self.vector_dim = Some(dims);
                }
                self.vector_column.extend_from_slice(&v);
            }
        }

        self.header.row_count += 1;
        self.header.row_bitmap_words = ((self.header.row_count as usize).div_ceil(64)) as u32;
        row_idx
    }

    /// 按行索引获取向量切片（列式连续存储中的子切片）。
    /// EN: Gets the vector slice for a row index (subslice of the contiguous columnar store).
    pub fn vector_slice(&self, row_idx: usize) -> Option<&[f32]> {
        let dims = self.vector_dim?;
        let start = row_idx.checked_mul(dims)?;
        let end = start + dims;
        self.vector_column.get(start..end)
    }

    pub fn get_row(&self, row_idx: usize) -> Option<PaxRowView> {
        if row_idx >= self.header.row_count as usize {
            return None;
        }
        let mut scalars = HashMap::new();
        for (name, values) in &self.scalar_columns {
            if let Some(value) = values.get(row_idx) {
                scalars.insert(name.clone(), value.clone());
            }
        }
        let vector = self.vector_slice(row_idx).map(|s| s.to_vec());
        Some(PaxRowView { scalars, vector })
    }

    pub fn scalar_column(&self, name: &str) -> Option<&[ScalarValue]> {
        self.scalar_columns.get(name).map(|x| x.as_slice())
    }

    /// 返回所有向量（平坦切片）和维度。
    /// EN: Returns all vectors (flat slice) and the dimension.
    pub fn vector_column(&self) -> &[f32] {
        self.vector_column.as_slice()
    }

    /// 标量过滤并输出 bitset（用匹配 row id 列表表示）。
    /// Apply scalar predicate tree and emit bitset as row-id list.
    /// 支持 Eq/Gt/Lt/GtEq/LtEq/In/And/Or。
    pub fn filter_to_bitset(&self, column: &str, predicate: &ScalarPredicate) -> Vec<u64> {
        let Some(values) = self.scalar_columns.get(column) else {
            return Vec::new();
        };
        let total = self.header.row_count as usize;
        let mut bitset = vec![false; total];
        eval_predicate(predicate, values, &mut bitset);
        bitset
            .iter()
            .enumerate()
            .filter_map(|(idx, &match_)| if match_ { Some(idx as u64) } else { None })
            .collect()
    }

    /// 多列过滤（每列独立谓词，结果按 AND 合并）。
    /// Multi-column filter: each column has its own predicate, combined with AND.
    ///
    /// 未知列 ⇒ 结果为空（与 [`filter_to_bitset`] 的单列口径一致）。旧实现把查不到
    /// 的列当作「无此条件」跳过，于是 `{"不存在": ...}` 与其它条件 AND 时被静默丢弃，
    /// 返回比用户要求更宽的结果集。
    /// EN: An unknown column yields the empty set, matching the single-column
    /// contract. The old code skipped missing columns, silently widening the
    /// result set instead of intersecting with nothing.
    pub fn filter_multi_column(&self, filters: &[(&str, &ScalarPredicate)]) -> Vec<u64> {
        let total = self.header.row_count as usize;
        if filters.is_empty() || total == 0 {
            return Vec::new();
        }
        let mut bitset = vec![true; total];
        for (column, pred) in filters {
            let Some(values) = self.scalar_columns.get(*column) else {
                return Vec::new();
            };
            let mut col_match = vec![false; total];
            eval_predicate(pred, values, &mut col_match);
            for i in 0..total {
                bitset[i] = bitset[i] && col_match[i];
            }
        }
        bitset
            .iter()
            .enumerate()
            .filter_map(|(idx, &match_)| if match_ { Some(idx as u64) } else { None })
            .collect()
    }
}

/// 递归评估谓词树。
/// EN: Recursively evaluate predicate tree.
fn eval_predicate(pred: &ScalarPredicate, values: &[ScalarValue], bitset: &mut [bool]) {
    match pred {
        ScalarPredicate::And(left, right) => {
            let mut left_bits = vec![false; bitset.len()];
            let mut right_bits = vec![false; bitset.len()];
            eval_predicate(left, values, &mut left_bits);
            eval_predicate(right, values, &mut right_bits);
            for i in 0..bitset.len() {
                bitset[i] = left_bits[i] && right_bits[i];
            }
        }
        ScalarPredicate::Or(left, right) => {
            let mut left_bits = vec![false; bitset.len()];
            let mut right_bits = vec![false; bitset.len()];
            eval_predicate(left, values, &mut left_bits);
            eval_predicate(right, values, &mut right_bits);
            for i in 0..bitset.len() {
                bitset[i] = left_bits[i] || right_bits[i];
            }
        }
        ScalarPredicate::In(list) => {
            for (idx, v) in values.iter().enumerate() {
                if list.iter().any(|expect| scalar_eq(expect, v)) {
                    bitset[idx] = true;
                }
            }
        }
        _ => {
            for (idx, v) in values.iter().enumerate() {
                bitset[idx] = match_pred(pred, v);
            }
        }
    }
}

/// 单值谓词匹配。
/// EN: Single-value predicate match.
fn match_pred(pred: &ScalarPredicate, current: &ScalarValue) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match pred {
        ScalarPredicate::Eq(expect) => scalar_eq(current, expect),
        ScalarPredicate::Gt(expect) => compare(current, expect) == Some(Greater),
        ScalarPredicate::Lt(expect) => compare(current, expect) == Some(Less),
        ScalarPredicate::GtEq(expect) => matches!(compare(current, expect), Some(Greater | Equal)),
        ScalarPredicate::LtEq(expect) => matches!(compare(current, expect), Some(Less | Equal)),
        _ => false,
    }
}

/// 标量相等：数值类跨 `Int64`/`Float64` 按数值语义。
/// Scalar equality — numeric variants compare by value across Int64/Float64.
///
/// 为什么需要（WHY）：写入侧把 JSON `10` 转成 `Int64`、`10.0` 转成 `Float64`，
/// 同一列可以合法地混有两种变体；若 `eq 10` / `in [10]` 只做变体匹配，就会
/// 漏掉 `Float64(10.0)` 的行，而 `gte 10` 却能命中——同一过滤路径上自相矛盾。
/// 语义与范围比较同口径：只在数值类之间提升，布尔/文本/JsonText 保持结构相等。
/// EN: writes map JSON `10` to Int64 and `10.0` to Float64, so a column can
/// legitimately mix variants; variant-strict equality would miss Float64(10.0)
/// while range ops match it. Numeric promotion only; other variants stay strict.
fn scalar_eq(a: &ScalarValue, b: &ScalarValue) -> bool {
    use ScalarValue::*;
    match (a, b) {
        (Int64(x), Float64(y)) => (*x as f64) == *y,
        (Float64(x), Int64(y)) => *x == (*y as f64),
        _ => a == b,
    }
}

/// 标量值比较；**类型不可比时返回 `None`**（不再退化为 `Equal`）。
/// Typed scalar comparison — returns `None` when the operands are not comparable,
/// instead of collapsing to `Equal`.
///
/// 为什么必须区分「相等」与「不可比」（WHY）：旧实现以 `_ => Equal` 兜底，而
/// `GtEq`/`LtEq` 把 `Equal` 视为命中——于是 `price >= 5` 会命中 payload 里**没有**
/// price 的行（PAX 对缺列以 `JsonText("null")` 补齐占位）以及 price 是字符串/布尔
/// 的行。这是过滤路径上的静默假阳性：客户端拿到从未满足条件的文档（由
/// `tests/filter_oracle.rs` 的差分对照 + TLP 三分区属性抓出）。
/// EN: The old `_ => Equal` fallback made `GtEq`/`LtEq` (which accept `Equal`) match
/// rows whose payload lacks the column (PAX pads missing columns with
/// `JsonText("null")`) or holds a different type — silent false positives on the
/// filter path (found by the differential + TLP oracle in tests/filter_oracle.rs).
///
/// 可比的类型组合 / Comparable pairs：
/// - 数值类内部互比：`Int64`↔`Int64`、`Float64`↔`Float64`、以及 `Int64`↔`Float64`
///   按 f64 提升（超过 ±2^53 的整数会损失精度——JSON 数字本身也无法无损承载，
///   与 houyi-server 写入侧的转换口径一致）。
/// - `Timestamp`↔`Timestamp`（Unix 秒）；`Text`↔`Text`（字典序）。
/// - 其余一律不可比：布尔、`JsonText`（数组/对象/null 载荷）、以及任何跨类组合。
fn compare(a: &ScalarValue, b: &ScalarValue) -> Option<std::cmp::Ordering> {
    use ScalarValue::*;
    match (a, b) {
        (Int64(x), Int64(y)) => Some(x.cmp(y)),
        (Float64(x), Float64(y)) => Some(x.total_cmp(y)),
        (Int64(x), Float64(y)) => Some((*x as f64).total_cmp(y)),
        (Float64(x), Int64(y)) => Some(x.total_cmp(&(*y as f64))),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp(y)),
        (Text(x), Text(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

// TODO: NUMA 亲和分配（基于 Region 池）.
// TODO: NUMA-aware allocation using Region pools.

#[cfg(test)]
mod tests {
    use super::{
        detect_numa_topology, memory_backend, parse_online_list, HouyiPage, NumaPolicy, RegionPool,
        ScalarPredicate, ScalarValue, CACHE_LINE_SIZE,
    };
    use std::collections::HashMap;

    #[test]
    fn page_alignment_and_header_defaults() {
        let page = HouyiPage::new(4096);
        assert_eq!(page.capacity(), 4096);
        assert_eq!(page.header().mvcc_version, 0);
        assert_eq!(page.header().row_count, 0);
        assert_eq!(page.header().row_bitmap_words, 0);
        assert_eq!(CACHE_LINE_SIZE, 64);
    }

    #[test]
    fn insert_and_read_pax_row() {
        let mut page = HouyiPage::new(4096);
        let mut scalars = HashMap::new();
        scalars.insert("id".to_string(), ScalarValue::Int64(7));
        scalars.insert("tag".to_string(), ScalarValue::Text("p1".to_string()));
        let row = page.insert_row(scalars, Some(vec![0.1, 0.2, 0.3, 0.4]));
        assert_eq!(row, 0);
        assert_eq!(page.header().row_count, 1);
        assert_eq!(page.header().row_bitmap_words, 1);

        let row_view = page.get_row(0).expect("row should exist");
        assert_eq!(row_view.scalars.get("id"), Some(&ScalarValue::Int64(7)));
        assert_eq!(
            row_view.scalars.get("tag"),
            Some(&ScalarValue::Text("p1".to_string()))
        );
        assert_eq!(row_view.vector, Some(vec![0.1, 0.2, 0.3, 0.4]));
    }

    /// 回归（TS SDK 集成测试发现）：某行首次引入的列必须先按行号回填占位，
    /// 否则值落在行 0 槽位——过滤按位置求值、回读按位置取行全部错位。
    /// EN: A column first introduced by a later row must be backfilled; otherwise
    /// its value lands in row 0's slot and every positional read goes wrong.
    #[test]
    fn new_column_introduced_by_later_row_stays_row_aligned() {
        let mut page = HouyiPage::new(4096);
        let mut row0 = HashMap::new();
        row0.insert("a".to_string(), ScalarValue::Int64(1));
        page.insert_row(row0, None);
        let mut row1 = HashMap::new();
        row1.insert("b".to_string(), ScalarValue::Int64(2));
        page.insert_row(row1, None);

        // 列 b：行 0 为缺失占位，行 1 才是真值。
        // EN: Column b: row 0 is the missing placeholder; row 1 holds the real value.
        let b = page.scalar_column("b").expect("b column");
        assert_eq!(b.len(), 2);
        assert!(matches!(&b[0], ScalarValue::JsonText(s) if s == "null"));
        assert_eq!(b[1], ScalarValue::Int64(2));

        // 过滤 b==2 只能命中行 1（错位实现会命中行 0）。
        // EN: Filtering b==2 must match only row 1 (a misaligned impl would match row 0).
        assert_eq!(
            page.filter_to_bitset("b", &ScalarPredicate::Eq(ScalarValue::Int64(2))),
            vec![1]
        );
        // 回读行 0 不得看到 b 的真值。
        // EN: Reading row 0 back must not expose b's real value.
        let view0 = page.get_row(0).expect("row 0");
        assert!(matches!(
            view0.scalars.get("b"),
            Some(ScalarValue::JsonText(s)) if s == "null"
        ));
        let view1 = page.get_row(1).expect("row 1");
        assert_eq!(view1.scalars.get("b"), Some(&ScalarValue::Int64(2)));
        assert!(matches!(
            view1.scalars.get("a"),
            Some(ScalarValue::JsonText(s)) if s == "null"
        ));
    }

    #[test]
    fn scalar_and_vector_columns_are_columnar() {
        let mut page = HouyiPage::new(4096);
        for i in 0..3 {
            let mut scalars = HashMap::new();
            scalars.insert("id".to_string(), ScalarValue::Int64(i));
            page.insert_row(scalars, Some(vec![i as f32, i as f32 + 1.0]));
        }
        let ids = page.scalar_column("id").expect("id column should exist");
        assert_eq!(ids.len(), 3);
        assert_eq!(page.vector_column().len(), 6); // 3 rows x 2 dims flat
        assert_eq!(page.header().row_count, 3);
    }

    #[test]
    fn scalar_filter_outputs_bitset_row_ids() {
        let mut page = HouyiPage::new(4096);
        for (id, tag) in [(1, "p0"), (2, "p1"), (3, "p1"), (4, "p2")] {
            let mut scalars = HashMap::new();
            scalars.insert("id".to_string(), ScalarValue::Int64(id));
            scalars.insert("tag".to_string(), ScalarValue::Text(tag.to_string()));
            page.insert_row(scalars, None);
        }

        let bitset = page.filter_to_bitset(
            "tag",
            &ScalarPredicate::Eq(ScalarValue::Text("p1".to_string())),
        );
        assert_eq!(bitset, vec![1, 2]);

        // 测试范围过滤。
        // EN: Test range filter.
        let range = ScalarPredicate::And(
            Box::new(ScalarPredicate::Gt(ScalarValue::Int64(1))),
            Box::new(ScalarPredicate::Lt(ScalarValue::Int64(4))),
        );
        let bitset2 = page.filter_to_bitset("id", &range);
        assert_eq!(bitset2, vec![1, 2]); // id=2 (idx1) and id=3 (idx2)

        // 测试多列 AND。
        // EN: Test multi-column AND.
        let bitset4 = page.filter_multi_column(&[
            (
                "tag",
                &ScalarPredicate::Eq(ScalarValue::Text("p1".to_string())),
            ),
            ("id", &ScalarPredicate::Gt(ScalarValue::Int64(1))),
        ]);
        assert_eq!(bitset4, vec![1, 2]); // tag=p1 (idx 1,2) AND id>1 (idx 1,2,3) → [1,2]
    }

    #[test]
    fn region_pool_interleaved_allocation_spreads_nodes() {
        let pool = RegionPool::new(NumaPolicy::Interleaved, 1024, 2);
        let a1 = pool.allocate(128).expect("alloc1");
        let a2 = pool.allocate(128).expect("alloc2");
        let a3 = pool.allocate(128).expect("alloc3");
        assert_eq!(a1.numa_node, 0);
        assert_eq!(a2.numa_node, 1);
        assert_eq!(a3.numa_node, 0);
    }

    #[test]
    fn region_pool_preferred_node_allocation() {
        let pool = RegionPool::new(NumaPolicy::Preferred(1), 512, 2);
        let alloc = pool.allocate(128).expect("alloc should succeed");
        assert_eq!(alloc.numa_node, 1);
    }

    #[test]
    fn page_can_be_tagged_with_numa_node() {
        let pool = RegionPool::new(NumaPolicy::Preferred(0), 1024, 1);
        let alloc = pool.allocate(256).expect("alloc should succeed");
        let page = HouyiPage::new_in_region(256, alloc);
        assert_eq!(page.numa_node(), Some(0));
    }

    #[test]
    fn region_pool_usage_snapshot_reports_per_node_usage() {
        let pool = RegionPool::new(NumaPolicy::Interleaved, 512, 2);
        let _ = pool.allocate(128).expect("alloc 1");
        let _ = pool.allocate(64).expect("alloc 2");
        let usage = pool.usage_snapshot();
        assert_eq!(usage.len(), 2);
        let total_used: usize = usage.iter().map(|u| u.used).sum();
        assert_eq!(total_used, 192);
    }

    #[test]
    fn detect_numa_topology_is_non_zero() {
        let info = detect_numa_topology();
        assert!(info.numa_nodes >= 1);
    }

    /// sysfs `node/online` 的常见形态与非法输入。 / EN: kernel list formats.
    #[test]
    fn online_list_parsing() {
        assert_eq!(parse_online_list("0"), Some(1));
        assert_eq!(parse_online_list("0-3"), Some(4));
        assert_eq!(parse_online_list("0-3,8"), Some(5));
        assert_eq!(parse_online_list("0,2,4-5"), Some(4));
        assert_eq!(parse_online_list(""), None);
        assert_eq!(parse_online_list("3-1"), None);
        assert_eq!(parse_online_list("x"), None);
        // 区间跨度溢出不 panic，返回 None。
        // EN: Span overflow is a clean None, without panic.
        assert_eq!(parse_online_list(&format!("0-{}", usize::MAX)), None);
    }

    #[test]
    fn memory_backend_is_defined() {
        assert!(!memory_backend().is_empty());
    }
}
