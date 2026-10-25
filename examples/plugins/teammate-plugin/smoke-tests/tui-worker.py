#!/usr/bin/env python3
"""真实 TUI → Teammate WASM → Runtime worker 开发权限回归，不让主模型代写。

前置：cargo build -p lucia；bun run scripts/build-plugin.ts teammate-plugin。
运行：python3 -B examples/plugins/teammate-plugin/smoke-tests/tui-worker.py。
LUCIA_BIN 可指向已安装的 lucia；仅使用临时目录与本地假模型，不读取用户配置或调用外部模型。
"""

import errno
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time
import unittest

REPO = Path(__file__).resolve().parents[4]
BIN = Path(os.environ.get("LUCIA_BIN", REPO / "target/debug/lucia")).expanduser().resolve()
MANIFEST = REPO / "examples/plugins/teammate-plugin/plugin.toml"
FRAME_END = b"\x1b[?2026l"


def message_text(message):
    """读取本地假模型收到的 provider 文本，不依赖 UI 展示字符串。"""
    content = message.get("content") or ""
    return content if isinstance(content, str) else "".join(part.get("text", "") for part in content)


def tool_calls(request):
    """保留每次真实工具结果对应的名称，避免把意图误当执行成功。"""
    calls = {call["id"]: call["function"]["name"]
             for message in request["messages"] for call in message.get("tool_calls", [])}
    return [(calls[message["tool_call_id"]], json.loads(message["content"]))
            for message in request["messages"] if message["role"] == "tool"]


class WorkerDevelopmentTest(unittest.TestCase):
    """检查生产 profile，而非 smoke 里宽松的 AgentDeriveConfig::default()。"""

    def test_worker_writes_edits_verifies_and_continues(self):
        errors = []
        completed = threading.Event()
        worker_steps = []
        lock = threading.Lock()

        def call(name, args, index):
            return {"role": "assistant", "content": None, "tool_calls": [{
                "id": f"call-{index}", "type": "function",
                "function": {"name": name, "arguments": json.dumps(args)},
            }]}

        def reply(request):
            users = [message_text(message) for message in request["messages"] if message["role"] == "user"]
            worker = any(text.startswith("<teammate_assignment>") for text in users)
            results = tool_calls(request)
            names = {tool["function"]["name"] for tool in request.get("tools", [])}
            if worker:
                self.assertEqual(names, {"read_file", "list_directory", "search_files", "write_file", "edit_file", "shell", "skill_read"})
                # 每次 provider 调用检查前一工具结果，保证实际落盘/执行成功，而非只返回工具调用。
                if results:
                    name, result = results[-1]
                    if len(results) == 4:
                        self.assertIn("error", result, "越界写入应返回工具错误，而非成功信封")
                        self.assertIn("工作区", result["error"], "越界写入必须被原生工具拒绝")
                    else:
                        self.assertNotIsInstance(result, str, f"{name}: {result}")
                        self.assertNotIn("error", result, f"{name}: {result}")
                    if name == "shell":
                        self.assertEqual(result["exit_code"], 0, result)
                sequence = [
                    ("write_file", {"path": "member.txt", "content": "before\n"}),
                    ("read_file", {"path": "member.txt"}),
                    ("edit_file", {"path": "member.txt", "old_string": "before", "new_string": "after"}),
                    ("write_file", {"path": "../forbidden.txt", "content": "must not escape"}),
                    ("shell", {"command": "test \"$(cat member.txt)\" = after && test -z \"${LUCIA_WORKER_TEST_SECRET-}\" && printf verified > verified.txt"}),
                ]
                if len(results) < len(sequence):
                    name, args = sequence[len(results)]
                    with lock:
                        worker_steps.append(name)
                    return call(name, args, f"worker-{len(results)}")
                if any(text.startswith("<teammate_message>") for text in users):
                    if len(results) == len(sequence):
                        with lock:
                            worker_steps.append("shell-continue")
                        return call("shell", {"command": "test \"$(cat member.txt)\" = after && printf continued > continued.txt"}, "continue")
                    return {"role": "assistant", "content": "WORKER_CONTINUED"}
                return {"role": "assistant", "content": "WORKER_IMPLEMENTED"}

            # 主模型只能发起团队控制调用，绝不返回 write/edit/shell；文件必须由队员写入。
            self.assertIn("teammate_spawn", names)
            spawned = next((result for name, result in results if name == "teammate_spawn"), None)
            if spawned is None:
                return call("teammate_spawn", {"role": "implementer", "input": "Create member.txt, edit and verify it yourself; do not hand code back for the controller to write."}, 0)
            member_id = spawned["member"]["id"]
            sent = next((result for name, result in results if name == "teammate_send"), None)
            dispatched = any(name == "teammate_dispatch" for name, _ in results)
            finished = results[-1][0] == "teammate_result" and results[-1][1].get("completed")
            if finished:
                outcome = results[-1][1]["outcome"]
                self.assertEqual(outcome["status"], "succeeded", outcome)
                if dispatched:
                    self.assertEqual(outcome["result"]["final_text"], "WORKER_CONTINUED")
                    completed.set()
                    return {"role": "assistant", "content": "CONTROLLER_VERIFIED_WORKER"}
                return call("teammate_send", {"recipient": member_id, "topic": "continue", "payload": "Verify again using shell and create continued.txt."}, len(results))
            if sent and not dispatched:
                return call("teammate_dispatch", {"member_id": member_id, "message_id": sent["message"]["id"]}, len(results))
            time.sleep(0.03)
            return call("teammate_result", {"member_id": member_id}, len(results))

        class ModelHandler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    message = reply(request)
                except Exception as error:
                    errors.append(repr(error))
                    message = {"role": "assistant", "content": "TEST_FAILED"}
                body = json.dumps({"id": "worker-test", "object": "chat.completion", "model": "gpt-4o",
                                   "choices": [{"index": 0, "message": message,
                                                "finish_reason": "tool_calls" if "tool_calls" in message else "stop"}]}).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), ModelHandler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory(prefix="lucia-worker-tui-") as root:
                root = Path(root)
                workspace = root / "workspace"
                workspace.mkdir()
                home = root / "home"
                home.mkdir()
                skill = home / "skills/test/SKILL.md"
                skill.parent.mkdir(parents=True)
                skill.write_text("---\nname: test\ndescription: Test fixture only.\n---\nFixture body.\n")
                config = home / "config.json"
                config.write_text(json.dumps({
                    "model": {"name": "local", "provider": "open-ai-compatible", "model": "gpt-4o",
                              "models_dev_provider": "openai", "api_key": "local-fixture",
                              "base_url": f"http://127.0.0.1:{server.server_port}/v1", "openai_protocol": "chat-completions"},
                    "agent": {"stream": False, "max_steps": 64}, "tui": {"fullscreen": True},
                }))
                master, slave = pty.openpty()
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
                env = os.environ.copy()
                env.update(LUCIA_HOME=str(home), TERM="xterm-256color", COLORTERM="truecolor", LUCIA_WORKER_TEST_SECRET="not-for-child")
                output = bytearray()

                def attach_terminal():
                    os.setsid()
                    fcntl.ioctl(0, termios.TIOCSCTTY, 0)

                process = subprocess.Popen([str(BIN), "--config", str(config), "--plugin-manifest", str(MANIFEST)],
                                           cwd=workspace, env=env, stdin=slave, stdout=slave, stderr=slave,
                                           preexec_fn=attach_terminal)

                def pump():
                    if not select.select([master], [], [], 0.03)[0]:
                        return
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            return
                        raise
                    offset = len(output)
                    output.extend(chunk)
                    for query, response in [(b"\x1b[?u", b"\x1b[?1u"), (b"\x1b[c", b"\x1b[?1;2c")]:
                        at = output.find(query, max(0, offset - len(query) + 1))
                        if at >= 0 and at + len(query) > offset:
                            os.write(master, response)

                def wait_until(predicate, label):
                    deadline = time.monotonic() + 25
                    while time.monotonic() < deadline:
                        if errors:
                            self.fail(str(errors))
                        if predicate():
                            return
                        pump()
                    self.fail(f"{label}; output={bytes(output[-1500:])!r}")

                try:
                    wait_until(lambda: FRAME_END in output, "TUI 启动")
                    # 等待渐进插件加载，主请求再由真实工具目录证明插件已经 ready。
                    deadline = time.monotonic() + 1
                    while time.monotonic() < deadline:
                        pump()
                    os.write(master, b"\x1b[200~WORKER_IMPLEMENTATION_TEST\x1b[201~")
                    deadline = time.monotonic() + 0.1
                    while time.monotonic() < deadline:
                        pump()
                    os.write(master, b"\r")
                    wait_until(completed.is_set, "队员实现、验证和续跑完成")
                    self.assertEqual((workspace / "member.txt").read_text(), "after\n")
                    self.assertEqual((workspace / "verified.txt").read_text(), "verified")
                    self.assertEqual((workspace / "continued.txt").read_text(), "continued")
                    self.assertFalse((root / "forbidden.txt").exists())
                    self.assertEqual(worker_steps, ["write_file", "read_file", "edit_file", "write_file", "shell", "shell-continue"])
                    self.assertFalse(errors)
                    os.write(master, b"\x04")
                    wait_until(lambda: process.poll() is not None, "TUI 退出")
                    self.assertEqual(process.returncode, 0)
                finally:
                    if process.poll() is None:
                        process.kill()
                    os.close(master)
                    os.close(slave)
                    process.wait(timeout=10)
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
