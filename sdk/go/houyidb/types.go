package houyidb

import "fmt"

// 与服务端的契约要点：
// - collection 在首次 upsert 时隐式创建，维度由首个向量定型；
// - payload 键 "label" 为保留列，占用会 400（用顶层 Label 字段）；
// - 检索响应的 Score 是相似度（1 - cosine_distance，越大越相似）；
// - Go 的 uint64 原生覆盖 id 键空间，无 JS 的 2^53 精度问题。
//
// EN: Contract points with the server:
// EN: - A collection is created implicitly on the first upsert; dimensions are
// fixed by the first vector;
// EN: - The payload key "label" is reserved; using it returns 400 (use the
// top-level Label field instead);
// EN: - Score in search responses is a similarity (1 - cosine_distance,
// larger is more similar);
// EN: - Go's uint64 natively covers the id key space, with no JS-style 2^53
// precision issue.

// Vector 一条待写入向量：ID + 分量 + 可选 Label/Payload。
// EN: Vector is a vector to write: ID + components + optional Label/Payload.
type Vector struct {
	ID      uint64                 `json:"id"`
	Values  []float64              `json:"values"`
	Label   string                 `json:"label,omitempty"`
	Payload map[string]interface{} `json:"payload,omitempty"`
}

// FilterOp 过滤算子白名单（与服务端一致；多条 Filter 之间取交集）。
// EN: FilterOp is the filter operator whitelist (matches the server;
// multiple Filters are intersected).
type FilterOp string

const (
	OpEq  FilterOp = "eq"
	OpGt  FilterOp = "gt"
	OpGte FilterOp = "gte"
	OpLt  FilterOp = "lt"
	OpLte FilterOp = "lte"
	OpIn  FilterOp = "in"
)

// Filter 一条结构化过滤条件。
// EN: Filter is a structured filter condition.
type Filter struct {
	Column string      `json:"column"`
	Op     FilterOp    `json:"op"`
	Value  interface{} `json:"value"`
}

// SearchHit 单条命中：ID + 相似度分数 + 可选 Payload（IncludePayload 时）。
// EN: SearchHit is a single hit: ID + similarity score + optional Payload
// (when IncludePayload is set).
type SearchHit struct {
	ID      uint64
	Score   float64
	Payload map[string]interface{}
}

// SearchResult 检索响应：Hits 按相似度从高到低排列。
// EN: SearchResult is a search response: Hits are ordered by similarity from
// high to low.
type SearchResult struct {
	Hits []SearchHit
}

// IDs 返回命中的 id 序列（便捷访问器）。
// EN: IDs returns the hit id sequence (convenience accessor).
func (r *SearchResult) IDs() []uint64 {
	ids := make([]uint64, len(r.Hits))
	for i, h := range r.Hits {
		ids[i] = h.ID
	}
	return ids
}

// Scores 返回命中的分数序列（便捷访问器）。
// EN: Scores returns the hit score sequence (convenience accessor).
func (r *SearchResult) Scores() []float64 {
	scores := make([]float64, len(r.Hits))
	for i, h := range r.Hits {
		scores[i] = h.Score
	}
	return scores
}

// searchResponse 检索响应的 wire 形状：ids/scores 平行数组 +
// payloads 以 id 十进制字符串为键（服务端契约）。
// EN: searchResponse is the wire shape of a search response: parallel
// ids/scores arrays plus payloads keyed by the decimal string form of ids
// (server contract).
type searchResponse struct {
	IDs      []uint64                          `json:"ids"`
	Scores   []float64                         `json:"scores"`
	Payloads map[string]map[string]interface{} `json:"payloads"`
}

func (r *searchResponse) toResult() *SearchResult {
	hits := make([]SearchHit, 0, len(r.IDs))
	for i, id := range r.IDs {
		hit := SearchHit{ID: id}
		if i < len(r.Scores) {
			hit.Score = r.Scores[i]
		}
		if r.Payloads != nil {
			hit.Payload = r.Payloads[fmt.Sprintf("%d", id)]
		}
		hits = append(hits, hit)
	}
	return &SearchResult{Hits: hits}
}

// HouyiError 服务端返回的非 2xx 响应（StatusCode + 服务端诊断文本）。
// EN: HouyiError is a non-2xx response returned by the server (StatusCode +
// server diagnostic text).
type HouyiError struct {
	StatusCode int
	Message    string
}

func (e *HouyiError) Error() string {
	return fmt.Sprintf("houyidb HTTP %d: %s", e.StatusCode, e.Message)
}
