#!/usr/bin/env python3
"""把 OpenAI 50K parquet 数据集导出为简单的二进制格式，供 Rust 基准测试使用。
EN: Export OpenAI 50K parquet dataset to simple binary format for Rust benchmarks.

输出格式：
EN: Output format:
  [n: u64 LE][dims: u64 LE][vectors: f32 LE × n × dims][ids: u64 LE × n]

用法：python3 scripts/export_dataset.py [dataset_dir] [output_dir]
EN: Usage: python3 scripts/export_dataset.py [dataset_dir] [output_dir]
"""

import struct, sys, os
import pyarrow.parquet as pq
import numpy as np

# 默认锚定脚本所在仓库根（与 cwd 无关）；argv / HOUYI_DATASET_DIR 均做 ~ 展开
# EN: Default anchors to the repository root containing the script (cwd-independent);
# EN: argv / HOUYI_DATASET_DIR both expand ~.
_DEFAULT_DS = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                           "..", "storage", "vectordb_bench",
                           "dataset", "openai", "openai_small_50k")
DS = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1
                        else os.environ.get("HOUYI_DATASET_DIR", _DEFAULT_DS))
OUT = sys.argv[2] if len(sys.argv) > 2 else "/tmp"


def export_parquet(pq_path: str, out_path: str):
    table = pq.read_table(pq_path)
    ids = table["id"].to_pylist()
    embs = table["emb"].to_pylist()
    n = len(ids)
    dims = len(embs[0]) if n > 0 else 0

    with open(out_path, "wb") as f:
        # 文件头：n (u64) + dims (u64)
        # EN: Header: n (u64) + dims (u64)
        f.write(struct.pack("<QQ", n, dims))
        # 向量：f32 LE 平铺
        # EN: Vectors: f32 LE flat
        for emb in embs:
            arr = np.array(emb, dtype=np.float32)
            f.write(arr.tobytes())
        # 编号：u64 LE
        # EN: IDs: u64 LE
        for id_val in ids:
            f.write(struct.pack("<Q", int(id_val)))

    size_mb = os.path.getsize(out_path) / 1024 / 1024
    print(f"  Exported {out_path}: {n} vectors × {dims} dims, {size_mb:.1f} MB")


def main():
    train_pq = os.path.join(DS, "shuffle_train.parquet")
    test_pq = os.path.join(DS, "test.parquet")

    if not os.path.exists(train_pq):
        print(f"Dataset not found at {DS}")
        print("Expected files: shuffle_train.parquet, test.parquet")
        sys.exit(1)

    print(f"Exporting from {DS} -> {OUT}")
    export_parquet(train_pq, os.path.join(OUT, "openai_50k_train.bin"))
    export_parquet(test_pq, os.path.join(OUT, "openai_50k_test.bin"))
    print("Done.")


if __name__ == "__main__":
    main()
