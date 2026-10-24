#!/usr/bin/env python3
"""在真实 PTY + 真实作业控制下端到端检查 lucia 的挂起/恢复。

为什么需要它：挂起恢复的关键代码在信号处理器里，只写 `write(2)` 与 `tcsetattr(2)`，
单元测试看不到这些字节，也不会真的把进程停住。这里用真 PTY 驱动一份已编译的二进制，
覆盖两条停止路径：

* `Ctrl+Z`：raw mode 关掉了 ISIG，应用自己 `raise(SIGTSTP)` 主动挂起；
* `SIGTTIN`：进程在后台读终端，即 shell 打印的 `suspended (tty input)`。

两者共用同一个处理器，都必须"先交还终端再停住"，恢复后重新接管并整屏重绘。

进程结构（关键）：进程组必须是**非孤儿**的，否则内核直接丢弃 SIGTSTP，检查会假绿。
所以这里自建一个 shell 替身，而不是让 lucia 直接当会话首进程：

    P（本脚本）——持有 master，读屏幕、写按键
      └── S（shell 替身）——setsid + 控制终端 + 把前台进程组交给 L + waitpid(WUNTRACED)
            └── L（lucia）

用法：

    python3 scripts/pty-suspend-check.py                 # 检查 target/debug/lucia
    LUCIA_BIN=~/.cargo/bin/lucia python3 scripts/pty-suspend-check.py
    LUCIA_CONFIG=/path/to/config.json ...                # 默认 ~/.lucia/config.json

退出码：0 全部通过，1 有断言失败，2 环境不成立（启动失败等）。
"""

import errno
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# 用环境变量传参，脚本各自的命令行参数才能留给它们自己。
BIN = os.environ.get("LUCIA_BIN", os.path.join(REPO, "target/debug/lucia"))
CONFIG = os.environ.get("LUCIA_CONFIG", os.path.expanduser("~/.lucia/config.json"))

# 逐个模式判断，不依赖发送顺序：顺序交给 Rust 侧的字节契约测试去锁。
MOUSE_MODES = ("1000", "1002", "1003", "1015", "1006")
DISABLE_MOUSE = tuple(f"\x1b[?{mode}l".encode() for mode in MOUSE_MODES)
ENABLE_MOUSE = tuple(f"\x1b[?{mode}h".encode() for mode in MOUSE_MODES)
LEAVE_ALT = b"\x1b[?1049l"
ENTER_ALT = b"\x1b[?1049h"
SHOW_CURSOR = b"\x1b[?25h"
PASTE_OFF = b"\x1b[?2004l"
PASTE_ON = b"\x1b[?2004h"


class Job:
    """P 侧对 S/L 的视图：屏幕输出 + 作业状态事件。"""

    def __init__(self):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 110, 0, 0))
        events_r, events_w = os.pipe()
        self.master = master
        self.events = events_r
        self.buffer = b""

        shell = os.fork()
        if shell == 0:
            self._be_shell(slave, events_w)
            os._exit(0)
        self.shell = shell
        os.close(slave)
        os.close(events_w)
        fcntl.fcntl(master, fcntl.F_SETFL, os.O_NONBLOCK)
        fcntl.fcntl(events_r, fcntl.F_SETFL, os.O_NONBLOCK)

    @staticmethod
    def _be_shell(slave, events_w):
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        # 真 shell 都忽略 SIGTTOU：它在后台操作终端进程组是正常的。
        signal.signal(signal.SIGTTOU, signal.SIG_IGN)
        child = os.fork()
        if child == 0:
            os.setpgid(0, 0)
            for fd in (0, 1, 2):
                os.dup2(slave, fd)
            os.chdir(REPO)
            os.environ.update(TERM="xterm-256color", COLORTERM="truecolor")
            os.execv(BIN, [BIN, "--config", CONFIG])
            os._exit(127)
        try:
            os.setpgid(child, child)
        except OSError:
            pass
        # 自己还是前台，先交出去；此后它才是前台的"作业"。
        os.tcsetpgrp(slave, child)
        os.write(events_w, f"START {child}\n".encode())
        while True:
            pid, status = os.waitpid(child, os.WUNTRACED)
            if os.WIFSTOPPED(status):
                name = signal.Signals(os.WSTOPSIG(status)).name
                os.write(events_w, f"STOP {name}\n".encode())
            elif os.WIFCONTINUED(status):
                os.write(events_w, b"CONT\n")
            else:
                os.write(events_w, f"EXIT {os.WEXITSTATUS(status)}\n".encode())
                break

    def pump(self, seconds):
        """最多花 seconds 秒把 PTY 输出吸干净。"""
        end = time.time() + max(0.0, seconds)
        while True:
            left = end - time.time()
            if left <= 0:
                return
            r, _, _ = select.select([self.master], [], [], min(0.05, left))
            if not r:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError as exc:
                if exc.errno in (errno.EIO, errno.EAGAIN):
                    return
                raise
            if not chunk:
                return
            self.buffer += chunk

    def next_event(self, timeout):
        """取下一条作业状态事件（START/STOP/CONT/EXIT），超时返回 None。"""
        end = time.time() + timeout
        pending = b""
        while time.time() < end:
            self.pump(0.02)
            try:
                pending += os.read(self.events, 4096)
            except OSError as exc:
                if exc.errno not in (errno.EAGAIN, errno.EWOULDBLOCK):
                    raise
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                return line.decode()
        return None

    def wait_for(self, needle, timeout):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.buffer:
                return True
            self.pump(0.05)
        return needle in self.buffer

    def wait_since(self, mark, needle, timeout):
        """等 mark 之后新出现的 needle：恢复序列每次都会重发，必须按位置判断。"""
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.buffer[mark:]:
                return True
            self.pump(0.05)
        return needle in self.buffer[mark:]

    def drain(self, seconds):
        self.pump(seconds)


def suspend_cycle(job, pid, check, label, trigger, expected):
    """走一遍"停住→交还终端→继续→重新接管"。"""
    mark = len(job.buffer)
    trigger()
    event = job.next_event(10)
    stop_signal = event.split()[1] if event and event.startswith("STOP") else event
    job.drain(1.0)
    handover = job.buffer[mark:]

    check(stop_signal == expected, f"{label}：停止信号是 {expected}（实际 {stop_signal}）")
    check(all(seq in handover for seq in DISABLE_MOUSE), f"{label}：关闭全部鼠标上报模式")
    check(
        not any(seq in handover for seq in ENABLE_MOUSE),
        f"{label}：交还后没有残留的鼠标开启序列",
    )
    check(LEAVE_ALT in handover, f"{label}：退出备用屏幕")
    check(PASTE_OFF in handover, f"{label}：关闭括号粘贴")
    check(handover.rstrip().endswith(SHOW_CURSOR), f"{label}：最后恢复光标")
    tail = handover.split(LEAVE_ALT, 1)[-1]
    check(ENTER_ALT not in tail, f"{label}：交还之后没有再偷偷回到备用屏幕")

    attrs = termios.tcgetattr(job.master)
    lflag = attrs[3]
    check(
        bool(lflag & termios.ICANON and lflag & termios.ECHO and lflag & termios.ISIG),
        f"{label}：termios 已还原为熟模式（lflag=0x{lflag:x}）",
    )

    mark = len(job.buffer)
    os.kill(pid, signal.SIGCONT)
    resumed = job.wait_since(mark, ENTER_ALT, 15)
    job.drain(1.5)
    back = job.buffer[mark:]
    check(resumed, f"{label}：恢复后重新进入备用屏幕")
    check(
        back.startswith(ENTER_ALT),
        f"{label}：先接管屏幕再画帧（交还期间不得往 shell 上画任何东西）",
    )
    check(all(seq in back for seq in ENABLE_MOUSE), f"{label}：恢复后重新开启鼠标捕获")
    check(PASTE_ON in back, f"{label}：恢复后重新开启括号粘贴")


def main():
    if os.name != "posix":
        print("本检查依赖 POSIX 作业控制与 pty，不支持当前平台")
        return 2
    if not os.access(BIN, os.X_OK):
        print(f"找不到可执行文件：{BIN}")
        return 2

    job = Job()
    problems = []

    def check(cond, message):
        print(("  ok   " if cond else "  FAIL ") + message)
        if not cond:
            problems.append(message)

    started = job.next_event(10)
    print(f"binary={BIN}\nconfig={CONFIG}\nshell 事件：{started}")
    if not started or not started.startswith("START"):
        print("!! 作业没有启动")
        return 2
    pid = int(started.split()[1])

    if not job.wait_for(ENTER_ALT, 45):
        print("!! 启动失败，输出尾部：")
        print(job.buffer[-3000:].decode("utf-8", "replace"))
        os.kill(pid, signal.SIGKILL)
        return 2
    job.drain(3.0)
    print(f"启动完成：{len(job.buffer)} 字节输出，pid={pid}\n")

    print("== 场景一：Ctrl+Z（应用主动挂起）")
    suspend_cycle(job, pid, check, "Ctrl+Z", lambda: os.write(job.master, b"\x1a"), "SIGTSTP")

    print("\n== 场景二：SIGTTIN（后台读终端，等价 shell 的 suspended (tty input)）")
    suspend_cycle(job, pid, check, "SIGTTIN", lambda: os.kill(pid, signal.SIGTTIN), "SIGTTIN")

    print("\n== 收尾")
    mark = len(job.buffer)
    os.write(job.master, b"z")
    job.drain(1.5)
    check(b"z" in job.buffer[mark:], "两轮挂起后键盘输入仍然有效")

    # 连续两次 Ctrl+C 才是退出（第一次只清掉待退出状态）。
    try:
        os.write(job.master, b"\x03")
        job.drain(0.5)
        os.write(job.master, b"\x03")
    except OSError:
        # 对端已经关闭：说明进程自己退出了，下面的事件会说明原因。
        pass
    end_event = job.next_event(20)
    check(end_event is not None and end_event.startswith("EXIT"), f"两次 Ctrl+C 正常退出（{end_event}）")
    if end_event is None:
        os.kill(pid, signal.SIGKILL)
    job.drain(0.3)

    print()
    if problems:
        print("失败项：" + "；".join(problems))
        return 1
    print("全部通过")
    return 0


sys.exit(main())
