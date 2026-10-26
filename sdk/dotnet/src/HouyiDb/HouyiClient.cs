using System.Net;
using System.Net.Http.Json;
using System.Text.Json;

namespace HouyiDb;

/// <summary>
/// HouyiDB 客户端入口（async，内部复用一个 <see cref="HttpClient"/>）。
/// 契约要点：collection 首次 upsert 隐式创建，维度由首个向量定型；payload 键
/// "label" 为保留列；检索响应 score 为相似度（1 - cosine_distance，越大越相似）。
/// EN: HouyiDB client entry point (async, reusing one <see cref="HttpClient"/>).
/// Contract: a collection is created implicitly on first upsert and its dimension
/// is fixed by the first vector; payload key "label" is reserved; search score is
/// similarity (1 - cosine_distance, higher is closer).
/// </summary>
public sealed class HouyiClient
{
    private static readonly TimeSpan DefaultTimeout = TimeSpan.FromSeconds(30);

    private readonly HttpClient _http;
    private readonly string _baseUrl;

    /// <summary>
    /// 构造客户端；<paramref name="baseUrl"/> 如 http://127.0.0.1:7000。
    /// EN: Constructs a client; <paramref name="baseUrl"/> such as http://127.0.0.1:7000.
    /// </summary>
    public HouyiClient(string baseUrl) : this(baseUrl, null, null) { }

    /// <summary>
    /// 构造客户端并指定单次请求超时。
    /// EN: Constructs a client with a per-request timeout.
    /// </summary>
    public HouyiClient(string baseUrl, TimeSpan timeout) : this(baseUrl, null, timeout) { }

    /// <summary>
    /// 构造客户端并复用外部 HttpClient（如 IHttpClientFactory 托管实例）。
    /// EN: Constructs a client reusing an external HttpClient (e.g. an
    /// IHttpClientFactory-managed instance).
    /// </summary>
    public HouyiClient(string baseUrl, HttpClient httpClient) : this(baseUrl, httpClient, null) { }

    private HouyiClient(string baseUrl, HttpClient? httpClient, TimeSpan? timeout)
    {
        _http = httpClient ?? new HttpClient { Timeout = timeout ?? DefaultTimeout };
        if (httpClient is not null && timeout is not null)
        {
            _http.Timeout = timeout.Value;
        }
        _baseUrl = baseUrl.TrimEnd('/');
    }

    /// <summary>
    /// 返回一个 named collection 的操作句柄。
    /// EN: Returns a handle for a named collection.
    /// </summary>
    public Collection GetCollection(string name) => new(_http, $"{_baseUrl}/v1/collections/{Escape(name)}", name);

    /// <summary>
    /// 服务健康检查（不可达/非 ok 一律 false，不抛异常）。
    /// EN: Health check (returns false when unreachable or not ok; never throws).
    /// </summary>
    public async Task<bool> HealthAsync(CancellationToken ct = default)
    {
        try
        {
            var doc = await GetJsonAsync("/v1/health", ct).ConfigureAwait(false);
            return doc.RootElement.TryGetProperty("status", out var s) && s.GetString() == "ok";
        }
        catch
        {
            return false;
        }
    }

    /// <summary>
    /// 列出全部 collection。
    /// EN: Lists all collections.
    /// </summary>
    public Task<JsonDocument> ListCollectionsAsync(CancellationToken ct = default) =>
        GetJsonAsync("/v1/collections", ct);

    /// <summary>
    /// 主索引双速写入指标（hot/pending/flush_count 等，可观测性用）。
    /// EN: Primary index dual-speed write metrics (hot/pending/flush_count, for observability).
    /// </summary>
    public Task<JsonDocument> IndexStatusAsync(CancellationToken ct = default) =>
        GetJsonAsync("/v1/index/status", ct);

    internal async Task<JsonDocument> GetJsonAsync(string path, CancellationToken ct)
    {
        using var resp = await _http.GetAsync(_baseUrl + path, ct).ConfigureAwait(false);
        return await ReadJsonAsync(resp, ct).ConfigureAwait(false);
    }

    /// <summary>
    /// 非 2xx 时读取服务端纯文本诊断并抛 <see cref="HouyiException"/>。
    /// EN: Reads the server's plain-text diagnostic on non-2xx and throws
    /// <see cref="HouyiException"/>.
    /// </summary>
    internal static async Task<JsonDocument> ReadJsonAsync(HttpResponseMessage resp, CancellationToken ct)
    {
        if (!resp.IsSuccessStatusCode)
        {
            var message = (await resp.Content.ReadAsStringAsync(ct).ConfigureAwait(false)).Trim();
            throw new HouyiException((int)resp.StatusCode, message);
        }
        return await resp.Content.ReadFromJsonAsync<JsonDocument>(cancellationToken: ct).ConfigureAwait(false)
            ?? throw new HouyiException((int)resp.StatusCode, "empty response body");
    }

    /// <summary>
    /// RFC 3986 unreserved 以外一律转义（与 Python quote(safe="") 同口径）：
    /// 未编码的 / ? # 会改写路由结构，表现为莫名其妙的 404 且 collection 永远建不出来。
    /// EN: Escapes everything outside RFC 3986 unreserved (same rule as Python
    /// quote(safe="")): an unencoded / ? # rewrites the route structure, surfacing
    /// as a puzzling 404 with the collection never being created.
    /// </summary>
    internal static string Escape(string name) => Uri.EscapeDataString(name);

    /// <summary>
    /// 一个 named collection 的操作句柄（经 <see cref="GetCollection(string)"/> 获取）。
    /// EN: Handle for a named collection (obtained via <see cref="GetCollection(string)"/>).
    /// </summary>
    public sealed class Collection
    {
        private readonly HttpClient _http;
        private readonly string _base;

        internal Collection(HttpClient http, string baseUrl, string name)
        {
            _http = http;
            _base = baseUrl;
            Name = name;
        }

        /// <summary>
        /// collection 名。
        /// EN: Collection name.
        /// </summary>
        public string Name { get; }

        /// <summary>
        /// 写入一批向量（collection 首次写入时隐式创建）。返回插入条数。
        /// EN: Writes a batch of vectors (the collection is created implicitly on
        /// first write). Returns the number of inserted rows.
        /// </summary>
        public async Task<int> UpsertAsync(IReadOnlyList<Vector> vectors, CancellationToken ct = default)
        {
            using var resp = await _http
                .PostAsJsonAsync($"{_base}/upsert", new { vectors }, ct)
                .ConfigureAwait(false);
            using var doc = await ReadJsonAsync(resp, ct).ConfigureAwait(false);
            return doc.RootElement.GetProperty("inserted").GetInt32();
        }

        /// <summary>
        /// 纯向量 top-k 检索。
        /// EN: Pure vector top-k search.
        /// </summary>
        public Task<SearchResult> SearchAsync(
            float[] vector, int k, SearchOptions? options = null, CancellationToken ct = default) =>
            PostSearchAsync($"{_base}/search", vector, k, options, includeFilters: false, ct);

        /// <summary>
        /// 向量 × 标量过滤的混合检索（filters 取交集，仅 hnsw/flat 后端支持）。
        /// EN: Hybrid vector × scalar filter search (filters are intersected; only
        /// hnsw/flat backends are supported).
        /// </summary>
        public Task<SearchResult> HybridSearchAsync(
            float[] vector, int k, SearchOptions? options = null, CancellationToken ct = default) =>
            PostSearchAsync($"{_base}/hybrid/search", vector, k, options, includeFilters: true, ct);

        private async Task<SearchResult> PostSearchAsync(
            string url, float[] vector, int k, SearchOptions? options, bool includeFilters, CancellationToken ct)
        {
            var body = new Dictionary<string, object> { ["vector"] = vector, ["k"] = k };
            if (includeFilters && options?.Filters.Count > 0)
            {
                body["filters"] = options.Filters;
            }
            if (options?.EfSearch is int ef)
            {
                body["ef_search"] = ef;
            }
            if (options?.IncludePayload == true)
            {
                body["include_payload"] = true;
            }
            using var resp = await _http.PostAsJsonAsync(url, body, ct).ConfigureAwait(false);
            using var doc = await ReadJsonAsync(resp, ct).ConfigureAwait(false);
            var wire = doc.RootElement.Deserialize<SearchResult.Wire>()
                ?? new SearchResult.Wire();
            return SearchResult.FromWire(wire);
        }

        /// <summary>
        /// 按 id 和/或过滤条件删除（幂等；两者取并集）。返回真实删除数。
        /// 按条件删除由服务端在标量页上求值：标量行随 upsert 同步落页，因此刚写入、
        /// 尚在异步构图队列中的向量也能被命中删除——这是「先检索拿 id 再删」无法保证的。
        /// EN: Deletes by id and/or filters (idempotent; the two sets are unioned).
        /// Returns the number actually deleted. Filter deletes are evaluated
        /// server-side on scalar pages: scalar rows land synchronously with upsert,
        /// so vectors just written and still in the async graph-build queue can also
        /// be matched and deleted, which "search first, delete by id" cannot guarantee.
        /// </summary>
        public async Task<int> DeleteAsync(
            IReadOnlyList<ulong>? ids = null, IReadOnlyList<Filter>? filters = null, CancellationToken ct = default)
        {
            if ((ids is null || ids.Count == 0) && (filters is null || filters.Count == 0))
            {
                throw new ArgumentException("delete requires at least one of ids or filters");
            }
            using var resp = await _http
                .PostAsJsonAsync($"{_base}/delete", new { ids = ids ?? [], filters = filters ?? [] }, ct)
                .ConfigureAwait(false);
            using var doc = await ReadJsonAsync(resp, ct).ConfigureAwait(false);
            return doc.RootElement.GetProperty("deleted").GetInt32();
        }

        /// <summary>
        /// collection 状态（indexed/pending/dims/index_type/scalar_row_count）。
        /// EN: Collection status (indexed/pending/dims/index_type/scalar_row_count).
        /// </summary>
        public async Task<JsonDocument> StatusAsync(CancellationToken ct = default)
        {
            using var resp = await _http.GetAsync($"{_base}/status", ct).ConfigureAwait(false);
            return await ReadJsonAsync(resp, ct).ConfigureAwait(false);
        }
    }
}
