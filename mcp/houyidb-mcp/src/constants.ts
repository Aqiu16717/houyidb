/**
 * 服务级常量：版本、超时与 HouyiDB 的检索边界。
 * EN: Service constants: version, timeout, and HouyiDB search limits.
 */

export const SERVER_NAME = "houyidb-mcp";

// 版本号与 package.json / server.json 保持一致；发布时三处必须同步。
// EN: Keep the version in sync with package.json and server.json on release.
export const SERVER_VERSION = "0.1.0";

// 默认连接本机服务；客户端可通过 HOUYIDB_URL 覆盖。
// EN: Defaults to the local service; clients may override it with HOUYIDB_URL.
export const DEFAULT_BASE_URL = "http://127.0.0.1:17000";

// 单次上游请求的超时，避免 MCP 客户端无限等待失联服务。
// EN: Per-request upstream timeout so MCP clients never wait forever.
export const REQUEST_TIMEOUT_MS = 30_000;

// 错误正文上限，避免把超长诊断整体灌进模型上下文。
// EN: Cap error bodies so oversized diagnostics do not flood the model context.
export const MAX_ERROR_BODY_CHARS = 4_096;

// 与服务端 MAX_SEARCH_K 同口径，提前失败并给出可读原因。
// EN: Mirrors the server's MAX_SEARCH_K so failures are immediate and readable.
export const MAX_SEARCH_K = 10_000;

// 与服务端 MAX_SEARCH_EF 同口径，避免请求侧放大预分配。
// EN: Mirrors the server's MAX_SEARCH_EF to avoid request-side allocation blowups.
export const MAX_SEARCH_EF = 10_000;
