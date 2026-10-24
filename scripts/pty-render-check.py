#!/usr/bin/env python3
"""用隔离的 demo 会话检查真实 PTY 上的清屏、缩放、粘贴与退出序列。

用法：先 `cargo build -p lucia --locked`，再运行本脚本；也可用 LUCIA_BIN 指定二进制。
仅依赖 Python 标准库，不访问用户配置、插件或真实模型。检查字节协议而非终端像素，
不能替代 Eden 等终端的图形后端验收。
"""

import errno
import fcntl
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
import time
import unittest

REPO = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("LUCIA_BIN", REPO / "target/debug/lucia")).expanduser().resolve()
ENTER_ALT = b"\x1b[?1049h"
LEAVE_ALT = b"\x1b[?1049l"
CLEAR = b"\x1b[2J"
BEGIN_FRAME = b"\x1b[?2026h"
END_FRAME = b"\x1b[?2026l"
PASTE_OFF = b"\x1b[?2004l"


class TerminalProcess:
    """管理一个有控制终端的隔离进程，退出或断言失败时都回收资源。"""

    def __init__(self, root, fullscreen, term):
        self.master, self.slave = pty.openpty()
        self.output = bytearray()
        self.resize(100, 24)
        config = Path(root) / "config.json"
        config.write_text(json.dumps({"tui": {"fullscreen": fullscreen}}), encoding="utf-8")
        env = os.environ.copy()
        env.update(LUCIA_HOME=root, TERM=term, COLORTERM="truecolor")

        def attach_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            self.process = subprocess.Popen(
                [str(BIN), "--demo", "--config", str(config)],
                cwd=root,
                env=env,
                stdin=self.slave,
                stdout=self.slave,
                stderr=self.slave,
                preexec_fn=attach_terminal,
            )
        except BaseException:
            os.close(self.master)
            os.close(self.slave)
            raise

    def resize(self, width, height):
        """修改真实终端网格；运行中的前台进程会收到 SIGWINCH。"""
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    def pump(self, seconds=0.05):
        """收集输出并回应键盘能力查询，避免启动依赖查询超时。"""
        if not select.select([self.master], [], [], seconds)[0]:
            return
        try:
            chunk = os.read(self.master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise
        start = len(self.output)
        self.output.extend(chunk)
        # 匹配跨读取边界的查询，但同一条查询只能应答一次。
        for query, response in [(b"\x1b[?u", b"\x1b[?0u"), (b"\x1b[c", b"\x1b[?1;2c")]:
            at = self.output.find(query, max(0, start - len(query) + 1))
            if at >= 0 and at + len(query) > start:
                os.write(self.master, response)

    def wait_for(self, needle, since=0, timeout=10):
        """只接受本次操作之后的输出；失败时保留输出尾部用于诊断。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.output[since:]:
                return
            self.pump()
            if self.process.poll() is not None:
                break
        if needle in self.output[since:]:
            return
        raise AssertionError(f"未等到 {needle!r}；输出尾部：{bytes(self.output[-1500:])!r}")

    def paste(self, text):
        """按括号粘贴协议写入，正文不会被当作命令键。"""
        os.write(self.master, b"\x1b[200~" + text.encode() + b"\x1b[201~")

    def wait_exit(self, timeout=10):
        """持续读取退出恢复序列，避免 PTY 输出背压阻塞进程退出。"""
        deadline = time.monotonic() + timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        return self.process.wait(timeout=1)

    def close(self):
        """失败时只终止本测试的进程，绝不触碰其他 Lucia 会话。"""
        try:
            if self.process.poll() is None:
                self.process.kill()
            # macOS 的 PTY 对端可能还在排空输出；先关闭描述符，再等待进程回收。
        finally:
            os.close(self.master)
            os.close(self.slave)
            self.process.wait(timeout=10)


class RenderProtocolTests(unittest.TestCase):
    """实际二进制的协议检查；同一脚本适用于默认插件版和纯 Core 版。"""

    def test_startup_resize_paste_switch_and_exit(self):
        for term, fullscreen in [
            ("xterm-256color", True),
            ("xterm-256color", False),
            ("dumb", True),
        ]:
            with (
                self.subTest(term=term, fullscreen=fullscreen),
                tempfile.TemporaryDirectory(prefix="lucia-render-") as root,
            ):
                job = TerminalProcess(root, fullscreen, term)
                try:
                    job.wait_for(END_FRAME)
                    prefix = bytes(job.output).split(BEGIN_FRAME, 1)[0]
                    self.assertIn(CLEAR, prefix, "首次绘制前必须显式清屏")
                    if fullscreen:
                        self.assertLess(prefix.index(ENTER_ALT), prefix.index(CLEAR))
                    else:
                        self.assertNotIn(ENTER_ALT, prefix)

                    # 缩小、极窄、放大均应清屏并完成一帧，不能因旧滚动条几何越界崩溃。
                    for width, height in [(60, 12), (12, 8), (110, 30), (100, 24)]:
                        mark = len(job.output)
                        job.resize(width, height)
                        job.wait_for(CLEAR, mark)
                        clear = job.output.index(CLEAR, mark)
                        job.wait_for(END_FRAME, clear)
                        self.assertIsNone(job.process.poll())

                    mark = len(job.output)
                    pasted = "PTY 中文 e\u0301 🙂 مرحبا END"
                    job.paste(pasted)
                    job.wait_for(b"END", mark)
                    end = job.output.index(b"END", mark)
                    job.wait_for(END_FRAME, end)
                    # 宽字符之后后端会插入绝对光标定位；只检查文本原样输出，不声称验证了字形。
                    text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", job.output[mark:])
                    self.assertIn(pasted.encode(), text)
                    self.assertNotIn(CLEAR, job.output[mark:], "普通输入仍应只输出差分")
                    mark = len(job.output)
                    os.write(job.master, b"\x15")  # Ctrl+U 清掉草稿，不向 demo 提交。
                    job.wait_for(END_FRAME, mark)

                    # 模式切换只写临时配置，进入和离开备用屏幕都要清屏后再绘制。
                    for target in [not fullscreen, fullscreen]:
                        mark = len(job.output)
                        job.paste("/tui fullscreen" if target else "/tui default")
                        os.write(job.master, b"\r")
                        sequence = ENTER_ALT if target else LEAVE_ALT
                        job.wait_for(sequence, mark)
                        job.wait_for(CLEAR, mark)
                        start = job.output.index(sequence, mark)
                        clear = job.output.index(CLEAR, start)
                        job.wait_for(END_FRAME, clear)
                        self.assertLess(start, clear)

                    mark = len(job.output)
                    os.write(job.master, b"\x04")  # 空输入上的 Ctrl+D。
                    job.wait_for(PASTE_OFF, mark)
                    self.assertEqual(job.wait_exit(), 0)
                    job.pump()
                    self.assertIn(LEAVE_ALT, job.output[mark:])
                    for mode in (1000, 1002, 1003, 1015, 1006):
                        self.assertIn(f"\x1b[?{mode}l".encode(), job.output[mark:])
                    self.assertNotIn(b"panicked at", job.output)
                finally:
                    job.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
