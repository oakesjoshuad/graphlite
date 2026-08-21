#!/usr/bin/env python3
"""Read-only Phase 2 rust-analyzer/LSP evaluation harness.

This intentionally lives outside graphlite's production binary. It starts a
fresh rust-analyzer, records readiness signals, requests per-file document
symbols, probes trait implementations, and optionally compares symbol
locations with an existing graphlite SQLite index. It never opens the DB for
writing and never modifies source files.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import select
import sqlite3
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import quote


class LspError(RuntimeError):
    pass


class Rpc:
    def __init__(self, root: Path, server: str):
        self.proc = subprocess.Popen(
            [server],
            cwd=root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.next_id = 1
        self.notifications: list[dict] = []
        self.buffer = b""

    def send(self, message: dict) -> None:
        raw = json.dumps(message, separators=(",", ":")).encode()
        assert self.proc.stdin is not None
        self.proc.stdin.write(f"Content-Length: {len(raw)}\r\n\r\n".encode() + raw)
        self.proc.stdin.flush()

    def reply(self, request: dict, result=None) -> None:
        self.send({"jsonrpc": "2.0", "id": request.get("id"), "result": result})

    def request(self, method: str, params, timeout: float) -> dict:
        ident = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            message = self.receive(max(0.1, deadline - time.monotonic()))
            if message.get("method") and "id" in message:
                self.reply(message, [] if message["method"] == "workspace/configuration" else None)
                continue
            if message.get("method"):
                self.notifications.append(message)
                continue
            if message.get("id") == ident:
                if "error" in message:
                    raise LspError(f"{method}: {message['error']}")
                return message
        raise LspError(f"timeout waiting for {method} id={ident}")

    def receive(self, timeout: float) -> dict:
        assert self.proc.stdout is not None
        deadline = time.monotonic() + timeout
        while b"\r\n\r\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError
            ready, _, _ = select.select([self.proc.stdout.fileno()], [], [], remaining)
            if not ready:
                raise TimeoutError
            chunk = os.read(self.proc.stdout.fileno(), 4096)
            if not chunk:
                raise LspError("rust-analyzer exited before sending a complete message")
            self.buffer += chunk
        header_bytes, self.buffer = self.buffer.split(b"\r\n\r\n", 1)
        length = None
        for line in header_bytes.decode("ascii", errors="replace").split("\r\n"):
            if line.lower().startswith("content-length:"):
                length = int(line.split(":", 1)[1].strip())
        if length is None:
            raise LspError(f"missing Content-Length in {header_bytes!r}")
        while len(self.buffer) < length:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError
            ready, _, _ = select.select([self.proc.stdout.fileno()], [], [], remaining)
            if not ready:
                raise TimeoutError
            self.buffer += os.read(self.proc.stdout.fileno(), 4096)
        body, self.buffer = self.buffer[:length], self.buffer[length:]
        return json.loads(body)

    def drain_until(self, predicate, timeout: float) -> tuple[dict | None, bool]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.receive(max(0.05, deadline - time.monotonic()))
            except TimeoutError:
                return None, False
            if "method" in message:
                self.notifications.append(message)
                if message["method"] in {
                    "workspace/configuration",
                    "client/registerCapability",
                    "window/workDoneProgress/create",
                } and "id" in message:
                    self.reply(message, [] if message["method"] == "workspace/configuration" else None)
                if predicate(message):
                    return message, True
            elif predicate(message):
                return message, True
        return None, False

    def close(self) -> None:
        if self.proc.poll() is None:
            try:
                self.request("shutdown", None, 5)
            except (LspError, TimeoutError, BrokenPipeError):
                pass
            try:
                self.send({"jsonrpc": "2.0", "method": "exit", "params": None})
            except (BrokenPipeError, OSError):
                pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()

    def diagnostics(self) -> str:
        if self.proc.stderr is None:
            return ""
        if self.proc.poll() is None:
            return ""
        try:
            return self.proc.stderr.read().decode(errors="replace")[-2000:]
        except (OSError, ValueError):
            return ""


def uri(path: Path) -> str:
    return "file://" + quote(str(path.resolve()), safe="/:~")


def rust_files(root: Path) -> list[Path]:
    ignored = {"target", ".git", ".graphlite", ".strata"}
    return sorted(p for p in root.rglob("*.rs") if not ignored.intersection(p.parts))


def open_document(rpc: Rpc, path: Path, text: str) -> None:
    rpc.send(
        {
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri(path),
                    "languageId": "rust",
                    "version": 1,
                    "text": text,
                }
            },
        }
    )


def flatten_symbols(value, out: list[tuple[str, int, str]], prefix: str = "") -> None:
    if not isinstance(value, dict):
        return
    name = value.get("name")
    start = value.get("range", {}).get("start", {}).get("line")
    if isinstance(name, str) and isinstance(start, int):
        qualified_name = f"{prefix}::{name}" if prefix else name
        out.append((name, start + 1, qualified_name))
        prefix = qualified_name
    for child in value.get("children", []) or []:
        flatten_symbols(child, out, prefix)


def symbol_rows(result) -> list[tuple[str, int, str]]:
    rows: list[tuple[str, int, str]] = []
    for item in result or []:
        flatten_symbols(item, rows)
        if isinstance(item, dict) and "location" in item:
            name = item.get("name")
            start = item.get("location", {}).get("range", {}).get("start", {}).get("line")
            if isinstance(name, str) and isinstance(start, int):
                rows.append((name, start + 1, name))
    return rows


def trait_probes(files: list[Path], limit: int) -> list[tuple[Path, int, int, str]]:
    if limit <= 0:
        return []
    probes = []
    pattern = re.compile(
        r"^\s*(?:(?:pub)(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)"
    )
    for path in files:
        for line_no, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
            match = pattern.search(line)
            if match:
                probes.append((path, line_no, match.start(1), match.group(1)))
                if len(probes) >= limit:
                    return probes
    return probes


def graph_symbols(db: Path, root: Path) -> dict[tuple[str, int], set[str]]:
    if not db.exists():
        return {}
    result: dict[tuple[str, int], set[str]] = {}
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        for name, file, line in conn.execute("SELECT name, file, range_start FROM nodes"):
            path = Path(file)
            if not path.is_absolute():
                path = (root / path).resolve()
            key = (str(path), int(line))
            result.setdefault(key, set()).add(name)
    finally:
        conn.close()
    return result


def graph_qualified_names(db: Path, root: Path) -> dict[str, list[tuple[int, int, str, str]]]:
    if not db.exists():
        return {}
    result: dict[str, list[tuple[int, int, str]]] = {}
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        for qualified_name, file, start, end, kind, name in conn.execute(
            "SELECT qualified_name, file, range_start, range_end, kind, name FROM nodes "
            "WHERE qualified_name IS NOT NULL AND qualified_name != ''"
        ):
            if kind == "crate":
                continue
            path = Path(file)
            if not path.is_absolute():
                path = (root / path).resolve()
            result.setdefault(str(path), []).append(
                (int(start), int(end), name, qualified_name)
            )
    finally:
        conn.close()
    return result


def qualified_names_at(
    qualified_truth: dict[str, list[tuple[int, int, str, str]]],
    path: Path,
    line: int,
    symbol_name: str,
) -> set[str]:
    """Return graph names matched by an LSP symbol's range or leaf name."""
    return {
        qualified_name
        for start, end, name, qualified_name in qualified_truth.get(str(path.resolve()), [])
        if (start <= line <= end) or name == symbol_name
    }


def graph_impl_edges(db: Path) -> set[tuple[str, str]]:
    if not db.exists():
        return set()
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return set(
            conn.execute(
                "SELECT n1.qualified_name, n2.qualified_name "
                "FROM edges e JOIN nodes n1 ON n1.id = e.from_id "
                "JOIN nodes n2 ON n2.id = e.to_id "
                "WHERE e.edge_type = 'IMPL_TRAIT'"
            ).fetchall()
        )
    finally:
        conn.close()


def rustdoc_impl_edges(json_dir: Path) -> set[tuple[str, str]]:
    """Extract local and cross-crate impl pairs from typed rustdoc JSON data."""
    if not json_dir.exists():
        return set()
    edges = set()
    for path in sorted(json_dir.glob("*.json")):
        try:
            document = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        paths = document.get("paths", {})

        def path_name(item_id) -> str | None:
            entry = paths.get(str(item_id))
            if not entry or not entry.get("path"):
                return None
            return "::".join(entry["path"][1:])

        for item in document.get("index", {}).values():
            impl = item.get("inner", {}).get("impl")
            if not impl or item.get("crate_id") != 0 or impl.get("is_synthetic"):
                continue
            trait = impl.get("trait")
            target = impl.get("for", {}).get("resolved_path")
            if not trait or not target:
                continue
            type_name = path_name(target.get("id"))
            trait_name = path_name(trait.get("id"))
            if type_name and trait_name:
                edges.add((type_name, trait_name))
    return edges


def graph_impl_type_at(db: Path, root: Path, file: str, line: int) -> str | None:
    if not db.exists():
        return None
    path = Path(file)
    if not path.is_absolute():
        path = (root / path).resolve()
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        row = conn.execute(
            "SELECT qualified_name FROM nodes "
            "WHERE kind = 'impl' AND range_start <= ? AND range_end >= ? "
            "AND (file = ? OR file = ? OR file = ?) "
            "ORDER BY (range_end - range_start), id LIMIT 1",
            (line, line, str(path), f"./{path.relative_to(root)}", str(path.relative_to(root))),
        ).fetchone()
        return row[0] if row else None
    finally:
        conn.close()


def graph_symbol_at(db: Path, root: Path, file: Path, line: int, name: str) -> str | None:
    if not db.exists():
        return None
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        row = conn.execute(
            "SELECT qualified_name FROM nodes WHERE name = ? AND range_start = ? "
            "AND (file = ? OR file = ? OR file = ?)",
            (
                name,
                line,
                str(file.resolve()),
                f"./{file.resolve().relative_to(root)}",
                str(file.resolve().relative_to(root)),
            ),
        ).fetchone()
        return row[0] if row else None
    finally:
        conn.close()


def source_impl_type(file: Path, line: int) -> str | None:
    try:
        source = file.read_text(errors="replace").splitlines()[line - 1]
    except (OSError, IndexError):
        return None
    match = re.search(r"\bfor\s+([A-Za-z_][A-Za-z0-9_]*(?:<[^>{}]+>)?)\s*\{", source)
    if not match:
        return None
    return re.sub(r"<.*>$", "", match.group(1))


def normalize_lsp_qualified_name(value: str) -> str:
    """Remove LSP-only impl-container labels before parity comparison."""
    parts = []
    for part in value.split("::"):
        match = re.match(r"impl\s+.+?\s+for\s+(.+)$", part)
        if match:
            parts.append(match.group(1))
        elif part.startswith("impl "):
            parts.append(part.removeprefix("impl "))
        else:
            parts.append(part)
    return "::".join(parts)


def equivalent_qualified_name(actual: str, expected: str) -> bool:
    actual = normalize_lsp_qualified_name(actual)
    return actual == expected or actual.endswith(f"::{expected}") or expected.endswith(
        f"::{actual}"
    )


def run_once(
    root: Path, db: Path, args, files: list[Path], ground_truth, qualified_truth, impl_edges
) -> dict:
    started = time.monotonic()
    rpc = None
    statuses = []
    progress_end = 0
    try:
        rpc = Rpc(root, args.server)
        init = rpc.request(
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": uri(root),
                "workspaceFolders": [{"uri": uri(root), "name": root.name}],
                "capabilities": {
                    "workspace": {"configuration": True},
                    "experimental": {"serverStatusNotification": True},
                    "textDocument": {
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}
                    },
                },
                "initializationOptions": {},
            },
            args.request_timeout,
        )
        rpc.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        ready_at = None
        readiness_deadline = time.monotonic() + args.readiness_timeout
        while time.monotonic() < readiness_deadline:
            try:
                message = rpc.receive(min(0.25, readiness_deadline - time.monotonic()))
            except TimeoutError:
                continue
            if message.get("method") == "experimental/serverStatus":
                params = message.get("params", {})
                status = {
                    "health": params.get("health"),
                    "quiescent": params.get("quiescent"),
                    "message": params.get("message"),
                    "elapsed_s": round(time.monotonic() - started, 3),
                }
                statuses.append(status)
                if params.get("health") not in (None, "ok"):
                    raise LspError(
                        f"rust-analyzer reported unhealthy status: {params.get('health')}"
                    )
                if params.get("health") == "ok" and params.get("quiescent") and ready_at is None:
                    ready_at = time.monotonic()
                    break
            elif message.get("method") == "$/progress":
                if message.get("params", {}).get("value", {}).get("kind") == "end":
                    progress_end = time.monotonic()
            elif "id" in message and message.get("method") in {
                "workspace/configuration",
                "client/registerCapability",
                "window/workDoneProgress/create",
            }:
                rpc.reply(message, [] if message["method"] == "workspace/configuration" else None)
            elif message.get("method"):
                rpc.notifications.append(message)

        if ready_at is None:
            raise LspError(
                f"readiness timeout after {args.readiness_timeout:.3f}s without healthy quiescence"
            )

        symbol_results = []
        selected_files = files[: args.max_files]
        selected_file_keys = {str(path.resolve()) for path in selected_files}
        observed_qualified_names: dict[tuple[str, int], list[tuple[str, str]]] = {}
        ground_truth_names = {}
        for (truth_file, _), names in ground_truth.items():
            ground_truth_names.setdefault(truth_file, set()).update(names)
        for path in selected_files:
            text = path.read_text(errors="replace")
            open_document(rpc, path, text)
        for path in selected_files:
            response = rpc.request(
                "textDocument/documentSymbol",
                {"textDocument": {"uri": uri(path)}},
                args.request_timeout,
            )
            rows = symbol_rows(response.get("result"))
            symbol_results.append(
                {
                    "file": str(path.relative_to(root)),
                    "count": len(rows),
                    "symbols": [
                        {"name": name, "line": line, "qualified_name": qualified_name}
                        for name, line, qualified_name in rows
                    ],
                }
            )
            for name, line, qualified_name in rows:
                key = (str(path.resolve()), line)
                observed_qualified_names.setdefault(key, []).append((name, qualified_name))
                if ground_truth:
                    symbol_results[-1].setdefault("matched", 0)
                    symbol_results[-1].setdefault("name_matched", 0)
                    if name in ground_truth.get(key, set()):
                        symbol_results[-1]["matched"] += 1
                    if name in ground_truth_names.get(str(path.resolve()), set()):
                        symbol_results[-1]["name_matched"] += 1
                    symbol_results[-1].setdefault("qualified_name_matched", 0)
                    expected_qnames = qualified_names_at(
                        qualified_truth, path, line, name
                    )
                    if qualified_name in expected_qnames:
                        symbol_results[-1]["qualified_name_matched"] += 1
                    symbol_results[-1].setdefault("normalized_qualified_name_matched", 0)
                    if any(
                        equivalent_qualified_name(qualified_name, expected)
                        for expected in expected_qnames
                    ):
                        symbol_results[-1]["normalized_qualified_name_matched"] += 1

        implementation_results = []
        for path, line, character, name in trait_probes(files, args.max_impl_probes):
            response = rpc.request(
                "textDocument/implementation",
                {
                    "textDocument": {"uri": uri(path)},
                    "position": {"line": line - 1, "character": character},
                },
                args.request_timeout,
            )
            value = response.get("result")
            locations = []
            for item in value or [] if isinstance(value, list) else []:
                location = item.get("uri") if isinstance(item, dict) else None
                range_start = item.get("range", {}).get("start") if isinstance(item, dict) else None
                if location and isinstance(range_start, dict):
                    locations.append(
                        {
                            "uri": location,
                            "line": range_start.get("line", 0) + 1,
                            "character": range_start.get("character", 0),
                        }
                    )
            trait_qname = graph_symbol_at(db, root, path, line, name)
            mapped_edges = []
            for location in locations:
                impl_qname = graph_impl_type_at(
                    db, root, Path(location["uri"][7:]), location["line"]
                )
                if impl_qname is None:
                    impl_qname = source_impl_type(Path(location["uri"][7:]), location["line"])
                if impl_qname and trait_qname:
                    mapped_edges.append((impl_qname, trait_qname))
            implementation_results.append(
                {
                    "trait": name,
                    "file": str(path.relative_to(root)),
                    "line": line,
                    "result_count": len(value) if isinstance(value, list) else (0 if value is None else 1),
                    "locations": locations,
                    "mapped_edges": mapped_edges,
                    "graph_edge_matches": sum(edge in impl_edges for edge in mapped_edges),
                }
            )

        total_symbols = sum(row["count"] for row in symbol_results)
        matched = sum(row.get("matched", 0) for row in symbol_results)
        name_matched = sum(row.get("name_matched", 0) for row in symbol_results)
        qualified_name_matched = sum(
            row.get("qualified_name_matched", 0) for row in symbol_results
        )
        normalized_qualified_name_matched = sum(
            row.get("normalized_qualified_name_matched", 0) for row in symbol_results
        )
        indexed_qname_total = 0
        indexed_qname_covered = 0
        for truth_file, ranges in qualified_truth.items():
            if truth_file not in selected_file_keys:
                continue
            for start, end, node_name, expected in ranges:
                indexed_qname_total += 1
                indexed_qname_covered += any(
                    equivalent_qualified_name(observed, expected)
                    for (observed_file, observed_line), observed_names in observed_qualified_names.items()
                    if observed_file == truth_file
                    for observed_name, observed in observed_names
                    if (start <= observed_line <= end) or observed_name == node_name
                )
        mapped_edges = [
            edge
            for result in implementation_results
            for edge in result.get("mapped_edges", [])
        ]
        graph_edge_matches = sum(edge in impl_edges for edge in mapped_edges)
        mapped_edge_set = set(mapped_edges)
        return {
            "ok": True,
            "elapsed_s": round(time.monotonic() - started, 3),
            "initialize_server_capabilities": init.get("result", {}).get("capabilities", {}),
            "readiness": {
                "quiescent_seen": ready_at is not None,
                "first_quiescent_s": round(ready_at - started, 3) if ready_at else None,
                "last_progress_end_s": round(progress_end - started, 3) if progress_end else None,
                "statuses": statuses,
            },
            "document_symbols": {
                "files_requested": len(selected_files),
                "symbols_returned": total_symbols,
                "ground_truth_name_line_matches": matched if ground_truth else None,
                "ground_truth_name_matches": name_matched if ground_truth else None,
                "ground_truth_qualified_name_matches": (
                    qualified_name_matched if qualified_truth else None
                ),
                "ground_truth_normalized_qualified_name_matches": (
                    normalized_qualified_name_matched if qualified_truth else None
                ),
                "graph_indexed_qualified_name_total": (
                    indexed_qname_total if qualified_truth else None
                ),
                "graph_indexed_qualified_name_covered": (
                    indexed_qname_covered if qualified_truth else None
                ),
                "files": symbol_results,
            },
                "implementation_probes": implementation_results,
                "mapped_impl_edges": mapped_edges,
                "mapped_impl_edge_matches": graph_edge_matches,
                "graph_impl_edge_count": len(impl_edges) if impl_edges else None,
                "lsp_edges_missing_from_graph": sorted(mapped_edge_set - impl_edges),
                "graph_edges_missing_from_lsp": sorted(impl_edges - mapped_edge_set),
        }
    except (LspError, TimeoutError, OSError, UnicodeError) as exc:
        return {
            "ok": False,
            "elapsed_s": round(time.monotonic() - started, 3),
            "error": str(exc) or type(exc).__name__,
            "server_stderr": rpc.diagnostics() if rpc else "",
            "readiness": {"statuses": statuses},
        }
    finally:
        if rpc:
            rpc.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--server", default="rust-analyzer")
    parser.add_argument("--db", type=Path, default=None)
    parser.add_argument("--rustdoc-json-dir", type=Path, default=None)
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--max-files", type=int, default=100)
    parser.add_argument("--max-impl-probes", type=int, default=20)
    parser.add_argument("--readiness-timeout", type=float, default=30)
    parser.add_argument("--request-timeout", type=float, default=15)
    parser.add_argument("--output", type=Path, default=None)
    args = parser.parse_args()
    root = args.root.resolve()
    files = rust_files(root)
    db = (args.db or root / ".graphlite" / "codegraph.db").resolve()
    ground_truth = graph_symbols(db, root)
    qualified_truth = graph_qualified_names(db, root)
    impl_edges = graph_impl_edges(db)
    rustdoc_edges = rustdoc_impl_edges(
        (args.rustdoc_json_dir or root / ".graphlite" / "rustdoc-json" / "doc").resolve()
    )
    runs = [
        run_once(root, db, args, files, ground_truth, qualified_truth, impl_edges)
        for _ in range(max(1, args.runs))
    ]
    try:
        version = subprocess.run(
            [args.server, "--version"], capture_output=True, text=True, check=False
        ).stdout.strip()
    except OSError as exc:
        version = f"unavailable: {exc}"
    report = {
        "root": str(root),
        "rust_analyzer": version,
        "files_discovered": len(files),
        "ground_truth_symbols": sum(len(names) for names in ground_truth.values()),
        "ground_truth_qualified_names": sum(len(ranges) for ranges in qualified_truth.values()),
        "rustdoc_json_impl_edges": sorted(rustdoc_edges),
        "rustdoc_json_impl_edge_count": len(rustdoc_edges),
        "options": {
            key: str(value) if isinstance(value, Path) else value
            for key, value in vars(args).items()
        }
        | {"root": str(root), "db": str(db), "output": str(args.output) if args.output else None},
        "runs": runs,
    }
    encoded = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(encoded)
    else:
        print(encoded, end="")
    return 0 if all(run.get("ok") for run in runs) else 1


if __name__ == "__main__":
    sys.exit(main())
