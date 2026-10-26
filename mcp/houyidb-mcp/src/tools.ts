/**
 * MCP 工具注册与 HouyiDB 请求映射。
 * EN: MCP tool registration and mapping to HouyiDB requests.
 */

import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";
import {
  assertCollectionAllowed,
  isCollectionAllowed,
  type HouyidbMcpConfig,
} from "./config.js";
import { MAX_ERROR_BODY_CHARS, MAX_SEARCH_EF, MAX_SEARCH_K } from "./constants.js";
import { HouyiResponseError, HouyidbMcpError } from "./errors.js";
import type { HouyiClient } from "./houyi-client.js";
import { truncateBody } from "./houyi-client.js";
import { collectionPath } from "./paths.js";

/**
 * collection 名称本身不能为空；白名单校验在处理器里执行。
 * EN: Collection names must be non-empty; allowlist checks run in the handler.
 */
const collectionSchema = z
  .string()
  .min(1)
  .describe("Collection name / collection 名称");

/**
 * 查询向量：至少一维且拒绝 NaN/Infinity。
 * EN: Query vector with at least one dimension and no NaN/Infinity.
 */
const vectorSchema = z
  .array(z.number().finite())
  .min(1)
  .describe("Dense query vector / 稠密查询向量");

/**
 * top-k 与服务端同边界，提前拒绝放大请求。
 * EN: Top-k shares the server bound so amplification is rejected early.
 */
const kSchema = z
  .number()
  .int()
  .min(1)
  .max(MAX_SEARCH_K)
  .describe(`Top-k, 1..${MAX_SEARCH_K} / 返回条数`);

/**
 * 可选 HNSW 搜索束宽。
 * EN: Optional HNSW search beam width.
 */
const efSearchSchema = z
  .number()
  .int()
  .min(1)
  .max(MAX_SEARCH_EF)
  .optional()
  .describe(`Optional HNSW ef_search, 1..${MAX_SEARCH_EF} / 可选 ef_search`);

/**
 * 是否回传标量 payload。
 * EN: Whether to return scalar payloads for each hit.
 */
const includePayloadSchema = z
  .boolean()
  .optional()
  .describe("Include payloads in hits (default false) / 是否回传 payload");

/**
 * 结构化过滤字面值：标量或标量数组（`in` 操作符）。
 * EN: Structured filter literal: scalar or scalar array (for the `in` operator).
 */
const filterLiteralSchema = z.union([
  z.string(),
  z.number().finite(),
  z.boolean(),
  z.array(z.union([z.string(), z.number().finite(), z.boolean()])),
]);

/**
 * 一条过滤条件，多个条件之间是 AND 交集。
 * EN: One filter condition; multiple filters are intersected with AND.
 */
const filterSchema = z.object({
  column: z
    .string()
    .min(1)
    .describe("Scalar column or reserved `label` column / 标量列名"),
  op: z
    .enum(["eq", "gt", "gte", "lt", "lte", "in"])
    .describe("Comparison operator / 比较操作符"),
  value: filterLiteralSchema.describe(
    "Comparison literal; arrays only for `in` / 比较字面值，仅 `in` 使用数组",
  ),
});

/**
 * 单个待写入向量。
 * EN: One vector to upsert.
 */
const upsertVectorSchema = z.object({
  id: z
    .number()
    .int()
    .min(0)
    .max(Number.MAX_SAFE_INTEGER)
    .describe("Vector id (safe integer range) / 向量 id（安全整数范围内）"),
  values: z.array(z.number().finite()).min(1).describe("Vector values / 向量分量"),
  label: z.string().optional().describe("Optional label / 可选标签"),
  payload: z
    .record(z.string(), z.unknown())
    .optional()
    .describe("Optional scalar payload / 可选标量 payload"),
});

/**
 * 把任意成功返回值包装成 MCP 文本结果。
 * EN: Wraps any successful value as an MCP text result.
 */
function successResult(value: unknown): CallToolResult {
  return {
    content: [
      {
        type: "text",
        text: JSON.stringify(value) ?? "null",
      },
    ],
  };
}

/**
 * 把异常转成出错结果。只暴露消息、不暴露堆栈，并限制长度。
 * EN: Converts failures into error results. Only the message (never a stack
 * trace) is exposed, and its length is capped.
 */
function errorResult(error: unknown): CallToolResult {
  const rawMessage =
    error instanceof Error ? error.message : String(error);
  const message = truncateBody(
    rawMessage.trim() || "unknown error",
    MAX_ERROR_BODY_CHARS,
  );
  return {
    content: [
      {
        type: "text",
        text: `[houyidb-mcp] ${message}`,
      },
    ],
    isError: true,
  };
}

/**
 * 统一执行工具逻辑，确保所有异常都变成可读的 `isError` 结果。
 * EN: Runs tool logic uniformly, turning every exception into a readable
 * `isError` result.
 */
async function runTool(
  operation: () => Promise<unknown>,
): Promise<CallToolResult> {
  try {
    return successResult(await operation());
  } catch (error) {
    return errorResult(error);
  }
}

/**
 * 读取上游 collection 列表并按白名单过滤。
 * EN: Reads upstream collections and filters them through the allowlist.
 */
async function listAllowedCollections(
  config: HouyidbMcpConfig,
  client: HouyiClient,
): Promise<unknown> {
  const data = await client.get("/v1/collections");
  if (!Array.isArray(data)) {
    throw new HouyiResponseError(
      "GET /v1/collections did not return a JSON array. " +
        "GET /v1/collections 未返回 JSON 数组。",
    );
  }
  if (config.collections.size === 0) return data;

  return data.filter((entry) => {
    if (typeof entry !== "object" || entry === null) return false;
    const name = (entry as { name?: unknown }).name;
    return typeof name === "string" && isCollectionAllowed(config, name);
  });
}

/**
 * 注册六类工具；只读模式只注册前四类，写工具根本不出现在 tools/list 中。
 * EN: Registers six tools; read-only mode registers only the first four, so
 * write tools never appear in tools/list at all.
 */
export function registerTools(
  server: McpServer,
  config: HouyidbMcpConfig,
  client: HouyiClient,
): void {
  server.registerTool(
    "list_collections",
    {
      title: "List collections / 列出 collections",
      description:
        "List HouyiDB collections visible under HOUYIDB_MCP_COLLECTIONS. " +
        "列出 HOUYIDB_MCP_COLLECTIONS 白名单内的 HouyiDB collections。",
      annotations: {
        readOnlyHint: true,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    () => runTool(() => listAllowedCollections(config, client)),
  );

  server.registerTool(
    "collection_status",
    {
      title: "Collection status / collection 状态",
      description:
        "Read indexed/pending counts, dimension, index type, and scalar rows " +
        "for one collection. 查询单个 collection 的已索引/待构图数量、维度、" +
        "索引类型与标量行数。",
      inputSchema: {
        collection: collectionSchema,
      },
      annotations: {
        readOnlyHint: true,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    ({ collection }) =>
      runTool(async () => {
        assertCollectionAllowed(config, collection);
        return client.get(collectionPath(collection, "status"));
      }),
  );

  server.registerTool(
    "search",
    {
      title: "Vector search / 向量检索",
      description:
        "Run top-k vector search in a collection. 在指定 collection 中执行 " +
        "top-k 向量检索。",
      inputSchema: {
        collection: collectionSchema,
        vector: vectorSchema,
        k: kSchema,
        include_payload: includePayloadSchema,
        ef_search: efSearchSchema,
      },
      annotations: {
        readOnlyHint: true,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    ({ collection, vector, k, include_payload, ef_search }) =>
      runTool(async () => {
        assertCollectionAllowed(config, collection);
        const payload: Record<string, unknown> = {
          vector,
          k,
          include_payload: include_payload ?? false,
        };
        if (ef_search !== undefined) payload.ef_search = ef_search;
        return client.post(collectionPath(collection, "search"), payload);
      }),
  );

  server.registerTool(
    "hybrid_search",
    {
      title: "Hybrid search / 混合检索",
      description:
        "Run vector search with structured scalar filters (AND semantics). " +
        "执行带结构化标量过滤（AND 交集）的向量检索。",
      inputSchema: {
        collection: collectionSchema,
        vector: vectorSchema,
        k: kSchema,
        filters: z
          .array(filterSchema)
          .optional()
          .describe("Structured filters / 结构化过滤条件"),
        include_payload: includePayloadSchema,
      },
      annotations: {
        readOnlyHint: true,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    ({ collection, vector, k, filters, include_payload }) =>
      runTool(async () => {
        assertCollectionAllowed(config, collection);
        return client.post(collectionPath(collection, "hybrid/search"), {
          vector,
          k,
          filters: filters ?? [],
          include_payload: include_payload ?? false,
        });
      }),
  );

  if (config.readOnly) return;

  server.registerTool(
    "upsert",
    {
      title: "Upsert vectors / 写入向量",
      description:
        "Upsert vectors and optional scalar payloads. The collection is created " +
        "on first write. 写入向量及可选标量 payload；collection 在首次写入时" +
        "隐式创建。",
      inputSchema: {
        collection: collectionSchema,
        vectors: z
          .array(upsertVectorSchema)
          .min(1)
          .describe("Vectors to upsert / 待写入向量"),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    ({ collection, vectors }) =>
      runTool(async () => {
        assertCollectionAllowed(config, collection);
        return client.post(collectionPath(collection, "upsert"), { vectors });
      }),
  );

  server.registerTool(
    "delete",
    {
      title: "Delete vectors / 删除向量",
      description:
        "Delete by ids and/or structured filters (the two sets are unioned). " +
        "按 id 和/或结构化过滤条件删除（两者取并集），至少提供其一。",
      inputSchema: {
        collection: collectionSchema,
        ids: z
          .array(z.number().int().min(0).max(Number.MAX_SAFE_INTEGER))
          .optional()
          .describe("Vector ids / 向量 id 列表"),
        filters: z
          .array(filterSchema)
          .optional()
          .describe("Structured filters / 结构化过滤条件"),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: true,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    ({ collection, ids, filters }) =>
      runTool(async () => {
        assertCollectionAllowed(config, collection);
        const idList = ids ?? [];
        const filterList = filters ?? [];
        if (idList.length === 0 && filterList.length === 0) {
          throw new HouyidbMcpError(
            "delete requires at least one id or filter. " +
              "delete 至少需要一个 id 或一条过滤条件。",
          );
        }
        return client.post(collectionPath(collection, "delete"), {
          ids: idList,
          filters: filterList,
        });
      }),
  );
}
