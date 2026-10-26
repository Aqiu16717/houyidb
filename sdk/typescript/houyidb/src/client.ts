/**
 * HouyiDB collections HTTP API 客户端（Node 18+ 全局 fetch，零额外运行时依赖）。
 * EN: HouyiDB collections HTTP API client (Node 18+ global fetch, no extra
 * runtime dependencies).
 */

import { stringify } from "./json.js";
import {
  Filter,
  HouyiError,
  SearchResult,
  toVectorId,
  Vector,
  VectorIdInput,
  vectorToJson,
} from "./types.js";

export interface HouyiClientOptions {
  /**
   * 单次请求超时毫秒数（默认 30000）。
   * EN: Per-request timeout in milliseconds (default 30000).
   */
  timeoutMs?: number;
  /**
   * 自定义 fetch 实现（测试注入/代理场景）。
   * EN: Custom fetch implementation (for test injection or proxy scenarios).
   */
  fetch?: typeof fetch;
}

/**
 * collection 状态（indexed/pending/dims/index_type/scalar_row_count 等）。
 * EN: Collection status (indexed/pending/dims/index_type/scalar_row_count, etc.).
 */
export type CollectionStatus = Record<string, unknown>;

/**
 * 检索可选参数。
 * EN: Optional search parameters.
 */
export interface SearchOptions {
  efSearch?: number;
  includePayload?: boolean;
}

/**
 * 混合检索可选参数。
 * EN: Optional hybrid search parameters.
 */
export interface HybridSearchOptions extends SearchOptions {
  filters?: Filter[];
}

/**
 * 删除参数：ids 与 filters 至少给其一（两者取并集）。
 * EN: Delete parameters: at least one of ids or filters (the two are unioned).
 */
export interface DeleteOptions {
  ids?: VectorIdInput[];
  filters?: Filter[];
}

/**
 * 一个 named collection 的操作句柄（经 HouyiClient.collection 获取）。
 * EN: Operation handle for a named collection (obtained via HouyiClient.collection).
 */
export class Collection {
  readonly #client: HouyiClient;
  readonly name: string;
  /**
   * URL 路径段必须转义：未编码的 `/`/`?`/`#` 会改写路由结构，
   * 表现为莫名其妙的 404 且 collection 永远建不出来。
   * EN: URL path segments must be escaped: unencoded `/`, `?`, or `#` rewrite
   * the routing structure, producing a puzzling 404 where the collection can
   * never be created.
   */
  readonly #quoted: string;

  constructor(client: HouyiClient, name: string) {
    this.#client = client;
    this.name = name;
    this.#quoted = encodeURIComponent(name);
  }

  /**
   * 写入一批向量（collection 首次写入时隐式创建）。返回插入条数。
   * EN: Write a batch of vectors (the collection is created implicitly on the
   * first write). Returns the number of inserted rows.
   */
  async upsert(vectors: Array<Vector | Record<string, unknown>>): Promise<number> {
    const docs = vectors.map((v) =>
      "values" in v && "id" in v ? vectorToJson(v as Vector) : v,
    );
    const body = await this.#client.post<{ inserted: number }>(
      `/v1/collections/${this.#quoted}/upsert`,
      { vectors: docs },
    );
    return body.inserted;
  }

  /**
   * 纯向量 top-k 检索。
   * EN: Pure vector top-k search.
   */
  async search(
    vector: number[],
    k: number,
    opts: SearchOptions = {},
  ): Promise<SearchResult> {
    const req: Record<string, unknown> = { vector, k };
    if (opts.efSearch !== undefined) req.ef_search = opts.efSearch;
    if (opts.includePayload) req.include_payload = true;
    return this.#client.postSearch(`/v1/collections/${this.#quoted}/search`, req);
  }

  /**
   * 向量 × 标量过滤的混合检索（filters 取交集，仅 hnsw/flat 后端支持）。
   * EN: Hybrid search combining a vector with scalar filters (filters are
   * intersected; only the hnsw/flat backends are supported).
   */
  async hybridSearch(
    vector: number[],
    k: number,
    opts: HybridSearchOptions = {},
  ): Promise<SearchResult> {
    const req: Record<string, unknown> = { vector, k };
    if (opts.filters && opts.filters.length > 0) req.filters = opts.filters;
    if (opts.efSearch !== undefined) req.ef_search = opts.efSearch;
    if (opts.includePayload) req.include_payload = true;
    return this.#client.postSearch(
      `/v1/collections/${this.#quoted}/hybrid/search`,
      req,
    );
  }

  /**
   * 按 id 和/或过滤条件删除（幂等；两者取并集）。返回真实删除数。
   *
   * 按条件删除由服务端在标量页上求值：标量行随 upsert 同步落页，因此
   * 刚写入、尚在异步构图队列中的向量也能被命中删除——这是「先检索拿 id
   * 再删」无法保证的（检索在队列排空前看不到新写入）。
   *
   * EN: Delete by ids and/or filter conditions (idempotent; the two are
   * unioned). Returns the actual number deleted.
   *
   * EN: Filter-based deletion is evaluated by the server on the scalar pages:
   * scalar rows land on pages synchronously with upsert, so vectors just
   * written and still in the async graph-building queue can also be matched
   * and deleted—something "search first, then delete by id" cannot guarantee
   * (search cannot see new writes before the queue drains).
   */
  async delete(opts: DeleteOptions): Promise<number> {
    const ids = (opts.ids ?? []).map(toVectorId);
    const filters = opts.filters ?? [];
    if (ids.length === 0 && filters.length === 0) {
      throw new Error("delete requires at least one of ids or filters");
    }
    const body = await this.#client.post<{ deleted: number }>(
      `/v1/collections/${this.#quoted}/delete`,
      { ids, filters },
    );
    return body.deleted;
  }

  /**
   * collection 状态（indexed/pending/dims/index_type/scalar_row_count）。
   * EN: Collection status (indexed/pending/dims/index_type/scalar_row_count).
   */
  async status(): Promise<CollectionStatus> {
    return this.#client.get<CollectionStatus>(
      `/v1/collections/${this.#quoted}/status`,
    );
  }
}

/**
 * HouyiDB 客户端入口。
 * EN: HouyiDB client entry point.
 */
export class HouyiClient {
  readonly #baseUrl: string;
  readonly #timeoutMs: number;
  readonly #fetch: typeof fetch;

  constructor(baseUrl = "http://127.0.0.1:7000", opts: HouyiClientOptions = {}) {
    this.#baseUrl = baseUrl.replace(/\/+$/, "");
    this.#timeoutMs = opts.timeoutMs ?? 30_000;
    this.#fetch = opts.fetch ?? fetch;
  }

  collection(name: string): Collection {
    return new Collection(this, name);
  }

  /**
   * 服务健康检查（不可达/非 ok 一律 false，不抛）。
   * EN: Service health check (unreachable or non-ok always yields false, never throws).
   */
  async health(): Promise<boolean> {
    try {
      const body = await this.get<{ status?: string }>("/v1/health");
      return body.status === "ok";
    } catch {
      return false;
    }
  }

  /**
   * 列出全部 collection。
   * EN: List all collections.
   */
  async listCollections(): Promise<Array<Record<string, unknown>>> {
    return this.get<Array<Record<string, unknown>>>("/v1/collections");
  }

  /**
   * 主索引双速写入指标（hot/pending/flush_count 等，可观测性用）。
   * EN: Primary index dual-speed write metrics (hot/pending/flush_count, etc.,
   * for observability).
   */
  async indexStatus(): Promise<Record<string, unknown>> {
    return this.get<Record<string, unknown>>("/v1/index/status");
  }

  // ── 内部传输 ──────────────────────────────────────────
  // EN: Internal transport.

  /** @internal */
  async get<T>(path: string): Promise<T> {
    const resp = await this.#fetch(`${this.#baseUrl}${path}`, {
      signal: AbortSignal.timeout(this.#timeoutMs),
    });
    return HouyiClient.checked<T>(resp);
  }

  /** @internal */
  async post<T>(path: string, payload: unknown): Promise<T> {
    const resp = await this.#fetch(`${this.#baseUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: stringify(payload),
      signal: AbortSignal.timeout(this.#timeoutMs),
    });
    return HouyiClient.checked<T>(resp);
  }

  /**
   * 检索专用 POST：响应走 SearchResult.fromRawJson（ids 防 2^53 精度腐蚀）。
   * @internal
   * EN: Search-specific POST: responses go through SearchResult.fromRawJson
   * (ids are protected against 2^53 precision corruption).
   */
  async postSearch(path: string, payload: unknown): Promise<SearchResult> {
    const resp = await this.#fetch(`${this.#baseUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: stringify(payload),
      signal: AbortSignal.timeout(this.#timeoutMs),
    });
    if (resp.status >= 400) {
      throw new HouyiError(resp.status, (await resp.text()).trim() || resp.statusText);
    }
    return SearchResult.fromRawJson(await resp.text());
  }

  private static async checked<T>(resp: Response): Promise<T> {
    if (resp.status >= 400) {
      // 服务端错误体是纯文本诊断（axum 元组错误），原样透出。
      // EN: The server error body is a plain-text diagnostic (axum tuple
      // rejection); surface it verbatim.
      throw new HouyiError(resp.status, (await resp.text()).trim() || resp.statusText);
    }
    return (await resp.json()) as T;
  }
}
