#!/usr/bin/env python3
"""Measures what two Kanzo builds cost to run, side by side, and prints JSON.

    measure.py --base BIN Cargo.lock --head BIN Cargo.lock [--pr N] [--rounds 3]

Each build gets its own throwaway data folder and a board of 3,000 tasks. The
builds take turns, round after round, so a busy runner slows both alike; every
number is the median of the rounds. Peak memory needs Linux's /proc.

Only the standard library, so it runs on any runner without installing anything.
"""

import argparse
import http.client
import json
import os
import socket
import statistics
import subprocess
import tempfile
import time

TASKS = 3000
STARTS = 10
BOARD_LOADS = 20
DESCRIPTION = (
    "Users on slow connections see the board flash empty before the tasks load.\n\n"
    "- Keep the last board in memory between polls\n"
    "- Only redraw the columns that changed\n"
    "- Check it on Safari 13.1, Chrome 80 and Firefox 78\n"
)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def proc_status(pid):
    """Resident memory now and at its peak, in bytes, and the thread count."""
    if not os.path.exists("/proc"):
        # Elsewhere, like a Mac trying the script, there's only memory now.
        rss = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)])) * 1024
        threads = len(subprocess.check_output(["ps", "-M", "-p", str(pid)]).splitlines()) - 1
        return rss, rss, threads
    fields = {}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            key, _, value = line.partition(":")
            fields[key] = value.split()[0] if value.split() else ""
    return int(fields["VmRSS"]) * 1024, int(fields["VmHWM"]) * 1024, int(fields["Threads"])


def tree_size(path):
    return sum(os.path.getsize(os.path.join(d, f)) for d, _, files in os.walk(path) for f in files)


class Board:
    """A running `kanzo` board on a free port, with a keep-alive connection to it."""

    def __init__(self, binary, home):
        self.port = free_port()
        self.proc = subprocess.Popen(
            [binary, "--home", home, "--port", str(self.port), "--no-open"],
            stdout=subprocess.DEVNULL,
        )
        start = time.perf_counter()
        while True:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                break
            except OSError:
                if self.proc.poll() is not None:
                    raise SystemExit(f"{binary} exited with {self.proc.returncode}")
                if time.perf_counter() - start > 10:
                    raise SystemExit(f"{binary} did not listen within 10 s")
                time.sleep(0.0002)
        self.start_ms = (time.perf_counter() - start) * 1000
        self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)

    def request(self, method, path, body=None):
        headers = {"Content-Type": "application/json"} if body is not None else {}
        self.conn.request(method, path, body=json.dumps(body) if body is not None else None, headers=headers)
        res = self.conn.getresponse()
        data = res.read()
        if res.status >= 400:
            raise SystemExit(f"{method} {path}: {res.status} {data[:200]!r}")
        return data

    def json(self, method, path, body=None):
        return json.loads(self.request(method, path, body))

    def stop(self):
        self.conn.close()
        self.proc.terminate()
        self.proc.wait(timeout=10)


def seed(board):
    """A busy project spread over every column, and a small one for agents' replies."""
    board.json("POST", "/api/projects", {"slug": "bench", "name": "Bench"})
    statuses = board.json("GET", "/api/board")["statuses"]
    for i in range(TASKS):
        task = board.json(
            "POST",
            "/api/tasks",
            {"project": "bench", "title": f"Keep the board steady while it reloads ({i})", "description": DESCRIPTION},
        )
        status = statuses[i % len(statuses)]
        if status != statuses[0]:
            board.json("PATCH", f"/api/tasks/{task['id']}", {"status": status})
        if i % 5 == 0:
            board.json("POST", f"/api/tasks/{task['id']}/comments", {"body": "Started on this; the poll is next."})

    board.json("POST", "/api/projects", {"slug": "small", "name": "Small"})
    ids = []
    for i in range(8):
        task = board.json(
            "POST", "/api/tasks", {"project": "small", "title": f"Small task number {i + 1}", "description": DESCRIPTION}
        )
        ids.append(task["id"])
    for body in ("Looked into it: the cache is cold on first load.", "Fixed in the last commit."):
        board.json("POST", f"/api/tasks/{ids[0]}/comments", {"body": body})
    return ids


class Mcp:
    """`kanzo mcp` on stdio, as an agent starts it, in a folder that isn't a git repo."""

    def __init__(self, binary, home, cwd):
        self.proc = subprocess.Popen(
            [binary, "--home", home, "mcp"], cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
        )
        self.next_id = 0

    def send(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.next_id += 1
            msg["id"] = self.next_id
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        if notify:
            return None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise SystemExit(f"kanzo mcp closed during {method}")
            reply = json.loads(line)
            if reply.get("id") == self.next_id:
                if "error" in reply:
                    raise SystemExit(f"{method}: {reply['error']}")
                return reply["result"]

    def call(self, tool, args):
        result = self.send("tools/call", {"name": tool, "arguments": args})
        return "".join(c.get("text", "") for c in result.get("content", []))

    def stop(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)


def measure(binary, lock):
    binary = os.path.abspath(binary)  # `kanzo mcp` runs from another folder
    out = {"binary_bytes": os.path.getsize(binary)}
    with open(lock) as f:
        out["crates"] = sum(1 for line in f if line.strip() == "[[package]]") - 1  # Kanzo itself

    with tempfile.TemporaryDirectory() as tmp:
        home = os.path.join(tmp, "home")
        subprocess.run([binary, "--home", home, "init"], check=True, stdout=subprocess.DEVNULL)

        starts = []
        for _ in range(STARTS):
            board = Board(binary, home)
            board.request("GET", "/api/ping")
            starts.append(board.start_ms)
            board.stop()
        out["start_ms"] = statistics.median(starts)

        board = Board(binary, home)
        board.request("GET", "/api/ping")
        time.sleep(1)
        out["idle_rss_bytes"], _, out["idle_threads"] = proc_status(board.proc.pid)
        out["page_bytes"] = sum(len(board.request("GET", p)) for p in ("/", "/app.js", "/logo.png"))
        out["empty_db_bytes"] = tree_size(home)

        ids = seed(board)
        loads, size = [], 0
        for _ in range(BOARD_LOADS):
            start = time.perf_counter()
            size = len(board.request("GET", "/api/board"))
            loads.append((time.perf_counter() - start) * 1000)
        out["board_ms"] = statistics.median(loads)
        out["board_bytes"] = size
        time.sleep(1)
        out["loaded_rss_bytes"], out["peak_rss_bytes"], out["loaded_threads"] = proc_status(board.proc.pid)

        mcp = Mcp(binary, home, tmp)
        init = mcp.send(
            "initialize",
            {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "1"}},
        )
        mcp.send("notifications/initialized", notify=True)
        out["instructions"] = init.get("instructions", "")
        out["tools"] = mcp.send("tools/list")["tools"]
        out["tools_bytes"] = len(json.dumps(out["tools"], separators=(",", ":")))
        out["replies"] = {
            "list_projects": mcp.call("list_projects", {}),
            "list_tasks (8)": mcp.call("list_tasks", {"project": "small"}),
            "get_task": mcp.call("get_task", {"id": f"small#{ids[0]}"}),
            "next_task": mcp.call("next_task", {"project": "small"}),
            "add_comment": mcp.call("add_comment", {"id": f"small#{ids[1]}", "body": "Done the first half."}),
            "update_task": mcp.call("update_task", {"id": f"small#{ids[1]}", "status": "review"}),
            "create_task": mcp.call("create_task", {"project": "small", "title": "Write the docs"}),
        }
        out["mcp_rss_bytes"], out["mcp_peak_rss_bytes"], _ = proc_status(mcp.proc.pid)
        mcp.stop()

        board.stop()
        out["db_bytes"] = tree_size(home)
    return out


def median_of(rounds):
    """Numbers as the median of the rounds; text from the last round."""
    merged = {}
    for key, last in rounds[-1].items():
        if isinstance(last, (int, float)) and not isinstance(last, bool):
            merged[key] = statistics.median(r[key] for r in rounds)
        else:
            merged[key] = last
    return merged


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", nargs=2, metavar=("BIN", "LOCK"), required=True)
    parser.add_argument("--head", nargs=2, metavar=("BIN", "LOCK"), required=True)
    parser.add_argument("--pr", type=int)
    parser.add_argument("--rounds", type=int, default=3)
    args = parser.parse_args()

    rounds = {"base": [], "head": []}
    for _ in range(args.rounds):
        for name in rounds:
            rounds[name].append(measure(*getattr(args, name)))
    result = {name: median_of(r) for name, r in rounds.items()}
    result.update(pr=args.pr, tasks=TASKS, rounds=args.rounds)
    print(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
