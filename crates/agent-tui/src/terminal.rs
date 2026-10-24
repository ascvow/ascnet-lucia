//! 终端接管与作业控制的协作。
//!
//! raw mode 关闭了终端的信号生成（ISIG），Ctrl+Z 不再变成 `SIGTSTP`；外部 `kill -TSTP`
//! 与后台读终端触发的 `SIGTTIN` 仍然会直接停住进程。这两条路径都必须**先恢复终端再停止**，
//! 否则鼠标捕获、备用屏幕与 raw mode 会原样留给外层 shell：指针移动被当作普通输入回显，
//! 屏幕上出现成片的 `ESC [ < … M` 垃圾。
//!
//! 信号处理器里只能调用 async-signal-safe 的接口，因此这里保存一份启动时的终端属性快照，
//! 用 `write(2)` 与 `tcsetattr(2)` 完成恢复；启用一侧仍走 crossterm，两侧的字节序列由
//! 本模块的测试逐字节对齐。恢复后的重新接管需要备用屏幕与鼠标等完整能力，放在主循环里
//! 用安全上下文完成，处理器只登记"该恢复了"。

#[cfg(unix)]
pub(crate) use unix::{
    install, is_detached, mark_attached, release, set_keyboard_enhanced, set_mouse_capture,
    set_screen, suspend, take_resume_request,
};

#[cfg(not(unix))]
pub(crate) use unsupported::{
    install, is_detached, mark_attached, release, set_keyboard_enhanced, set_mouse_capture,
    set_screen, suspend, take_resume_request,
};

#[cfg(unix)]
mod unix {
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    /// 是否占用终端备用屏幕；`/tui` 可以在运行期切换，处理器要按当前值决定是否退出。
    static FULLSCREEN: AtomicBool = AtomicBool::new(false);
    /// 终端是否支持键盘增强协议；支持时挂起前必须弹出对应标记。
    static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
    /// 鼠标捕获是否开启；关闭后不能再发禁用序列，否则会打乱用户自己开的终端选择。
    static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(false);
    /// 终端当前是否由本进程接管；恢复动作以此作为幂等闸门。
    static ATTACHED: AtomicBool = AtomicBool::new(false);
    /// 处理器已经接管终端、主循环需要重新启用并整屏重绘。
    static RESUME_REQUESTED: AtomicBool = AtomicBool::new(false);
    /// 进程以后台作业继续运行，暂时不能碰终端。
    static DETACHED: AtomicBool = AtomicBool::new(false);
    /// 进入 raw mode 之前的终端属性；处理器里只能用裸 `tcsetattr` 还原。
    static ORIGINAL_TERMIOS: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

    /// 弹出键盘增强标记，对应 crossterm 的 `PopKeyboardEnhancementFlags`。
    const POP_KEYBOARD_ENHANCEMENT: &[u8] = b"\x1b[<1u";
    /// 关闭全部鼠标上报模式，对应 crossterm 的 `DisableMouseCapture`（逆序发送）。
    const DISABLE_MOUSE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
    /// 关闭括号粘贴，对应 crossterm 的 `DisableBracketedPaste`。
    const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
    /// 退出备用屏幕，对应 crossterm 的 `LeaveAlternateScreen`。
    const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
    /// 交还终端前恢复光标，对应 crossterm 的 `Show`。
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

    /// 记录原始终端属性并安装 `SIGTSTP`/`SIGTTIN`/`SIGCONT` 处理器。
    ///
    /// 必须在 `enable_raw_mode` **之前**调用：只有那时才读得到"原样"的终端属性，而挂起
    /// 恢复不能依赖 crossterm 的全局状态。标准输入不是终端时没有作业控制可言，直接跳过。
    pub(crate) fn install(fullscreen: bool) {
        FULLSCREEN.store(fullscreen, Ordering::SeqCst);
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `original` 是有效的 termios 存储，`tcgetattr` 只写入它。
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return;
        }
        // 泄漏一份快照交给处理器读取：它的生命周期与进程相同，无需释放。
        ORIGINAL_TERMIOS.store(Box::into_raw(Box::new(original)), Ordering::SeqCst);
        // 安装失败只意味着失去作业控制能力，不应阻断 TUI 启动，因此不返回错误。
        // SAFETY: 处理器是 `extern "C" fn(c_int)`，与 `signal(2)` 要求的签名一致。
        unsafe {
            libc::signal(libc::SIGTSTP, handler_address(on_stop_signal));
            libc::signal(libc::SIGTTIN, handler_address(on_stop_signal));
            libc::signal(libc::SIGCONT, handler_address(on_continue_signal));
        }
    }

    /// 把 `extern "C"` 处理函数转成 `signal(2)` 需要的地址形式。
    ///
    /// 直接转成整数会触发 `function_casts_as_integer`：先退化成裸指针再转整数，
    /// 语义与 C 里的函数指针转换一致，但类型转换是显式的。
    fn handler_address(handler: extern "C" fn(libc::c_int)) -> libc::sighandler_t {
        handler as *const () as libc::sighandler_t
    }

    /// 同步备用屏幕状态，供处理器决定挂起时是否需要退出备用屏幕。
    pub(crate) fn set_screen(fullscreen: bool) {
        FULLSCREEN.store(fullscreen, Ordering::SeqCst);
    }

    /// 同步键盘增强协议支持状态。
    pub(crate) fn set_keyboard_enhanced(enabled: bool) {
        KEYBOARD_ENHANCED.store(enabled, Ordering::SeqCst);
    }

    /// 同步鼠标捕获状态。
    pub(crate) fn set_mouse_capture(enabled: bool) {
        MOUSE_CAPTURE.store(enabled, Ordering::SeqCst);
    }

    /// 标记终端已接管，此后挂起必须由处理器恢复。
    pub(crate) fn mark_attached() {
        ATTACHED.store(true, Ordering::SeqCst);
        RESUME_REQUESTED.store(false, Ordering::SeqCst);
        DETACHED.store(false, Ordering::SeqCst);
    }

    /// 退出时交还终端：之后的信号不再需要（也不能）再写恢复序列。
    pub(crate) fn release() {
        ATTACHED.store(false, Ordering::SeqCst);
        RESUME_REQUESTED.store(false, Ordering::SeqCst);
        DETACHED.store(true, Ordering::SeqCst);
    }

    /// 主动挂起：`SIGTSTP` 由处理器先恢复终端再按默认动作停住进程，返回即代表已被继续。
    pub(crate) fn suspend() {
        // SAFETY: `raise` 只向本进程投递信号。
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
    }

    /// 取走"需要重新接管终端"的请求。
    pub(crate) fn take_resume_request() -> bool {
        RESUME_REQUESTED.swap(false, Ordering::SeqCst)
    }

    /// 进程是否留在后台、暂时不能绘制。
    pub(crate) fn is_detached() -> bool {
        DETACHED.load(Ordering::SeqCst)
    }

    /// 挂起信号处理器：先恢复终端，再按默认动作真正停住进程。
    ///
    /// 处理器运行期间该信号默认被阻塞，必须先复位处置方式并解阻塞，`raise` 才能生效。
    /// 若安装处理器失败，进程会按默认动作停止，终端状态保持原样——与未实现作业控制前一致。
    extern "C" fn on_stop_signal(signal: libc::c_int) {
        restore_terminal();
        // SAFETY: 只调用 async-signal-safe 的接口，且 addr 均为本模块的有效静态数据。
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            // 这里会停住整个进程；返回说明已经收到 SIGCONT。
            libc::raise(signal);
            libc::signal(signal, handler_address(on_stop_signal));
        }
        if is_foreground() {
            RESUME_REQUESTED.store(true, Ordering::SeqCst);
        } else {
            // 用 `bg` 继续的后台作业不能抢占终端，等下一次 SIGCONT 再尝试接管。
            DETACHED.store(true, Ordering::SeqCst);
        }
    }

    /// `SIGCONT` 处理器：后台作业回到前台时补登记恢复请求。
    extern "C" fn on_continue_signal(_signal: libc::c_int) {
        if DETACHED.load(Ordering::SeqCst) && is_foreground() {
            DETACHED.store(false, Ordering::SeqCst);
            RESUME_REQUESTED.store(true, Ordering::SeqCst);
        }
    }

    /// 挂起前的终端恢复：必须幂等，且只使用 async-signal-safe 的接口。
    ///
    /// 顺序与启用的逆序一致：先退回键盘增强与鼠标上报，再离开备用屏幕，最后交还终端属性。
    fn restore_terminal() {
        if !ATTACHED.swap(false, Ordering::SeqCst) {
            return;
        }
        if KEYBOARD_ENHANCED.load(Ordering::SeqCst) {
            write_raw(POP_KEYBOARD_ENHANCEMENT);
        }
        if MOUSE_CAPTURE.load(Ordering::SeqCst) {
            write_raw(DISABLE_MOUSE);
        }
        write_raw(DISABLE_BRACKETED_PASTE);
        if FULLSCREEN.load(Ordering::SeqCst) {
            write_raw(LEAVE_ALTERNATE_SCREEN);
        }
        let original = ORIGINAL_TERMIOS.load(Ordering::SeqCst);
        if !original.is_null() {
            // SAFETY: 指针由 `install` 泄漏保存，进程退出前一直有效。
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
            }
        }
        // TUI 绘制时隐藏了光标，shell 拿回终端后需要自己的光标。
        write_raw(SHOW_CURSOR);
    }

    /// 直写标准输出，绕过 Rust 缓冲：处理器里不能分配、也不能重试到死。
    ///
    /// 短写（例如被信号打断）视为放弃，后续字符由主循环恢复时的整屏重绘兜底。
    fn write_raw(bytes: &'static [u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            // SAFETY: `rest` 指向静态字节切片，长度来自同一片切片。
            let written =
                unsafe { libc::write(libc::STDOUT_FILENO, rest.as_ptr().cast(), rest.len() as _) };
            if written <= 0 {
                return;
            }
            rest = &rest[written as usize..];
        }
    }

    /// 当前进程组是否仍持有终端前台。
    ///
    /// `tcgetpgrp` 失败（标准输入不是终端）时按前台处理：无法判定时宁可直接恢复界面，
    /// 也不能把 TUI 永久留在"后台"状态。
    fn is_foreground() -> bool {
        // SAFETY: 两个调用都只读取进程与终端状态。
        let foreground = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        foreground < 0 || foreground == unsafe { libc::getpgrp() }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            DISABLE_BRACKETED_PASTE, DISABLE_MOUSE, LEAVE_ALTERNATE_SCREEN,
            POP_KEYBOARD_ENHANCEMENT, SHOW_CURSOR,
        };
        use crossterm::cursor::Show;
        use crossterm::event::{
            DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
        };
        use crossterm::queue;
        use crossterm::terminal::LeaveAlternateScreen;

        /// 把 crossterm 命令编码成原始字节，和处理器里的字面量逐字节比较。
        fn encode(command: impl crossterm::Command) -> Vec<u8> {
            let mut buffer = Vec::new();
            queue!(buffer, command).expect("网络无关的内存缓冲不会失败");
            buffer
        }

        /// 处理器里手写的序列必须与 crossterm 的启用/停用一一对应，否则挂起后终端会残留状态。
        #[test]
        fn escape_sequences_match_crossterm() {
            assert_eq!(
                encode(PopKeyboardEnhancementFlags),
                POP_KEYBOARD_ENHANCEMENT
            );
            assert_eq!(encode(DisableMouseCapture), DISABLE_MOUSE);
            assert_eq!(encode(DisableBracketedPaste), DISABLE_BRACKETED_PASTE);
            assert_eq!(encode(LeaveAlternateScreen), LEAVE_ALTERNATE_SCREEN);
            assert_eq!(encode(Show), SHOW_CURSOR);
        }
    }
}

/// 非 Unix 平台没有 POSIX 作业控制信号，全部降级为空实现。
#[cfg(not(unix))]
mod unsupported {
    /// 不安装任何处理器。
    pub(crate) fn install(_fullscreen: bool) {}

    /// 无备用屏幕状态需要同步。
    pub(crate) fn set_screen(_fullscreen: bool) {}

    /// 无键盘增强弹栈动作需要同步。
    pub(crate) fn set_keyboard_enhanced(_enabled: bool) {}

    /// 无鼠标捕获状态需要同步。
    pub(crate) fn set_mouse_capture(_enabled: bool) {}

    /// 无接管动作需要登记。
    pub(crate) fn mark_attached() {}

    /// 无接管动作需要撤销。
    pub(crate) fn release() {}

    /// 无作业控制，挂起退化为空操作。
    pub(crate) fn suspend() {}

    /// 不会产生恢复请求。
    pub(crate) fn take_resume_request() -> bool {
        false
    }

    /// 不会进入后台挂起状态。
    pub(crate) fn is_detached() -> bool {
        false
    }
}
