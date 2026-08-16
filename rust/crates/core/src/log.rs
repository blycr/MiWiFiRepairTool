//! 线程安全日志器。行格式与原版 tftp.log 一致：`[dd/MM HH:mm:ss.fff] message`。
//!
//! 级别：Debug < Info < Warn < Error。
//! - 落盘文件（set_file）记录**全部**级别（默认即 debug 级别，方便排查问题）；
//! - 订阅者（UI）默认只收到 Info 及以上，可经 `set_ui_level` 放宽到 Debug。

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// 日志级别。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LogLevel {
    /// 详细调试信息（报文细节、选项协商、重传等），默认仅写文件、不显示在 UI
    Debug = 0,
    /// 常规流程信息
    Info = 1,
    /// 需要注意但不致命的状况
    Warn = 2,
    /// 错误
    Error = 3,
}

impl LogLevel {
    fn tag(self) -> &'static str {
        match self {
            LogLevel::Debug => "DBG",
            LogLevel::Info => "INF",
            LogLevel::Warn => "WRN",
            LogLevel::Error => "ERR",
        }
    }
}

type Callback = Box<dyn Fn(LogLevel, &str) + Send + Sync>;

struct Inner {
    callbacks: Mutex<Vec<Callback>>,
    file: Mutex<Option<File>>,
    /// 订阅者可见的最低级别（默认 Info；UI 勾选"调试日志"后降为 Debug）
    ui_min: AtomicU8,
    /// 是否同时输出到 stderr（终端自刷新面板模式关闭，避免与清屏重绘交错刷屏）
    console_echo: AtomicBool,
}

/// 可克隆的日志器句柄（内部共享）。
#[derive(Clone)]
pub struct Logger {
    inner: Arc<Inner>,
}

impl Default for Logger {
    fn default() -> Self {
        Self::new()
    }
}

impl Logger {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                callbacks: Mutex::new(Vec::new()),
                file: Mutex::new(None),
                ui_min: AtomicU8::new(LogLevel::Info as u8),
                console_echo: AtomicBool::new(true),
            }),
        }
    }

    /// 控制是否同时输出到 stderr（终端自刷新面板模式下关闭，避免与清屏重绘交错）。
    pub fn set_console_echo(&self, on: bool) {
        self.inner.console_echo.store(on, Ordering::SeqCst);
    }

    /// 订阅日志行（UI 用）。回调收到 (级别, 完整行)。仅在级别 >= 当前 UI 级别时触发。
    pub fn subscribe(&self, cb: impl Fn(LogLevel, &str) + Send + Sync + 'static) {
        self.inner.callbacks.lock().unwrap().push(Box::new(cb));
    }

    /// 设置订阅者（UI）可见的最低级别。默认 Info。
    pub fn set_ui_level(&self, level: LogLevel) {
        self.inner.ui_min.store(level as u8, Ordering::SeqCst);
    }

    /// 可选落盘（追加模式，记录全部级别，尽力而为）。用于 tftp.log / debug.log。
    pub fn set_file(&self, path: PathBuf) {
        let f = File::options().create(true).append(true).open(path).ok();
        *self.inner.file.lock().unwrap() = f;
    }

    /// 输出一行（自动加时间戳与级别标签）。
    pub fn emit(&self, level: LogLevel, message: &str) {
        let line = if message.is_empty() {
            format!("[{}]", chrono::Local::now().format("%d/%m %H:%M:%S%.3f"))
        } else {
            format!(
                "[{}] [{}] {}",
                chrono::Local::now().format("%d/%m %H:%M:%S%.3f"),
                level.tag(),
                message
            )
        };
        // 文件：全量
        if let Some(f) = self.inner.file.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{line}");
        }
        // 订阅者：级别过滤
        if (level as u8) >= self.inner.ui_min.load(Ordering::SeqCst) {
            let cbs = self.inner.callbacks.lock().unwrap();
            for cb in cbs.iter() {
                cb(level, &line);
            }
        }
        // 终端自刷新面板模式下关闭 stderr 回显（面板已显示日志）；非 tty 保持
        if self.inner.console_echo.load(Ordering::SeqCst) {
            eprintln!("{line}");
        }
    }

    /// Debug 级别。
    pub fn debug(&self, message: &str) {
        self.emit(LogLevel::Debug, message);
    }

    /// Info 级别（`write` 的别名，语义一致）。
    pub fn info(&self, message: &str) {
        self.emit(LogLevel::Info, message);
    }

    /// Warn 级别。
    pub fn warn(&self, message: &str) {
        self.emit(LogLevel::Warn, message);
    }

    /// Error 级别。
    pub fn error(&self, message: &str) {
        self.emit(LogLevel::Error, message);
    }

    /// 写一行 Info 日志（兼容旧调用点）。
    pub fn write(&self, message: &str) {
        self.emit(LogLevel::Info, message);
    }

    /// 空行（与原版 Blank() 一致）。
    pub fn blank(&self) {
        self.emit(LogLevel::Info, "");
    }
}
