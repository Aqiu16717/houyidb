import { afterEach, describe, expect, it } from "vitest";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { loadConfig, type HouyidbMcpConfig } from "../src/config.js";
import { MAX_ERROR_BODY_CHARS } from "../src/constants.js";
import type { FetchLike, FetchResponseLike } from "../src/houyi-client.js";
import { createHouyidbServer } from "../src/server.js";

interface CapturedRequest {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: string | undefined;
}

interface MockFetch {
  fetchImpl: FetchLike;
  calls: CapturedRequest[];
}

function createMockFetch(
  handler: (
    request: CapturedRequest,
  ) => FetchResponseLike | Promise<FetchResponseLike>,
): MockFetch {
  const calls: CapturedRequest[] = [];
  const fetchImpl: FetchLike = async (url, init = {}) => {
    const request: CapturedRequest = {
      url,
      method: init.method ?? "GET",
      headers: init.headers ?? {},
      body: init.body,
    };
    calls.push(request);
    return handler(request);
  };
  return { fetchImpl, calls };
}

function jsonResponse(body: unknown, status = 200): FetchResponseLike {
  return {
    status,
    text: async () => JSON.stringify(body),
  };
}

function textResponse(text: string, status = 200): FetchResponseLike {
  return {
    status,
    text: async () => text,
  };
}

function makeConfig(
  overrides: Partial<HouyidbMcpConfig> = {},
): HouyidbMcpConfig {
  return {
    baseUrl: "http://houyi.test:17000",
    readOnly: true,
    collections: new Set<string>(),
    ...overrides,
  };
}

const closers: Array<() => Promise<void>> = [];

afterEach(async () => {
  while (closers.length > 0) {
    const close = closers.pop();
    if (close !== undefined) await close();
  }
});

async function connect(
  config: HouyidbMcpConfig,
  fetchImpl: FetchLike,
): Promise<Client> {
  const server = createHouyidbServer(config, { fetchImpl });
  const client = new Client({ name: "houyidb-mcp-test", version: "0.0.0" });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);
  closers.push(async () => {
    await Promise.allSettled([client.close(), server.close()]);
  });
  return client;
}

interface TextResult {
  isError?: boolean;
  text: string;
}

async function callText(
  client: Client,
  name: string,
  args: Record<string, unknown>,
): Promise<TextResult> {
  const result = (await client.callTool({
    name,
    arguments: args,
  })) as {
    isError?: boolean;
    content?: Array<{ type?: string; text?: string }>;
  };
  const text = (result.content ?? [])
    .filter((item) => item.type === "text")
    .map((item) => item.text ?? "")
    .join("\n");
  return { ...(result.isError === undefined ? {} : { isError: result.isError }), text };
}

async function toolNames(client: Client): Promise<string[]> {
  const { tools } = await client.listTools();
  return tools.map((tool) => tool.name).sort();
}

describe("configuration", () => {
  it("uses safe defaults", () => {
    const config = loadConfig({});
    expect(config.baseUrl).toBe("http://127.0.0.1:17000");
    expect(config.authToken).toBeUndefined();
    expect(config.readOnly).toBe(true);
    expect(config.collections.size).toBe(0);
  });

  it("parses write mode, token, URL, and allowlist", () => {
    const config = loadConfig({
      HOUYIDB_URL: "http://houyi.local:18000///",
      HOUYI_AUTH_TOKEN: "  secret-token  ",
      HOUYIDB_MCP_READ_ONLY: "0",
      HOUYIDB_MCP_COLLECTIONS: " docs, notes ,, docs ",
    });
    expect(config.baseUrl).toBe("http://houyi.local:18000");
    expect(config.authToken).toBe("secret-token");
    expect(config.readOnly).toBe(false);
    expect([...config.collections]).toEqual(["docs", "notes"]);
  });

  it("rejects ambiguous read-only values instead of enabling writes", () => {
    expect(() => loadConfig({ HOUYIDB_MCP_READ_ONLY: "maybe" })).toThrow(
      /HOUYIDB_MCP_READ_ONLY/,
    );
  });

  it("rejects credentials embedded in the base URL", () => {
    expect(() =>
      loadConfig({ HOUYIDB_URL: "http://user:pass@houyi.local:17000" }),
    ).toThrow(/must not embed credentials/);
  });
});

describe("tool registration", () => {
  it("registers only read tools in read-only mode", async () => {
    const mock = createMockFetch(() => jsonResponse([]));
    const client = await connect(makeConfig(), mock.fetchImpl);
    expect(await toolNames(client)).toEqual([
      "collection_status",
      "hybrid_search",
      "list_collections",
      "search",
    ]);
    expect(mock.calls).toHaveLength(0);
  });

  it("registers write tools when read-only is disabled", async () => {
    const mock = createMockFetch(() => jsonResponse([]));
    const client = await connect(
      makeConfig({ readOnly: false }),
      mock.fetchImpl,
    );
    expect(await toolNames(client)).toEqual([
      "collection_status",
      "delete",
      "hybrid_search",
      "list_collections",
      "search",
      "upsert",
    ]);
  });
});

describe("collection allowlist", () => {
  it("filters list_collections by the allowlist", async () => {
    const mock = createMockFetch(() =>
      jsonResponse([
        { name: "docs", vectors: 10 },
        { name: "secret", vectors: 20 },
      ]),
    );
    const client = await connect(
      makeConfig({ collections: new Set(["docs"]) }),
      mock.fetchImpl,
    );
    const { isError, text } = await callText(client, "list_collections", {});
    expect(isError).not.toBe(true);
    expect(JSON.parse(text)).toEqual([{ name: "docs", vectors: 10 }]);
  });

  it("blocks every collection-scoped tool before making a request", async () => {
    const mock = createMockFetch(() => jsonResponse({}));
    const client = await connect(
      makeConfig({
        readOnly: false,
        collections: new Set(["docs"]),
      }),
      mock.fetchImpl,
    );

    const calls: Array<[string, Record<string, unknown>]> = [
      ["collection_status", { collection: "secret" }],
      ["search", { collection: "secret", vector: [1, 0], k: 1 }],
      [
        "hybrid_search",
        { collection: "secret", vector: [1, 0], k: 1, filters: [] },
      ],
      [
        "upsert",
        { collection: "secret", vectors: [{ id: 1, values: [1, 0] }] },
      ],
      ["delete", { collection: "secret", ids: [1] }],
    ];

    for (const [name, args] of calls) {
      const { isError, text } = await callText(client, name, args);
      expect(isError, name).toBe(true);
      expect(text, name).toContain("HOUYIDB_MCP_COLLECTIONS");
    }
    expect(mock.calls).toHaveLength(0);
  });
});

describe("search request mapping", () => {
  it("sends search options to the collection search endpoint", async () => {
    const mock = createMockFetch(() =>
      jsonResponse({
        ids: [7],
        scores: [0.99],
        payloads: { "7": { label: "doc-7", kind: "guide" } },
      }),
    );
    const client = await connect(
      makeConfig({
        collections: new Set(["docs"]),
        authToken: "top-secret",
      }),
      mock.fetchImpl,
    );

    const { isError, text } = await callText(client, "search", {
      collection: "docs",
      vector: [0.25, -0.5, 1],
      k: 3,
      include_payload: true,
      ef_search: 128,
    });

    expect(isError).not.toBe(true);
    expect(JSON.parse(text)).toEqual({
      ids: [7],
      scores: [0.99],
      payloads: { "7": { label: "doc-7", kind: "guide" } },
    });
    expect(mock.calls).toHaveLength(1);
    const request = mock.calls[0];
    expect(request?.url).toBe(
      "http://houyi.test:17000/v1/collections/docs/search",
    );
    expect(request?.method).toBe("POST");
    expect(request?.headers.Authorization).toBe("Bearer top-secret");
    expect(request?.headers["Content-Type"]).toBe("application/json");
    expect(JSON.parse(request?.body ?? "{}")).toEqual({
      vector: [0.25, -0.5, 1],
      k: 3,
      include_payload: true,
      ef_search: 128,
    });
  });

  it("defaults include_payload to false and omits ef_search", async () => {
    const mock = createMockFetch(() =>
      jsonResponse({ ids: [], scores: [] }),
    );
    const client = await connect(makeConfig(), mock.fetchImpl);

    await callText(client, "search", {
      collection: "docs",
      vector: [1, 2],
      k: 2,
    });

    expect(JSON.parse(mock.calls[0]?.body ?? "{}")).toEqual({
      vector: [1, 2],
      k: 2,
      include_payload: false,
    });
  });

  it("maps hybrid filters to the hybrid endpoint", async () => {
    const mock = createMockFetch(() =>
      jsonResponse({ ids: [1], scores: [0.5] }),
    );
    const client = await connect(makeConfig(), mock.fetchImpl);
    const filters = [
      { column: "category", op: "eq", value: "fruit" },
      { column: "year", op: "in", value: [2025, 2026] },
    ];

    await callText(client, "hybrid_search", {
      collection: "docs",
      vector: [1, 0],
      k: 5,
      filters,
      include_payload: true,
    });

    expect(mock.calls[0]?.url).toBe(
      "http://houyi.test:17000/v1/collections/docs/hybrid/search",
    );
    expect(JSON.parse(mock.calls[0]?.body ?? "{}")).toEqual({
      vector: [1, 0],
      k: 5,
      filters,
      include_payload: true,
    });
  });

  it("encodes collection names used as path segments", async () => {
    const mock = createMockFetch(() => jsonResponse({ name: "docs/2026" }));
    const client = await connect(makeConfig(), mock.fetchImpl);

    await callText(client, "collection_status", {
      collection: "docs/2026",
    });

    expect(mock.calls[0]?.url).toBe(
      "http://houyi.test:17000/v1/collections/docs%2F2026/status",
    );
  });
});

describe("write request mapping", () => {
  it("maps upsert vectors to the upsert endpoint", async () => {
    const mock = createMockFetch(() => jsonResponse({ inserted: 1 }));
    const client = await connect(
      makeConfig({ readOnly: false }),
      mock.fetchImpl,
    );
    const vectors = [
      {
        id: 42,
        values: [0.1, 0.2],
        label: "doc",
        payload: { category: "fruit" },
      },
    ];

    const { isError } = await callText(client, "upsert", {
      collection: "docs",
      vectors,
    });

    expect(isError).not.toBe(true);
    expect(mock.calls[0]?.url).toBe(
      "http://houyi.test:17000/v1/collections/docs/upsert",
    );
    expect(JSON.parse(mock.calls[0]?.body ?? "{}")).toEqual({ vectors });
  });

  it("maps delete ids and filters, and rejects an empty delete", async () => {
    const mock = createMockFetch(() => jsonResponse({ deleted: 2 }));
    const client = await connect(
      makeConfig({ readOnly: false }),
      mock.fetchImpl,
    );
    const filters = [{ column: "category", op: "eq", value: "fruit" }];

    await callText(client, "delete", {
      collection: "docs",
      ids: [1, 2],
      filters,
    });
    expect(mock.calls[0]?.url).toBe(
      "http://houyi.test:17000/v1/collections/docs/delete",
    );
    expect(JSON.parse(mock.calls[0]?.body ?? "{}")).toEqual({
      ids: [1, 2],
      filters,
    });

    const empty = await callText(client, "delete", { collection: "docs" });
    expect(empty.isError).toBe(true);
    expect(empty.text).toContain("at least one id or filter");
    expect(mock.calls).toHaveLength(1);
  });
});

describe("error mapping", () => {
  it("includes status and a redacted, truncated server body", async () => {
    const token = "top-secret";
    const mock = createMockFetch(() =>
      textResponse(
        `upstream exploded token=${token} ${"x".repeat(MAX_ERROR_BODY_CHARS * 2)}`,
        503,
      ),
    );
    const client = await connect(
      makeConfig({ authToken: token }),
      mock.fetchImpl,
    );

    const { isError, text } = await callText(client, "collection_status", {
      collection: "docs",
    });

    expect(isError).toBe(true);
    expect(text).toContain("HouyiDB HTTP 503");
    expect(text).toContain("upstream exploded");
    expect(text).toContain("[REDACTED]");
    expect(text).not.toContain(token);
    expect(text).toContain("truncated");
    expect(text.length).toBeLessThan(MAX_ERROR_BODY_CHARS * 2);
  });

  it("maps transport failures to a readable tool error without the token", async () => {
    const token = "top-secret";
    const mock = createMockFetch(() => {
      throw new Error(`ECONNREFUSED token=${token}`);
    });
    const client = await connect(
      makeConfig({ authToken: token }),
      mock.fetchImpl,
    );

    const { isError, text } = await callText(client, "collection_status", {
      collection: "docs",
    });

    expect(isError).toBe(true);
    expect(text).toContain("Failed to reach HouyiDB");
    expect(text).toContain("[REDACTED]");
    expect(text).not.toContain(token);
  });

  it("maps invalid JSON to a readable tool error", async () => {
    const mock = createMockFetch(() => textResponse("not-json", 200));
    const client = await connect(makeConfig(), mock.fetchImpl);

    const { isError, text } = await callText(client, "collection_status", {
      collection: "docs",
    });

    expect(isError).toBe(true);
    expect(text).toContain("invalid JSON");
    expect(text).toContain("not-json");
  });
});
