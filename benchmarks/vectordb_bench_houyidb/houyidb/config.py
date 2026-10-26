"""HouyiDB 的 VectorDBBench 连接与索引参数。
EN: VectorDBBench connection and index settings for HouyiDB.
"""

from __future__ import annotations

from pydantic import BaseModel, SecretStr

from ..api import DBCaseConfig, DBConfig, MetricType


class HouyiDBConfig(DBConfig):
    """连接配置。
    EN: Connection configuration.
    """

    host: str
    # 中文：默认值与仓库内 VDBBENCH_PROTOCOL.md 的 benchmark 端口一致。
    # EN: Defaults match the benchmark ports documented in VDBBENCH_PROTOCOL.md.
    port: int = 15432
    http_port: int | None = 17000
    use_tls: bool = False
    transport: str = "pgwire"
    api_key: SecretStr | None = None
    timeout_sec: int = 600

    def to_dict(self) -> dict:
        """转换为适配器内部使用的普通字典。
        EN: Convert to the plain dictionary consumed by the adapter.
        """
        return {
            "host": self.host,
            "port": self.port,
            "use_tls": self.use_tls,
            "transport": self.transport,
            "api_key": self.api_key.get_secret_value() if self.api_key else None,
            "timeout_sec": self.timeout_sec,
            "http_port": self.http_port,
        }


class HouyiDBIndexConfig(BaseModel, DBCaseConfig):
    """HNSW 构建与检索参数。
    EN: HNSW build and search settings.
    """

    metric_type: MetricType | None = MetricType.COSINE
    m: int = 24
    ef_construction: int = 400
    ef_search: int = 256

    def index_param(self) -> dict:
        """返回构建参数摘要。
        EN: Return the build-parameter summary.
        """
        return {
            "metric_type": self.metric_type.name if self.metric_type else MetricType.COSINE.name,
            "m": self.m,
            "ef_construction": self.ef_construction,
        }

    def search_param(self) -> dict:
        """返回检索参数摘要。
        EN: Return the search-parameter summary.
        """
        return {
            "ef_search": self.ef_search,
        }
