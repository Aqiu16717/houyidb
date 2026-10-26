// Package houyidb 是 HouyiDB 向量数据库的官方 Go 客户端（collections HTTP API）。
// EN: Package houyidb is the official Go client for the HouyiDB vector
// database (collections HTTP API).
package houyidb

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Client HouyiDB 客户端入口。
// EN: Client is the HouyiDB client entry point.
type Client struct {
	baseURL    string
	httpClient *http.Client
}

// ClientOption 客户端可选配置。
// EN: ClientOption configures optional client settings.
type ClientOption func(*Client)

// WithTimeout 单次请求超时（连接+读取，默认 30s）。
// EN: WithTimeout sets the per-request timeout (connect + read, default 30s).
func WithTimeout(d time.Duration) ClientOption {
	return func(c *Client) { c.httpClient.Timeout = d }
}

// WithHTTPClient 注入自定义 http.Client（代理/传输层定制）。
// EN: WithHTTPClient injects a custom http.Client (proxy/transport customization).
func WithHTTPClient(hc *http.Client) ClientOption {
	return func(c *Client) { c.httpClient = hc }
}

// NewClient 构造客户端；baseURL 缺省 "http://127.0.0.1:7000"。
// EN: NewClient constructs a client; baseURL defaults to "http://127.0.0.1:7000".
func NewClient(baseURL string, opts ...ClientOption) *Client {
	if baseURL == "" {
		baseURL = "http://127.0.0.1:7000"
	}
	c := &Client{
		baseURL:    strings.TrimRight(baseURL, "/"),
		httpClient: &http.Client{Timeout: 30 * time.Second},
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// Collection 返回一个 named collection 的操作句柄。
// EN: Collection returns an operation handle for a named collection.
func (c *Client) Collection(name string) *Collection {
	return &Collection{
		client: c,
		name:   name,
		// URL 路径段必须转义：未编码的 / ? # 会改写路由结构，
		// 表现为莫名其妙的 404 且 collection 永远建不出来。
		// EN: URL path segments must be escaped: unencoded / ? # rewrite the
		// routing structure, producing a puzzling 404 where the collection can
		// never be created.
		quoted: url.PathEscape(name),
	}
}

// Health 服务健康检查（不可达/非 ok 一律 false，不返回错误）。
// EN: Health performs a service health check (unreachable or non-ok always
// yields false, without returning an error).
func (c *Client) Health(ctx context.Context) bool {
	var body struct {
		Status string `json:"status"`
	}
	if err := c.get(ctx, "/v1/health", &body); err != nil {
		return false
	}
	return body.Status == "ok"
}

// ListCollections 列出全部 collection。
// EN: ListCollections lists all collections.
func (c *Client) ListCollections(ctx context.Context) ([]map[string]interface{}, error) {
	var out []map[string]interface{}
	err := c.get(ctx, "/v1/collections", &out)
	return out, err
}

// IndexStatus 主索引双速写入指标（hot/pending/flush_count 等，可观测性用）。
// EN: IndexStatus reports primary-index dual-speed write metrics
// (hot/pending/flush_count, etc., for observability).
func (c *Client) IndexStatus(ctx context.Context) (map[string]interface{}, error) {
	var out map[string]interface{}
	err := c.get(ctx, "/v1/index/status", &out)
	return out, err
}

// ── 内部传输 ──────────────────────────────────────────
// EN: Internal transport.

func (c *Client) get(ctx context.Context, path string, out interface{}) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.baseURL+path, nil)
	if err != nil {
		return err
	}
	return c.do(req, out)
}

func (c *Client) post(ctx context.Context, path string, payload, out interface{}) error {
	body, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(
		ctx, http.MethodPost, c.baseURL+path, bytes.NewReader(body),
	)
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	return c.do(req, out)
}

func (c *Client) do(req *http.Request, out interface{}) error {
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		// 服务端错误体是纯文本诊断（axum 元组错误），原样透出。
		// EN: The server error body is a plain-text diagnostic (axum tuple
		// rejection); surface it verbatim.
		text, _ := io.ReadAll(resp.Body)
		msg := strings.TrimSpace(string(text))
		if msg == "" {
			msg = resp.Status
		}
		return &HouyiError{StatusCode: resp.StatusCode, Message: msg}
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

// Collection 一个 named collection 的操作句柄（经 Client.Collection 获取）。
// EN: Collection is an operation handle for a named collection (obtained via
// Client.Collection).
type Collection struct {
	client *Client
	name   string
	quoted string
}

// Name 返回 collection 名。
// EN: Name returns the collection name.
func (c *Collection) Name() string { return c.name }

// Upsert 写入一批向量（collection 首次写入时隐式创建）。返回插入条数。
// EN: Upsert writes a batch of vectors (the collection is created implicitly
// on the first write). Returns the number of inserted rows.
func (c *Collection) Upsert(ctx context.Context, vectors []Vector) (int, error) {
	var body struct {
		Inserted int `json:"inserted"`
	}
	err := c.client.post(ctx,
		fmt.Sprintf("/v1/collections/%s/upsert", c.quoted),
		map[string]interface{}{"vectors": vectors}, &body)
	return body.Inserted, err
}

// SearchOptions 检索可选参数（nil 即全默认）。
// EN: SearchOptions holds optional search parameters (nil means all defaults).
type SearchOptions struct {
	EFSearch       int
	IncludePayload bool
}

func (o *SearchOptions) apply(req map[string]interface{}) {
	if o == nil {
		return
	}
	if o.EFSearch > 0 {
		req["ef_search"] = o.EFSearch
	}
	if o.IncludePayload {
		req["include_payload"] = true
	}
}

// Search 纯向量 top-k 检索。
// EN: Search performs a pure vector top-k search.
func (c *Collection) Search(
	ctx context.Context, vector []float64, k int, opts *SearchOptions,
) (*SearchResult, error) {
	req := map[string]interface{}{"vector": vector, "k": k}
	opts.apply(req)
	var body searchResponse
	err := c.client.post(ctx,
		fmt.Sprintf("/v1/collections/%s/search", c.quoted), req, &body)
	if err != nil {
		return nil, err
	}
	return body.toResult(), nil
}

// HybridSearchOptions 混合检索可选参数。
// EN: HybridSearchOptions holds optional hybrid search parameters.
type HybridSearchOptions struct {
	Filters        []Filter
	EFSearch       int
	IncludePayload bool
}

// HybridSearch 向量 × 标量过滤的混合检索（Filters 取交集，仅 hnsw/flat 后端支持）。
// EN: HybridSearch performs a hybrid vector × scalar-filter search (Filters
// are intersected; only the hnsw/flat backends are supported).
func (c *Collection) HybridSearch(
	ctx context.Context, vector []float64, k int, opts *HybridSearchOptions,
) (*SearchResult, error) {
	req := map[string]interface{}{"vector": vector, "k": k}
	if opts != nil {
		if len(opts.Filters) > 0 {
			req["filters"] = opts.Filters
		}
		if opts.EFSearch > 0 {
			req["ef_search"] = opts.EFSearch
		}
		if opts.IncludePayload {
			req["include_payload"] = true
		}
	}
	var body searchResponse
	err := c.client.post(ctx,
		fmt.Sprintf("/v1/collections/%s/hybrid/search", c.quoted), req, &body)
	if err != nil {
		return nil, err
	}
	return body.toResult(), nil
}

// DeleteOptions 删除参数：IDs 与 Filters 至少给其一（两者取并集）。
// EN: DeleteOptions holds delete parameters: at least one of IDs or Filters
// (the two are unioned).
type DeleteOptions struct {
	IDs     []uint64
	Filters []Filter
}

// Delete 按 id 和/或过滤条件删除（幂等）。返回真实删除数。
//
// 按条件删除由服务端在标量页上求值：标量行随 upsert 同步落页，因此
// 刚写入、尚在异步构图队列中的向量也能被命中删除——这是「先检索拿 id
// 再删」无法保证的（检索在队列排空前看不到新写入）。
//
// EN: Delete deletes by ids and/or filter conditions (idempotent). Returns
// the actual number deleted.
//
// EN: Filter-based deletion is evaluated by the server on the scalar pages:
// scalar rows land on pages synchronously with upsert, so vectors just
// written and still in the async graph-building queue can also be matched
// and deleted—something "search first, then delete by id" cannot guarantee
// (search cannot see new writes before the queue drains).
func (c *Collection) Delete(ctx context.Context, opts *DeleteOptions) (int, error) {
	if opts == nil || (len(opts.IDs) == 0 && len(opts.Filters) == 0) {
		return 0, fmt.Errorf("delete requires at least one of ids or filters")
	}
	ids := opts.IDs
	if ids == nil {
		ids = []uint64{}
	}
	filters := opts.Filters
	if filters == nil {
		filters = []Filter{}
	}
	var body struct {
		Deleted int `json:"deleted"`
	}
	err := c.client.post(ctx,
		fmt.Sprintf("/v1/collections/%s/delete", c.quoted),
		map[string]interface{}{"ids": ids, "filters": filters}, &body)
	return body.Deleted, err
}

// Status collection 状态（indexed/pending/dims/index_type/scalar_row_count）。
// EN: Status returns the collection status
// (indexed/pending/dims/index_type/scalar_row_count).
func (c *Collection) Status(ctx context.Context) (map[string]interface{}, error) {
	var out map[string]interface{}
	err := c.client.get(ctx,
		fmt.Sprintf("/v1/collections/%s/status", c.quoted), &out)
	return out, err
}
