#!/usr/bin/env node
/**
 * CLI 入口：解析环境变量并通过 stdio 启动 MCP server。
 * EN: CLI entry: parse environment variables and start the MCP server over stdio.
 */

import { realpathSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { loadConfig } from "./config.js";
import { runStdioServer } from "./server.js";

export {
  loadConfig,
  isCollectionAllowed,
  assertCollectionAllowed,
  type HouyidbMcpConfig,
  ConfigError,
} from "./config.js";
export { createHouyidbServer, runStdioServer } from "./server.js";
export {
  HouyidbMcpError,
  CollectionNotAllowedError,
  HouyiTransportError,
  HouyiHttpError,
  HouyiResponseError,
} from "./errors.js";

/**
 * 启动失败只写 stderr（stdout 保留给 MCP 协议），不输出堆栈或凭据。
 * EN: Startup failures go to stderr only (stdout belongs to the MCP protocol),
 * without stack traces or credentials.
 */
async function main(): Promise<void> {
  const config = loadConfig();
  await runStdioServer(config);
}

/**
 * 判断本文件是否被直接执行；作为模块导入时不自动启动服务。
 * EN: Detects direct execution so importing this module never starts a server.
 */
function isDirectExecution(): boolean {
  const entry = process.argv[1];
  if (entry === undefined) return false;
  let resolved = entry;
  try {
    resolved = realpathSync(entry);
  } catch {
    // 路径不存在时按原样比较；不会影响正常 CLI 启动。
    // EN: Fall back to the raw path when realpath fails; CLI startup still works.
  }
  return import.meta.url === pathToFileURL(resolved).href;
}

if (isDirectExecution()) {
  void main().catch((error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    console.error(`[houyidb-mcp] fatal: ${message}`);
    process.exitCode = 1;
  });
}
