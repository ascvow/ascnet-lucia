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

    def __init__(self, root, fullscreen, term, color_env=None):
        self.master, self.slave = pty.openpty()
        self.output = bytearray()
        self.resize(100, 24)
        config = Path(root) / "config.json"
        config.write_text(json.dumps({"tui": {"fullscreen": fullscreen}}), encoding="utf-8")
        env = os.environ.copy()
        env.update(LUCIA_HOME=root, TERM=term, COLORTERM="truecolor")
        env.pop("NO_COLOR", None)
        if color_env is not None:
            env.update(color_env)

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

    def wait_for_text(self, text, since=0, timeout=10):
        """在已完成的帧中查文字，允许逐键差分在字符之间插入光标定位或样式序列。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            complete = self.output.rfind(END_FRAME, since)
            if complete >= since:
                rendered = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", self.output[since:complete])
                if text.encode() in rendered:
                    return
            self.pump()
            if self.process.poll() is not None:
                break
        raise AssertionError(f"未等到完整帧文字 {text!r}；输出尾部：{bytes(self.output[-1500:])!r}")

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

    def test_git_branch_refreshes_without_input_in_repository_and_worktree(self):
        """真正从外部 Git 切分支，空闲 PTY 底栏必须自动变更；HEAD 文件与目录均覆盖。"""
        for worktree in (False, True):
            with self.subTest(worktree=worktree), tempfile.TemporaryDirectory(prefix="lucia-git-pty-") as root:
                root = Path(root)
                repo = root / "repo"
                repo.mkdir()
                git_env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
                git_env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)

                def git(*args, cwd=repo):
                    return subprocess.run(["git", *args], cwd=cwd, env=git_env, check=True,
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.decode().strip()

                # 使用等长且逐列不同的标记，避免差分渲染省略未变化字符造成字节断言误报。
                git("init", "--initial-branch=AAAAAAAA")
                git("-c", "user.name=Lucia Test", "-c", "user.email=test@example.invalid",
                    "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "fixture")
                cwd = repo
                if worktree:
                    cwd = root / "worktree"
                    git("worktree", "add", "-b", "WWWWWWWW", str(cwd))
                initial = "WWWWWWWW" if worktree else "AAAAAAAA"
                job = TerminalProcess(str(cwd), True, "xterm-256color")
                try:
                    job.wait_for_text(initial)
                    mark = len(job.output)
                    git("switch", "-c", "ZZZZZZZZ", cwd=cwd)
                    job.wait_for_text("ZZZZZZZZ", mark)
                    mark = len(job.output)
                    git("switch", "--detach", cwd=cwd)
                    short_hash = git("rev-parse", "HEAD", cwd=cwd)[:8]
                    job.wait_for_text(short_hash, mark)
                    mark = len(job.output)
                    git("switch", initial, cwd=cwd)
                    job.wait_for_text(initial, mark)
                    self.assertIsNone(job.process.poll())
                    os.write(job.master, b"\x04")
                    self.assertEqual(job.wait_exit(), 0)
                finally:
                    job.close()

    def test_burst_typing_preserves_ime_text_edits_and_paths(self):
        """一次 PTY 写入模拟 IME 提交/积压按键，不得伪造粘贴、折叠或丢掉后续编辑。"""
        with tempfile.TemporaryDirectory(prefix="lucia-input-") as root:
            path = Path(root) / "中文输入.txt"
            path.write_text("only attach on paste or submit", encoding="utf-8")
            job = TerminalProcess(root, True, "xterm-256color")
            try:
                job.wait_for(END_FRAME)
                for index, text in enumerate([
                    "中文输入法整词上屏",
                    "FastEnglishTyping",
                    "日本語 한글 e\u0301 🙂 مرحبا",
                    "连续输入" * 17,  # 超过粘贴折叠宽度，普通打字仍须保留原文。
                    str(path),  # 已存在的路径也不能仅因输入快就被替换成附件。
                ]):
                    with self.subTest(text=text):
                        mark = len(job.output)
                        os.write(job.master, (text + "x").encode() + b"\x7fEND")
                        job.wait_for_text("END", mark)
                        # 强制完整重绘后只检查这一帧，不能把曾显示过但又丢失的文字当作通过。
                        frame = len(job.output)
                        job.resize(180 + index, 30)
                        job.wait_for(CLEAR, frame)
                        clear = job.output.index(CLEAR, frame)
                        job.wait_for(END_FRAME, clear)
                        frame_end = job.output.index(END_FRAME, clear) + len(END_FRAME)
                        rendered = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", job.output[clear:frame_end]).decode("utf-8")
                        self.assertIn("".join((text + "END").split()), "".join(rendered.split()))
                        output_text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", job.output[mark:]).decode("utf-8")
                        for unexpected in ["正在准备附件", "[粘贴的文本", "[FILE#", "附件仍在准备"]:
                            self.assertNotIn(unexpected, output_text)
                        mark = len(job.output)
                        os.write(job.master, b"\x15")
                        job.wait_for(END_FRAME, mark)

                # 快速输入斜杠命令后立即回车仍须走原生命令路由。
                mark = len(job.output)
                os.write(job.master, b"/tui default\r")
                job.wait_for(LEAVE_ALT, mark)
                job.wait_for(END_FRAME, job.output.index(LEAVE_ALT, mark))
                command_output = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", job.output[mark:]).decode("utf-8")
                self.assertNotIn("正在准备附件", command_output)

                # 真正的括号粘贴仍识别文件路径，不把兼容修复变成关闭粘贴能力。
                mark = len(job.output)
                job.paste(str(path))
                job.wait_for(b"[FILE#1]", mark)
                job.wait_for(END_FRAME, job.output.index(b"[FILE#1]", mark))
                mark = len(job.output)
                os.write(job.master, b"\x15")
                job.wait_for(END_FRAME, mark)
                os.write(job.master, b"\x04")
                self.assertEqual(job.wait_exit(), 0)
            finally:
                job.close()

    def test_burst_typing_followed_by_enter_submits_exact_text(self):
        """快速输入、退格、续打和回车在同一批到达时，持久化消息必须完整且顺序不变。"""
        with tempfile.TemporaryDirectory(prefix="lucia-input-submit-") as root:
            job = TerminalProcess(root, True, "xterm-256color")
            try:
                job.wait_for(END_FRAME)
                expected = "中文整词上屏FastEnglish继续输入"
                os.write(job.master, "中文整词上屏FastEnglishx".encode() + b"\x7f" + "继续输入\r".encode())
                deadline = time.monotonic() + 10
                users = []
                while time.monotonic() < deadline:
                    job.pump()
                    users = []
                    for path in Path(root).rglob("*.json"):
                        record = json.loads(path.read_text(encoding="utf-8"))
                        for message in record.get("session", {}).get("messages", []):
                            if message["role"] == "user":
                                users.append("".join(block.get("text", "") for block in message["content"]))
                    if users:
                        break
                self.assertEqual(users, [expected])
                # 即使 demo 仍在运行，空草稿上的 Ctrl+D 也应恢复终端并退出。
                os.write(job.master, b"\x04")
                self.assertEqual(job.wait_exit(), 0)
            finally:
                job.close()

    def test_terminal_palette_and_monochrome_output(self):
        """检查真实后端输出的颜色协议；不把色板声明测试冒充图形主题截图验收。"""
        for term, color_env, monochrome in [
            ("xterm", {"COLORTERM": ""}, False),
            ("xterm-256color", {"NO_COLOR": "1"}, True),
            ("dumb", {}, True),
        ]:
            with (
                self.subTest(term=term, color_env=color_env),
                tempfile.TemporaryDirectory(prefix="lucia-colors-") as root,
            ):
                job = TerminalProcess(root, True, term, color_env)
                try:
                    job.wait_for(END_FRAME)
                    # 菜单选择必须保留非颜色提示，单色也能看见当前候选。
                    mark = len(job.output)
                    job.paste("/")
                    job.wait_for(b"/context", mark)
                    menu = job.output.index(b"/context", mark)
                    job.wait_for(END_FRAME, menu)
                    output = bytes(job.output)
                    sgr = re.findall(rb"\x1b\[([0-9;:]*)m", output)
                    self.assertNotRegex(output, rb"\x1b\[[0-9;:]*(?:38|48|58);2;")
                    self.assertTrue(any(b"7" in code.split(b";") for code in sgr), "菜单应保留反色选中态")
                    has_palette_color = any(
                        value in {b"38", b"48", b"58"}
                        or (value.isdigit() and (30 <= int(value) <= 37 or 40 <= int(value) <= 47 or 90 <= int(value) <= 107))
                        for code in sgr for value in code.split(b";")
                    )
                    if monochrome:
                        self.assertFalse(has_palette_color, "单色模式不应发送显式颜色")
                    else:
                        self.assertTrue(has_palette_color, "基础终端应使用可配置的 ANSI 色板")
                        self.assertNotIn(b"\x1b]10;?", output)
                        self.assertNotIn(b"\x1b]11;?", output)
                    mark = len(job.output)
                    os.write(job.master, b"\x15")
                    job.wait_for(END_FRAME, mark)
                    os.write(job.master, b"\x04")
                    self.assertEqual(job.wait_exit(), 0)
                finally:
                    job.close()

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
                    # 差分可能跳过上一帧已存在的空格；宽字符后也会插入绝对光标定位。
                    # 此处验证非空白文本/字素原样输出，空格几何由 TestBackend 渲染测试覆盖。
                    text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", job.output[mark:]).decode("utf-8")
                    self.assertIn("".join(pasted.split()), "".join(text.split()))
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
