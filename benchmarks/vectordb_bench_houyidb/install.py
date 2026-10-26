#!/usr/bin/env python3
"""安装 HouyiDB 的 VectorDBBench 适配器。
EN: Install the HouyiDB adapter into a local VectorDBBench checkout.

中文：脚本只修改 ``--vdb-root`` 指定的目录，复制 ``houyidb/`` 并幂等注册
DB 枚举、三个工厂分支与 CLI 命令；不会联网，也不会运行 benchmark。
EN: The script only modifies the directory passed via ``--vdb-root``. It
copies ``houyidb/`` and idempotently registers the DB enum value, the three
factory branches and the CLI command. It never accesses the network or runs
a benchmark.
"""

from __future__ import annotations

import argparse
import ast
import os
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

SOURCE_PACKAGE = Path(__file__).resolve().parent / "houyidb"
DEST_REL = Path("vectordb_bench/backend/clients/houyidb")
CLIENTS_INIT_REL = Path("vectordb_bench/backend/clients/__init__.py")
CLI_REL = Path("vectordb_bench/cli/vectordbbench.py")
BACKUP_DIR_NAME = ".houyidb-install-backup"

ENUM_LINE = '    HouyiDB = "HouyiDB"'
ENUM_ANCHOR = "\n    @property\n    def init_cls(self)"
IMPORT_LINE = "from ..backend.clients.houyidb.cli import HouyiDB"
COMMAND_LINE = "cli.add_command(HouyiDB)"
COMMAND_ANCHOR = "cli.add_command(BatchCli)\n"
IMPORT_ANCHORS = (
    "from .batch_cli import BatchCli\n",
    "from .cli import cli\n",
)

FACTORY_BRANCHES = {
    "init_cls": """        if self == DB.HouyiDB:
            from .houyidb.houyidb import HouyiDB

            return HouyiDB""",
    "config_cls": """        if self == DB.HouyiDB:
            from .houyidb.config import HouyiDBConfig

            return HouyiDBConfig""",
    "case_config_cls": """        if self == DB.HouyiDB:
            from .houyidb.config import HouyiDBIndexConfig

            return HouyiDBIndexConfig""",
}


class InstallError(RuntimeError):
    """安装失败。
    EN: Installation failure.
    """


def _read(path: Path) -> str:
    """读取 UTF-8 文本。
    EN: Read UTF-8 text.
    """
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise InstallError(f"cannot read {path}: {exc}") from exc


def _atomic_write(path: Path, text: str) -> None:
    """原子替换目标文件。
    EN: Atomically replace a target file.
    """
    tmp = path.with_name(f".{path.name}.houyidb-tmp-{os.getpid()}")
    try:
        tmp.write_text(text, encoding="utf-8")
        os.replace(tmp, path)
    finally:
        if tmp.exists():
            tmp.unlink()


def _validate_paths(vdb_root: Path) -> tuple[Path, Path, Path, Path]:
    """校验 VDB 根目录和待修改文件。
    EN: Validate the VDB root and files that will be modified.
    """
    if not vdb_root.is_dir():
        raise InstallError(f"VDB root is not a directory: {vdb_root}")
    if not SOURCE_PACKAGE.is_dir():
        raise InstallError(f"adapter source package is missing: {SOURCE_PACKAGE}")

    required_source = [SOURCE_PACKAGE / name for name in ("houyidb.py", "config.py", "cli.py")]
    missing_source = [str(path) for path in required_source if not path.is_file()]
    if missing_source:
        raise InstallError("adapter source is incomplete; missing: " + ", ".join(missing_source))

    clients_init = vdb_root / CLIENTS_INIT_REL
    cli_file = vdb_root / CLI_REL
    for path in (clients_init, cli_file):
        if not path.is_file():
            raise InstallError(
                f"{path} not found; --vdb-root must point to a VectorDBBench source checkout"
            )
    return vdb_root / DEST_REL, clients_init, cli_file, SOURCE_PACKAGE


def _parse_syntax(path: Path, text: str) -> None:
    """在写入前校验 Python 语法。
    EN: Validate Python syntax before writing.
    """
    try:
        ast.parse(text, filename=str(path))
    except SyntaxError as exc:
        raise InstallError(f"refusing to modify syntactically invalid file {path}: {exc}") from exc


def _patch_enum(text: str) -> tuple[str, bool]:
    """向 DB 枚举插入 HouyiDB，已存在则跳过。
    EN: Insert HouyiDB into the DB enum, or skip when already present.
    """
    count = text.count(ENUM_LINE)
    if count == 1:
        return text, False
    if count > 1:
        raise InstallError(f"found {count} conflicting HouyiDB enum entries; clean them up first")
    anchor_count = text.count(ENUM_ANCHOR)
    if anchor_count != 1:
        raise InstallError(
            "cannot find the unique DB enum insertion point before 'init_cls'; "
            "this VectorDBBench version is not supported"
        )
    text = text.replace(ENUM_ANCHOR, f"\n{ENUM_LINE}\n{ENUM_ANCHOR}", 1)
    return text, True


def _find_db_method(text: str, method_name: str) -> ast.FunctionDef:
    """定位 DB 类中的工厂方法。
    EN: Locate a factory method inside the DB class.
    """
    try:
        tree = ast.parse(text)
    except SyntaxError as exc:
        raise InstallError(f"cannot parse client registry: {exc}") from exc
    db_class = next(
        (node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == "DB"),
        None,
    )
    if db_class is None:
        raise InstallError("DB class not found in client registry")
    method = next(
        (node for node in db_class.body if isinstance(node, ast.FunctionDef) and node.name == method_name),
        None,
    )
    if method is None:
        raise InstallError(f"DB.{method_name} not found; this VectorDBBench version is not supported")
    return method


def _patch_factory(text: str, method_name: str, branch: str) -> tuple[str, bool]:
    """向指定工厂方法插入 HouyiDB 分支。
    EN: Insert the HouyiDB branch into one factory method.
    """
    method = _find_db_method(text, method_name)
    method_source = ast.get_source_segment(text, method) or ""
    if "DB.HouyiDB" in method_source:
        return text, False

    body = method.body
    if (
        body
        and isinstance(body[0], ast.Expr)
        and isinstance(body[0].value, ast.Constant)
        and isinstance(body[0].value.value, str)
    ):
        # 中文：保留 docstring 作为方法体第一条语句。
        # EN: Keep the docstring as the first statement.
        body = body[1:]
    if not body:
        raise InstallError(f"DB.{method_name} has no insertion point")

    insert_at = body[0].lineno - 1
    lines = text.splitlines(keepends=True)
    branch_lines = [line + "\n" for line in branch.splitlines()]
    lines[insert_at:insert_at] = branch_lines + ["\n"]
    return "".join(lines), True


def _patch_clients(text: str) -> tuple[str, bool, int]:
    """注册 DB 枚举和三个工厂分支。
    EN: Register the DB enum and all three factory branches.
    """
    text, enum_changed = _patch_enum(text)
    factory_changes = 0
    for method_name, branch in FACTORY_BRANCHES.items():
        text, changed = _patch_factory(text, method_name, branch)
        factory_changes += int(changed)

    branch_count = text.count("if self == DB.HouyiDB:")
    if branch_count != 3:
        raise InstallError(
            f"expected 3 DB.HouyiDB factory branches after patching, found {branch_count}; "
            "registration is in an inconsistent state"
        )
    _parse_syntax(CLIENTS_INIT_REL, text)
    return text, enum_changed or bool(factory_changes), factory_changes


def _insert_import(text: str) -> str:
    """插入 CLI import。
    EN: Insert the CLI import.
    """
    if IMPORT_LINE in text:
        return text
    for anchor in IMPORT_ANCHORS:
        if text.count(anchor) == 1:
            return text.replace(anchor, f"{IMPORT_LINE}\n{anchor}", 1)
    raise InstallError(
        "cannot find a unique CLI import insertion point; this VectorDBBench version is not supported"
    )


def _insert_command(text: str) -> str:
    """插入 CLI add_command 调用。
    EN: Insert the CLI add_command call.
    """
    if COMMAND_LINE in text:
        return text
    if text.count(COMMAND_ANCHOR) == 1:
        return text.replace(COMMAND_ANCHOR, f"{COMMAND_LINE}\n{COMMAND_ANCHOR}", 1)

    # 中文：较新版本可能没有 BatchCli，退化为插在最后一个 add_command 之后。
    # EN: Newer versions may not ship BatchCli; append after the last add_command.
    lines = text.splitlines(keepends=True)
    command_lines = [index for index, line in enumerate(lines) if line.startswith("cli.add_command(")]
    if not command_lines:
        raise InstallError("cannot find a CLI add_command insertion point")
    last = command_lines[-1]
    if not lines[last].endswith("\n"):
        lines[last] += "\n"
    lines.insert(last + 1, f"{COMMAND_LINE}\n")
    return "".join(lines)


def _patch_cli(text: str) -> tuple[str, bool]:
    """注册 CLI 子命令。
    EN: Register the CLI subcommand.
    """
    original = text
    text = _insert_import(text)
    text = _insert_command(text)
    if text.count(IMPORT_LINE) != 1 or text.count(COMMAND_LINE) != 1:
        raise InstallError("CLI registration is in an inconsistent state after patching")
    _parse_syntax(CLI_REL, text)
    return text, text != original


def _make_backup(vdb_root: Path, destination: Path, registration_files: list[Path]) -> tuple[Path, bool]:
    """备份现有适配器与注册文件。
    EN: Back up the existing adapter and registration files.
    """
    stamp = time.strftime("%Y%m%d-%H%M%S") + f"-{os.getpid()}"
    backup_root = vdb_root / BACKUP_DIR_NAME / stamp
    backup_root.mkdir(parents=True, exist_ok=False)
    ignore = shutil.ignore_patterns("__pycache__", "*.pyc")
    had_destination = destination.is_dir()
    if had_destination:
        shutil.copytree(destination, backup_root / DEST_REL, ignore=ignore)
    for path in registration_files:
        relative = path.relative_to(vdb_root)
        target = backup_root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)
    return backup_root, had_destination


def _remove_backup(backup_root: Path) -> None:
    """成功后删除本次备份。
    EN: Remove this run's backup after a successful install.
    """
    shutil.rmtree(backup_root, ignore_errors=True)
    try:
        backup_root.parent.rmdir()
    except OSError:
        pass


def _rollback_hint(
    vdb_root: Path,
    destination: Path,
    registration_files: list[Path],
    backup_root: Path,
    had_destination: bool,
) -> None:
    """打印可手动执行的回滚命令。
    EN: Print manually executable rollback commands.
    """
    quote = lambda value: shlex.quote(str(value))  # noqa: E731
    print("\n手动回滚 / Manual rollback:", file=sys.stderr)
    print("  1) 恢复适配器目录 / restore adapter directory:", file=sys.stderr)
    print(f"     rm -rf {quote(destination)}", file=sys.stderr)
    if had_destination:
        print(f"     cp -a {quote(backup_root / DEST_REL)} {quote(destination)}", file=sys.stderr)
    print("  2) 恢复注册文件 / restore registration files:", file=sys.stderr)
    for path in registration_files:
        relative = path.relative_to(vdb_root)
        print(f"     cp -a {quote(backup_root / relative)} {quote(path)}", file=sys.stderr)
    print(f"  备份目录 / backup directory: {backup_root}", file=sys.stderr)


def _verify_installed(destination: Path, clients_init: Path, cli_file: Path) -> None:
    """验证安装文件与注册结果。
    EN: Verify installed files and registration entries.
    """
    for name in ("houyidb.py", "config.py", "cli.py"):
        path = destination / name
        if not path.is_file():
            raise InstallError(f"installed adapter file missing: {path}")
        _parse_syntax(path, _read(path))

    clients_text = _read(clients_init)
    if clients_text.count(ENUM_LINE) != 1 or clients_text.count("if self == DB.HouyiDB:") != 3:
        raise InstallError("client registry verification failed")
    cli_text = _read(cli_file)
    if cli_text.count(IMPORT_LINE) != 1 or cli_text.count(COMMAND_LINE) != 1:
        raise InstallError("CLI registration verification failed")


def _compile_destination(destination: Path) -> None:
    """对安装后的包运行 compileall。
    EN: Run compileall against the installed package.
    """
    result = subprocess.run(
        [sys.executable, "-m", "compileall", "-q", str(destination)],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise InstallError(
            "compileall failed for the installed adapter\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )


def install(vdb_root: Path) -> int:
    """执行安装。
    EN: Perform the installation.
    """
    backup_root: Path | None = None
    had_destination = False
    try:
        destination, clients_init, cli_file, source_package = _validate_paths(vdb_root)

        print(f"[1/4] 检查 VDB 目录 / validating VDB root: {vdb_root}")
        clients_text = _read(clients_init)
        cli_text = _read(cli_file)
        _parse_syntax(clients_init, clients_text)
        _parse_syntax(cli_file, cli_text)

        print("[2/4] 计算注册变更 / planning registration changes")
        new_clients_text, clients_changed, factory_changes = _patch_clients(clients_text)
        new_cli_text, cli_changed = _patch_cli(cli_text)
        enum_changed = ENUM_LINE not in clients_text
        print(
            "      enum: "
            + ("insert" if enum_changed else "already present")
            + f"; factories: {factory_changes}/3 inserted"
            + "; CLI: "
            + ("updated" if cli_changed else "already present")
        )

        print(f"[3/4] 安装适配器 / installing adapter: {destination}")
        backup_root, had_destination = _make_backup(vdb_root, destination, [clients_init, cli_file])
        if destination.exists():
            shutil.rmtree(destination)
        shutil.copytree(
            source_package,
            destination,
            ignore=shutil.ignore_patterns("__pycache__", "*.pyc"),
        )
        if clients_changed:
            _atomic_write(clients_init, new_clients_text)
        if cli_changed:
            _atomic_write(cli_file, new_cli_text)

        print("[4/4] 校验语法 / verifying syntax")
        _verify_installed(destination, clients_init, cli_file)
        _compile_destination(destination)
    except Exception as exc:  # noqa: BLE001
        print(f"\nERROR: {exc}", file=sys.stderr)
        if backup_root is not None:
            destination = vdb_root / DEST_REL
            registration_files = [vdb_root / CLIENTS_INIT_REL, vdb_root / CLI_REL]
            _rollback_hint(vdb_root, destination, registration_files, backup_root, had_destination)
        else:
            print("未写入任何文件 / no files were modified before the failure.", file=sys.stderr)
        return 1

    assert backup_root is not None
    _remove_backup(backup_root)
    print("\n安装完成 / installation complete.")
    print(f"  适配器 / adapter: {destination}")
    print("  注册 / registration: DB.HouyiDB, 3 factory branches, CLI subcommand")
    print("  自检 / smoke check:")
    print(f"    cd {shlex.quote(str(vdb_root))}")
    print("    vectordb_bench houyidb --help")
    return 0


def main(argv: list[str] | None = None) -> int:
    """解析参数并执行安装。
    EN: Parse arguments and run the installer.
    """
    parser = argparse.ArgumentParser(
        description="Install the HouyiDB adapter into a local VectorDBBench checkout."
    )
    parser.add_argument(
        "--vdb-root",
        required=True,
        type=Path,
        help="Path to the VectorDBBench source checkout.",
    )
    args = parser.parse_args(argv)
    return install(args.vdb_root.expanduser().resolve())


if __name__ == "__main__":
    raise SystemExit(main())
