/**
 * MCP 服务错误类型。错误消息直接进入工具结果，因此必须是人类可读的，
 * 且绝不能包含 Authorization token。
 * EN: Error types for the MCP server. Messages flow straight into tool results,
 * so they must stay human-readable and must never contain the Authorization token.
 */

/**
 * 所有可预期错误的基类。
 * EN: Base class for expected failures.
 */
export class HouyidbMcpError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/**
 * collection 不在 HOUYIDB_MCP_COLLECTIONS 白名单内。
 * EN: Collection is outside the HOUYIDB_MCP_COLLECTIONS allowlist.
 */
export class CollectionNotAllowedError extends HouyidbMcpError {
  readonly collection: string;

  constructor(collection: string) {
    super(
      `Collection "${collection}" is not allowed by HOUYIDB_MCP_COLLECTIONS. ` +
        `collection "${collection}" 不在 HOUYIDB_MCP_COLLECTIONS 白名单内。`,
    );
    this.collection = collection;
  }
}

/**
 * 无法连接 HouyiDB 或请求超时。
 * EN: HouyiDB is unreachable or the request timed out.
 */
export class HouyiTransportError extends HouyidbMcpError {}

/**
 * 服务端返回非 2xx。正文已完成脱敏与截断。
 * EN: Upstream returned non-2xx. The body is already redacted and truncated.
 */
export class HouyiHttpError extends HouyidbMcpError {
  readonly status: number;
  readonly body: string;

  constructor(status: number, body: string) {
    super(`HouyiDB HTTP ${status}: ${body}`);
    this.status = status;
    this.body = body;
  }
}

/**
 * 服务端 2xx 响应不是预期的 JSON，或形状不符合端点契约。
 * EN: Upstream returned 2xx but the payload is not valid JSON or violates the
 * endpoint contract.
 */
export class HouyiResponseError extends HouyidbMcpError {}
