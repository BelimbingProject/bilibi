use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub enum LogLevel {
    Warning,
    Error,
}

#[derive(Clone, Copy)]
pub struct LogEvent {
    pub level: LogLevel,
    pub source: &'static str,
    pub template: &'static str,
}

type Sink = Box<dyn Fn(LogEvent) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

pub fn install_log_sink(sink: impl Fn(LogEvent) + Send + Sync + 'static) {
    if SINK.set(Box::new(sink)).is_ok() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let source = info
                .location()
                .and_then(|location| location.file().rsplit(['/', '\\']).next())
                .unwrap_or("panic");
            capture_log(LogEvent {
                level: LogLevel::Error,
                source,
                template: "A server task panicked.",
            });
            previous(info);
        }));
    }
}

#[doc(hidden)]
pub fn capture_log(event: LogEvent) {
    if let Some(sink) = SINK.get() {
        sink(event);
    }
}

#[macro_export]
macro_rules! server_log {
    ($level:ident, $template:literal => $format:literal $($arguments:tt)*) => {{
        ::std::eprintln!($format $($arguments)*);
        $crate::capture_log($crate::LogEvent {
            level: $crate::LogLevel::$level,
            source: ::std::module_path!(),
            template: $template,
        });
    }};
    ($level:ident, $template:literal $($arguments:tt)*) => {
        $crate::server_log!($level, $template => $template $($arguments)*)
    };
}
