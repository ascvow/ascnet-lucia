#!/usr/bin/env python3
"""用本地受控模型和真实 PTY 验证队列、Ctrl+Enter 及取消恢复；不访问用户配置或外部服务。

先 cargo build -p lucia，再 python3 -B scripts/pty-queue-check.py；LUCIA_BIN 可指定纯 Core 产物。
"""

import errno
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time
import unittest

REPO = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("LUCIA_BIN", REPO / "target/debug/lucia")).expanduser().resolve()
FRAME_END = b"\x1b[?2026l"


class QueuePtyTest(unittest.TestCase):
    """模型响应由测试释放，确保按键发生在真实运行期间而非依赖网络时序。"""

    def test_priority_send_and_cancelled_queue_survive_reload_consistently(self):
        self.check_priority_queue(complete_priority=False)

    def test_completed_priority_send_automatically_drains_queue(self):
        self.check_priority_queue(complete_priority=True)

    def check_priority_queue(self, complete_priority):
        """共用真实按键场景，分别验证插队完成后续跑和主动取消后暂停。"""
        requests = []
        release = threading.Event()

        class ModelHandler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                # 标题生成是独立模型调用，不计入用户提交的调度顺序，也不阻塞它。
                is_title = any(message.get("role") == "system" and "<title_instructions>" in str(message.get("content"))
                               for message in request["messages"])
                if not is_title:
                    requests.append(request)
                    if not release.wait(30):
                        return
                response = json.dumps({
                    "id": "queue-test", "object": "chat.completion", "model": "queue-test",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "Done"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11},
                }).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(response)))
                    self.end_headers()
                    self.wfile.write(response)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), ModelHandler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory(prefix="lucia-queue-pty-") as root:
                root = Path(root)
                config = root / "config.json"
                config.write_text(json.dumps({
                    "model": {"name": "local", "provider": "open-ai-compatible", "model": "gpt-4o", "models_dev_provider": "openai",
                              "api_key": "local-test-only", "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                              "openai_protocol": "chat-completions"},
                    "agent": {"stream": False, "max_steps": 8},
                    "tui": {"fullscreen": True},
                }))
                master, slave = pty.openpty()
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
                env = os.environ.copy()
                env.update(LUCIA_HOME=str(root), TERM="xterm-256color", COLORTERM="truecolor")
                output = bytearray()

                def attach_terminal():
                    os.setsid()
                    fcntl.ioctl(0, termios.TIOCSCTTY, 0)

                def start():
                    return subprocess.Popen([str(BIN), "--config", str(config), "--session-id", "queue-pty"],
                                            cwd=root, env=env, stdin=slave, stdout=slave, stderr=slave,
                                            preexec_fn=attach_terminal)

                process = start()

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
                    for query, reply in [(b"\x1b[?u", b"\x1b[?1u"), (b"\x1b[c", b"\x1b[?1;2c")]:
                        at = output.find(query, max(0, offset - len(query) + 1))
                        if at >= 0 and at + len(query) > offset:
                            os.write(master, reply)

                def wait_until(predicate, description):
                    deadline = time.monotonic() + 8
                    while time.monotonic() < deadline:
                        if predicate():
                            return
                        pump()
                    self.fail(f"等待失败：{description}; output={bytes(output[-1200:])!r}")

                def settle():
                    deadline = time.monotonic() + 0.15
                    while time.monotonic() < deadline:
                        pump()

                def send(text, enter=b"\r"):
                    mark = len(output)
                    os.write(master, b"\x1b[200~" + text.encode() + b"\x1b[201~")
                    wait_until(lambda: FRAME_END in output[mark:], "粘贴重绘")
                    os.write(master, enter)
                    settle()

                def latest_user(request):
                    content = next(message["content"] for message in reversed(request["messages"]) if message["role"] == "user")
                    if isinstance(content, str):
                        return content
                    return "".join(block.get("text", "") for block in content)

                def session_record():
                    for path in root.rglob("*.json"):
                        if path == config:
                            continue
                        record = json.loads(path.read_text())
                        if record.get("id") == "queue-pty":
                            return record
                    return None

                def completed_replies():
                    record = session_record()
                    return sum(message["role"] == "assistant" for message in record["session"]["messages"]) if record else 0

                try:
                    wait_until(lambda: FRAME_END in output, "启动")
                    send("FIRST_RUN")
                    wait_until(lambda: len(requests) == 1, "首个模型请求")
                    send("QUEUED_ONE")
                    send("QUEUED_TWO")
                    self.assertEqual(len(requests), 1, "普通 Enter 应只排队")
                    send("DRAFT_PRIORITY", b"\x1b[13;5u")
                    wait_until(lambda: len(requests) == 2, "草稿 Ctrl+Enter 抢先启动")
                    self.assertEqual(latest_user(requests[1]), "DRAFT_PRIORITY")
                    self.assertNotIn("QUEUED_ONE", json.dumps(requests[1]["messages"]))

                    # 队列聚焦后选第二项，Ctrl+Enter 必须打断当前优先任务，而非只置顶。
                    os.write(master, b"\x0f")
                    settle()
                    os.write(master, b"\x1b[B")
                    settle()
                    os.write(master, b"\x1b[13;5u")
                    wait_until(lambda: len(requests) == 3, "队列 Ctrl+Enter 抢先启动")
                    self.assertEqual(latest_user(requests[2]), "QUEUED_TWO")
                    self.assertNotIn("QUEUED_ONE", json.dumps(requests[2]["messages"]))

                    expected_users = ["FIRST_RUN", "DRAFT_PRIORITY", "QUEUED_TWO"]
                    mark = len(output)
                    if complete_priority:
                        send("QUEUED_LATER")
                        release.set()
                        wait_until(lambda: len(requests) == 5, "优先任务完成后自动继续原队列和新消息")
                        self.assertEqual([latest_user(request) for request in requests[3:]],
                                         ["QUEUED_ONE", "QUEUED_LATER"])
                        wait_until(lambda: completed_replies() == 3, "优先任务与两条自动续跑完成持久化")
                        wait_until(lambda: b"Done" in output[mark:], "自动续跑回复上屏")
                        expected_users.extend(["QUEUED_ONE", "QUEUED_LATER"])
                    else:
                        os.write(master, b"\x1b")
                        wait_until(lambda: b"paused" in output[mark:], "取消后队列暂停")
                    settle()
                    expected_requests = len(expected_users)
                    self.assertEqual(len(requests), expected_requests,
                                     "完成后继续 FIFO，主动取消则不能自动派发")
                    os.write(master, b"\x04")
                    wait_until(lambda: process.poll() is not None, "退出")
                    self.assertEqual(process.returncode, 0)

                    record = session_record()
                    self.assertIsNotNone(record)
                    users = ["".join(block.get("text", "") for block in message["content"])
                             for message in record["session"]["messages"] if message["role"] == "user"]
                    self.assertEqual(users, expected_users)
                    if complete_priority:
                        self.assertEqual(completed_replies(), 3, "优先任务和两条自动续跑均须保存完整回复")

                    output.clear()
                    os.close(master)
                    os.close(slave)
                    master, slave = pty.openpty()
                    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
                    process = start()
                    wait_until(lambda: FRAME_END in output, "恢复会话")
                    settle()
                    text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(output)).decode("utf-8", "replace")
                    for user in users:
                        self.assertIn(user, text)
                    if not complete_priority:
                        self.assertNotIn("QUEUED_ONE", text)
                    self.assertGreaterEqual(text.count("▌"), len(expected_users), "恢复的提交都应显示用户气泡")
                    self.assertEqual(len(requests), expected_requests, "恢复会话不应偷偷执行队列")
                    os.write(master, b"\x04")
                    wait_until(lambda: process.poll() is not None, "恢复后退出")
                    self.assertEqual(process.returncode, 0)
                finally:
                    if process.poll() is None:
                        process.kill()
                    os.close(master)
                    os.close(slave)
                    process.wait(timeout=10)
        finally:
            release.set()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
