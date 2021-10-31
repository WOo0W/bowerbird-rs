use colored::{ColoredString, Colorize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 1,
    Info,
    Warn,
    Error,
    Line,
}

pub fn should_log(set_level: LogLevel, log_level: LogLevel) -> bool {
    set_level <= log_level
}

pub static TARGET_LOG_LEVEL: LogLevel = LogLevel::Debug;

#[macro_export]
macro_rules! debug {
	($($arg:tt)*) => {
		if $crate::log::should_log($crate::log::TARGET_LOG_LEVEL, $crate::log::LogLevel::Debug) {
			use colored::Colorize;
			println!("\r{} [{}] {}",
			$crate::log::gray_datetime(),
			"DEBUG".bright_blue(),
			format!($($arg)*).bright_black());
		}
	};
}

#[macro_export]
macro_rules! info {
	($($arg:tt)*) => {
		if $crate::log::should_log($crate::log::TARGET_LOG_LEVEL, $crate::log::LogLevel::Info) {
			use colored::Colorize;
			println!("\r{} [{}] {}",
			$crate::log::gray_datetime(),
			"INFO".bright_green(),
			format!($($arg)*));
		}
	};
}

#[macro_export]
macro_rules! warn {
	($($arg:tt)*) => {
		if $crate::log::should_log($crate::log::TARGET_LOG_LEVEL, $crate::log::LogLevel::Warn) {
			use colored::Colorize;
			println!("\r{} [{}] {}",
			$crate::log::gray_datetime(),
			"WARN".bright_yellow(),
			format!($($arg)*).bright_yellow());
		}
	};
}

#[macro_export]
macro_rules! error {
	($($arg:tt)*) => {
		if $crate::log::should_log($crate::log::TARGET_LOG_LEVEL, $crate::log::LogLevel::Debug) {
			use colored::Colorize;
			println!("\r{} [{}] {}",
			$crate::log::gray_datetime(),
			"ERROR".bright_red(),
			format!($($arg)*).bright_red());
		}
	};
}

pub fn gray_datetime() -> ColoredString {
    chrono::Local::now()
        .format("%m-%d %T")
        .to_string()
        .bright_black()
}
