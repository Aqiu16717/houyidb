"""HouyiDB 的 VectorDBBench 客户端。
EN: VectorDBBench client for HouyiDB.

中文：该模块只负责把 VectorDBBench 的写入/检索调用映射到 HouyiDB 的
pgwire 或 HTTP 接口，不包含任何服务端索引实现。
EN: This module maps VectorDBBench load/search calls onto HouyiDB pgwire or
HTTP endpoints; it contains no server-side index implementation.
"""

from __future__ import annotations

import logging
import math
import struct
import threading
import time
from collections.abc import Generator, Iterable
from contextlib import contextmanager
from typing import Any

import requests

from vectordb_bench.backend.filter import FilterOp

from ..api import MetricType, VectorDB
from .config import HouyiDBIndexConfig

log = logging.getLogger(__name__)


def _encode_houyi_upsert_batch_bytea_binary_v1(dim: int, items: list[tuple[int, list[float]]]) -> bytes:
    """编码 AUB1 批量 upsert 的 BYTEA 二进制负载。

    中文：布局为 ``AUB1 + dim(u32 le) + count(u32 le)``，随后每条记录为
    ``id(u64 le) + f32 向量``；与服务端 pgwire 解码器保持逐字节一致。
    EN: Layout is ``AUB1 + dim(u32 le) + count(u32 le)`` followed by
    ``id(u64 le) + f32 vector`` per item, byte-compatible with the server
    pgwire decoder.
    """
    out = bytearray()
    out.extend(b"AUB1")
    out.extend(int(dim).to_bytes(4, "little"))
    out.extend(len(items).to_bytes(4, "little"))
    for vid, vec in items:
        out.extend(int(vid).to_bytes(8, "little"))
        if len(vec) != dim:
            raise ValueError(f"AUB1 dim mismatch: expected {dim}, got {len(vec)}")
        for x in vec:
            out.extend(struct.pack("<f", float(x)))
    return bytes(out)


def _encode_houyi_vector_bytea_binary_v1(values: list[float]) -> bytes:
    """编码 AVB1 查询向量的 BYTEA 二进制负载。

    中文：布局为 ``AVB1 + dim(u32 le) + f32 向量``。
    EN: Layout is ``AVB1 + dim(u32 le) + f32 vector``.
    """
    out = bytearray()
    out.extend(b"AVB1")
    out.extend(len(values).to_bytes(4, "little"))
    for x in values:
        out.extend(struct.pack("<f", float(x)))
    return bytes(out)


class HouyiDB(VectorDB):
    """HouyiDB 的 VectorDBBench 适配器。
    EN: VectorDBBench adapter for HouyiDB.

    中文：
        ``transport="pgwire"`` 通过 PostgreSQL extended query + BYTEA 写入和
        检索；``transport="http"`` 使用 benchmark HTTP API。两种模式都会通过
        HTTP ``/v1/health``、``/v1/optimize`` 与 ``/v1/index/status`` 完成健康
        检查和构图排空。``transport="mock"`` 仅用于离线自测，不得用于任何性能
        结论。
    EN:
        ``transport="pgwire"`` uses PostgreSQL extended query + BYTEA for
        writes and searches. ``transport="http"`` uses the benchmark HTTP API.
        Both modes use HTTP ``/v1/health``, ``/v1/optimize`` and
        ``/v1/index/status`` for health checks and index drain. The
        ``transport="mock"`` mode exists only for offline adapter self-tests
        and must never be used for performance claims.

    中文：每个 OS 线程独立持有一个 ``requests.Session`` 或 psycopg 连接，
    因此声明 ``thread_safe=True`` 后可被 VectorDBBench 的并发 worker 复用。
    EN: Each OS thread owns its own ``requests.Session`` or psycopg
    connection; the class therefore declares ``thread_safe=True`` and can be
    shared by VectorDBBench concurrent workers.
    """

    thread_safe: bool = True

    # 中文：支持 NonFilter（默认）、NumGE（id > N）与 StrEqual（label == value）。
    # EN: Supports NonFilter (default), NumGE (id > N) and StrEqual (label == value).
    supported_filter_types = [FilterOp.NonFilter, FilterOp.NumGE, FilterOp.StrEqual]

    # 中文：HTTP 批量分片；较小分片降低并发写入时的单请求 CPU 与 WAL 峰值。
    # EN: HTTP batch chunk size; smaller chunks reduce per-request CPU and WAL bursts.
    _HTTP_UPSERT_CHUNK_SIZE = 64

    # 中文：HouyiDB pgwire 握手使用的固定角色与逻辑库名。
    # EN: Fixed role and logical database names used by the HouyiDB pgwire handshake.
    _PGWIRE_USER = "tester"
    _PGWIRE_DATABASE = "houyi"

    def _init_runtime(self) -> None:
        """初始化不可序列化的运行时字段。
        EN: Initialize non-picklable runtime fields.

        中文：线程本地句柄与锁会在并发 worker 进程反序列化后重建。
        EN: Thread-local handles and locks are recreated after deserialization
        in concurrent worker processes.
        """
        self._thread_local = threading.local()
        self._http_sessions_lock = threading.Lock()
        self._http_sessions: list[requests.Session] = []
        self._pgwire_sessions_lock = threading.Lock()
        self._pgwire_sessions: list[Any] = []

    def __init__(
        self,
        dim: int,
        db_config: dict,
        db_case_config: HouyiDBIndexConfig,
        collection_name: str = "houyidb_collection",
        drop_old: bool = False,
        name: str = "HouyiDB",
        **kwargs: Any,
    ):
        self.name = name
        self.dim = dim
        self.db_config = db_config
        self.case_config = db_case_config
        self.collection_name = collection_name
        self.transport = str(db_config.get("transport", "pgwire")).lower()
        self.metric = self.case_config.metric_type or MetricType.COSINE
        self.timeout_sec = int(db_config.get("timeout_sec", 600))
        raw_api_key = db_config.get("api_key")
        # 中文：兼容调用方直接传入 pydantic SecretStr 的情况。
        # EN: Also accept a pydantic SecretStr passed directly by callers.
        self.api_key = (
            raw_api_key.get_secret_value() if hasattr(raw_api_key, "get_secret_value") else raw_api_key
        )
        scheme = "https" if db_config.get("use_tls", False) else "http"
        hp = db_config.get("http_port")
        self._http_port = int(hp) if hp is not None else int(db_config["port"])
        host = str(db_config["host"])
        # 中文：IPv6 字面量在 URL 中必须加方括号。
        # EN: IPv6 literals must be bracketed inside URLs.
        if ":" in host and not host.startswith("["):
            host = f"[{host}]"
        self.base_url = f"{scheme}://{host}:{self._http_port}"
        self._init_runtime()

        self._store: dict[int, list[float]] = {}
        self._prepared_filter = None
        self._connected = False

        if drop_old:
            log.warning(
                "HouyiDB uses one server-side store and cannot drop it from the client; "
                "start houyi-server with an empty WAL for a cold run."
            )
        log.info("%s initialized transport=%s dim=%s", self.name, self.transport, self.dim)
        log.info("HouyiDB index params=%s", self.case_config.index_param())
        log.info("HouyiDB search params=%s", self.case_config.search_param())

    def __getstate__(self) -> dict[str, Any]:
        """生成可 pickle 的状态。
        EN: Build picklable state.

        中文：移除线程本地句柄、Session 与锁，供并发 runner 交给子进程。
        EN: Drop thread-local handles, sessions and locks before handing the
        object to concurrent runner processes.
        """
        state = dict(self.__dict__)
        for key in (
            "_thread_local",
            "_http_sessions_lock",
            "_http_sessions",
            "_pgwire_sessions_lock",
            "_pgwire_sessions",
        ):
            state.pop(key, None)
        return state

    def __setstate__(self, state: dict[str, Any]) -> None:
        """恢复状态并重建运行时句柄。
        EN: Restore state and recreate runtime handles.
        """
        self.__dict__.update(state)
        self._init_runtime()

    def prepare_filter(self, filters) -> None:
        """保存当前过滤条件，供后续检索使用。
        EN: Store the active filter for subsequent searches.
        """
        self._prepared_filter = filters

    def _http_session(self) -> requests.Session:
        """返回当前线程的 HTTP Session。
        EN: Return the current thread's HTTP session.

        中文：首次使用时创建并做健康检查；传入 api_key 时注入 Bearer 头。
        EN: Create and health-check it on first use; inject a Bearer header
        when api_key is configured.
        """
        if not hasattr(self._thread_local, "session"):
            session = requests.Session()
            if self.api_key:
                session.headers.update({"Authorization": f"Bearer {self.api_key}"})
            with self._http_sessions_lock:
                self._http_sessions.append(session)
            try:
                resp = session.get(f"{self.base_url}/v1/health", timeout=self.timeout_sec)
                resp.raise_for_status()
            except Exception as exc:  # noqa: BLE001
                with self._http_sessions_lock:
                    if session in self._http_sessions:
                        self._http_sessions.remove(session)
                try:
                    session.close()
                except Exception:
                    pass
                raise RuntimeError(
                    f"failed to connect HouyiDB HTTP endpoint {self.base_url}: {exc}"
                ) from exc
            self._thread_local.session = session
        return self._thread_local.session

    def _pgwire_conn(self) -> Any:
        """返回当前线程的 psycopg 连接。
        EN: Return the current thread's psycopg connection.

        中文：api_key 作为 pgwire 密码发送，以便服务端启用 token 认证。
        EN: Send api_key as the pgwire password so token-authenticated servers work.
        """
        if not hasattr(self._thread_local, "pg_conn"):
            try:
                import psycopg
                from psycopg.conninfo import make_conninfo
            except ImportError as exc:
                raise RuntimeError(
                    "transport=pgwire requires psycopg; install 'psycopg[binary]>=3.1'"
                ) from exc
            params: dict[str, Any] = {
                "host": str(self.db_config["host"]),
                "port": str(self.db_config["port"]),
                "user": self._PGWIRE_USER,
                "dbname": self._PGWIRE_DATABASE,
                "connect_timeout": str(self.timeout_sec),
            }
            if self.api_key:
                params["password"] = str(self.api_key)
            try:
                conn = psycopg.connect(make_conninfo(**params), autocommit=True)
            except Exception as exc:
                raise RuntimeError(
                    f"failed to connect HouyiDB pgwire {self.db_config['host']}:"
                    f"{self.db_config['port']}: {exc}"
                ) from exc
            with self._pgwire_sessions_lock:
                self._pgwire_sessions.append(conn)
            try:
                conn.execute("SELECT 1")
            except Exception as exc:
                with self._pgwire_sessions_lock:
                    if conn in self._pgwire_sessions:
                        self._pgwire_sessions.remove(conn)
                try:
                    conn.close()
                except Exception:
                    pass
                raise RuntimeError(f"pgwire health check failed: {exc}") from exc
            self._thread_local.pg_conn = conn
            # 中文：BYTEA 扩展查询参数必须使用 psycopg.Binary 包装。
            # EN: BYTEA extended-query parameters must be wrapped with psycopg.Binary.
            self._thread_local._pg_binary = psycopg.Binary
        return self._thread_local.pg_conn

    @contextmanager
    def init(self) -> Generator[None, None, None]:
        """建立传输层资源并在退出时清理。
        EN: Establish transport resources and clean them up on exit.
        """
        if self.transport not in ("http", "pgwire", "mock"):
            raise ValueError(
                f"Unsupported HouyiDB transport={self.transport!r}; expected 'pgwire', 'http' or 'mock'"
            )
        self._connected = True
        try:
            if self.transport == "http":
                # 中文：预热调用线程；写入/检索 worker 会各自惰性建连。
                # EN: Prime the caller thread; insert/search workers connect lazily.
                self._http_session()
            elif self.transport == "pgwire":
                self._pgwire_conn()
            else:
                log.warning("HouyiDB transport=mock is for offline self-tests only; no server is used.")
            yield
        finally:
            self._connected = False
            if self.transport in ("http", "pgwire"):
                with self._http_sessions_lock:
                    for session in self._http_sessions:
                        try:
                            session.close()
                        except Exception:
                            log.warning("Failed to close HouyiDB HTTP session", exc_info=True)
                    self._http_sessions.clear()
                if hasattr(self._thread_local, "session"):
                    del self._thread_local.session
            if self.transport == "pgwire":
                with self._pgwire_sessions_lock:
                    for conn in self._pgwire_sessions:
                        try:
                            conn.close()
                        except Exception:
                            log.warning("Failed to close HouyiDB pgwire connection", exc_info=True)
                    self._pgwire_sessions.clear()
                for attr in ("pg_conn", "_pg_binary"):
                    if hasattr(self._thread_local, attr):
                        delattr(self._thread_local, attr)

    def optimize(self, data_size: int | None = None) -> None:
        """等待服务端索引清空待构建积压。
        EN: Wait until the server-side index has drained its build backlog.

        中文：先 POST ``/v1/optimize`` 触发 freeze/compact，再轮询
        ``/v1/index/status`` 直到 ``pending_count == 0``。HII 的全量 L1 重构
        可能远超 600 秒，因此读超时单独放宽，避免与普通请求超时共用。
        EN: POST ``/v1/optimize`` to trigger freeze/compact, then poll
        ``/v1/index/status`` until ``pending_count == 0``. A full HII L1 rebuild
        can exceed 600 seconds, so the read timeout is widened independently.
        """
        if self.transport == "mock":
            return
        if not self._connected:
            raise RuntimeError("Please call init() before optimize")
        session = self._http_session()
        opt_read = max(7200, int(self.timeout_sec))
        response = session.post(
            f"{self.base_url}/v1/optimize",
            timeout=(30, opt_read),
        )
        response.raise_for_status()
        poll_seconds = 0.05
        while True:
            response = session.get(
                f"{self.base_url}/v1/index/status",
                timeout=self.timeout_sec,
            )
            response.raise_for_status()
            payload = response.json()
            pending = int(payload.get("pending_count", 0))
            if pending <= 0:
                return
            time.sleep(poll_seconds)

    def insert_embeddings(
        self,
        embeddings: Iterable[list[float]],
        metadata: list[int],
        labels_data: list[str] | None = None,
        **kwargs: Any,
    ) -> tuple[int, Exception | None]:
        """批量写入向量并返回成功条数与首个错误。
        EN: Insert vectors in batches and return the inserted count plus the first error.
        """
        if not self._connected:
            return 0, RuntimeError("Please call init() before insert")
        vectors = list(embeddings)
        if len(vectors) != len(metadata):
            return 0, ValueError("embeddings and metadata length mismatch")

        # 中文：带标量标签时必须走 HTTP，服务端才保存 label 供过滤检索。
        # EN: Scalar labels require HTTP so the server persists them for filtered search.
        has_labels = labels_data is not None and len(labels_data) == len(vectors)

        inserted = 0
        if self.transport == "http" or has_labels:
            try:
                session = self._http_session()
                case = self.case_config
                for start in range(0, len(vectors), self._HTTP_UPSERT_CHUNK_SIZE):
                    end = start + self._HTTP_UPSERT_CHUNK_SIZE
                    payload_vectors = []
                    for idx, vector in enumerate(vectors[start:end], start=start):
                        item = {"id": int(metadata[idx]), "values": vector}
                        if has_labels:
                            item["label"] = labels_data[idx]
                        payload_vectors.append(item)
                    payload = {
                        "vectors": payload_vectors,
                        "index": {
                            "m": int(case.m),
                            "ef_construction": int(case.ef_construction),
                        },
                    }
                    response = session.post(
                        f"{self.base_url}/v1/vectors/upsert",
                        json=payload,
                        timeout=self.timeout_sec,
                    )
                    response.raise_for_status()
                    inserted += int(response.json().get("inserted", 0))
                return inserted, None
            except Exception as exc:  # noqa: BLE001
                return inserted, exc

        if self.transport == "pgwire":
            try:
                conn = self._pgwire_conn()
                pg_binary = self._thread_local._pg_binary
                dim = self.dim
                for start in range(0, len(vectors), self._HTTP_UPSERT_CHUNK_SIZE):
                    end = min(start + self._HTTP_UPSERT_CHUNK_SIZE, len(vectors))
                    chunk: list[tuple[int, list[float]]] = []
                    for idx in range(start, end):
                        vector = vectors[idx]
                        if len(vector) != dim:
                            return inserted, ValueError(f"dim mismatch, expected {dim}, got {len(vector)}")
                        chunk.append((int(metadata[idx]), vector))
                    payload = _encode_houyi_upsert_batch_bytea_binary_v1(dim, chunk)
                    row = conn.execute(
                        "/* houyi:upsert_batch=%s */ SELECT 1",
                        (pg_binary(payload),),
                    ).fetchone()
                    inserted += int(row[0]) if row else 0
                return inserted, None
            except Exception as exc:  # noqa: BLE001
                return inserted, exc

        try:
            for idx, vector in enumerate(vectors):
                if len(vector) != self.dim:
                    raise ValueError(f"dim mismatch, expected {self.dim}, got {len(vector)}")
                self._store[int(metadata[idx])] = vector
                inserted += 1
        except Exception as exc:  # noqa: BLE001
            return inserted, exc
        return inserted, None

    def _search_http(
        self,
        query: list[float],
        k: int,
        filters: dict | None,
        timeout: int | None,
    ) -> list[int]:
        """通过 HTTP 执行一次向量检索。
        EN: Run one vector search over HTTP.
        """
        payload: dict[str, Any] = {
            "vector": query,
            "k": int(k),
            "ef_search": int(self.case_config.ef_search),
        }
        if filters and "id" in filters:
            payload["filter_gt_id"] = int(filters["id"])

        # 中文：字符串等值过滤通过 prepare_filter 传递，服务端按 label 过滤。
        # EN: String equality filters are passed via prepare_filter and applied by label.
        active_filter = getattr(self, "_prepared_filter", None)
        if active_filter is not None and active_filter.type == FilterOp.StrEqual:
            payload["filter_label"] = active_filter.label_value

        try:
            session = self._http_session()
            response = session.post(
                f"{self.base_url}/v1/vectors/search",
                json=payload,
                timeout=timeout or self.timeout_sec,
            )
            response.raise_for_status()
            return [int(item) for item in response.json().get("ids", [])]
        except Exception as exc:  # noqa: BLE001
            log.warning("HouyiDB HTTP search failed: %s", exc, exc_info=True)
            return []

    def search_embedding(
        self,
        query: list[float],
        k: int = 100,
        filters: dict | None = None,
        timeout: int | None = None,
        **kwargs: Any,
    ) -> list[int]:
        """执行 k 近邻检索并返回 id 列表。
        EN: Run a k-nearest-neighbour search and return the id list.
        """
        if not self._connected:
            return []
        if len(query) != self.dim:
            return []

        # 中文：label 过滤只有 HTTP 数据面支持，pgwire 模式会自动回退。
        # EN: Label filters are only supported by the HTTP data path; pgwire falls back to HTTP.
        active_filter = getattr(self, "_prepared_filter", None)
        has_label_filter = active_filter is not None and active_filter.type == FilterOp.StrEqual

        if self.transport == "http" or has_label_filter:
            return self._search_http(query, k, filters, timeout)

        if self.transport == "pgwire":
            if filters:
                return self._search_http(query, k, filters, timeout)
            try:
                conn = self._pgwire_conn()
                pg_binary = self._thread_local._pg_binary
                vector_payload = _encode_houyi_vector_bytea_binary_v1(query)
                ef_search = int(self.case_config.ef_search)
                sql = (
                    f"/* houyi:ef_search={ef_search} */ /* houyi:vector=%s */ "
                    f"SELECT id FROM docs LIMIT {int(k)}"
                )
                rows = conn.execute(sql, (pg_binary(vector_payload),)).fetchall()
                return [int(row[0]) for row in rows]
            except Exception as exc:  # noqa: BLE001
                log.warning("HouyiDB pgwire search failed: %s", exc, exc_info=True)
                return []

        min_id_filter = None if not filters else filters.get("id")
        scored: list[tuple[int, float]] = []
        for idx, vector in self._store.items():
            if min_id_filter is not None and idx <= min_id_filter:
                continue
            scored.append((idx, self._distance(query, vector)))
        scored.sort(key=lambda item: (item[1], item[0]))
        return [item[0] for item in scored[:k]]

    def _distance(self, a: list[float], b: list[float]) -> float:
        """计算 mock transport 使用的距离。
        EN: Compute the distance used by the mock transport.
        """
        if self.metric == MetricType.IP:
            return -sum(x * y for x, y in zip(a, b, strict=False))
        if self.metric == MetricType.COSINE:
            dot = sum(x * y for x, y in zip(a, b, strict=False))
            norm_a = math.sqrt(sum(x * x for x in a))
            norm_b = math.sqrt(sum(y * y for y in b))
            if norm_a == 0.0 or norm_b == 0.0:
                return 1.0
            return 1.0 - (dot / (norm_a * norm_b))
        return sum((x - y) * (x - y) for x, y in zip(a, b, strict=False))
