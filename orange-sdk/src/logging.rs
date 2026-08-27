use chrono::Utc;
use ldk_node::lightning::util::logger::Logger as LdkLogger;
use ldk_node::lightning::util::logger::{Level, Record};
use ldk_node::logger::{LogLevel, LogRecord, LogWriter};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

/// The type of logger to use.
#[derive(Debug, Clone)]
pub enum LoggerType {
	/// A logger that uses the `log` crate facade.
	/// This allows integration with existing logging infrastructure.
	LogFacade,
	/// A logger that writes to a specified file.
	/// The file will be created if it does not exist and its parent directories will be created as needed.
	File {
		/// The path to the log file.
		path: PathBuf,
	},
}

#[derive(Debug)]
pub(crate) enum Logger {
	Facade,
	File(Mutex<fs::File>),
}

impl Logger {
	pub(crate) fn new(logger_type: &LoggerType) -> Result<Self, ()> {
		match logger_type {
			LoggerType::LogFacade => Ok(Self::Facade),
			LoggerType::File { path } => {
				if path.parent().is_some_and(|p| !p.exists()) {
					fs::create_dir_all(path.parent().unwrap()).map_err(|_| ())?;
				}
				Ok(Self::File(Mutex::new(
					fs::OpenOptions::new().create(true).append(true).open(path).map_err(|_| ())?,
				)))
			},
		}
	}
}

impl LogWriter for Logger {
	fn log(&self, record: LogRecord) {
		if record.level == Level::Gossip {
			return;
		}

		match self {
			Logger::Facade => {
				let level = match record.level {
					LogLevel::Gossip | LogLevel::Trace => log::Level::Trace,
					LogLevel::Debug => log::Level::Debug,
					LogLevel::Info => log::Level::Info,
					LogLevel::Warn => log::Level::Warn,
					LogLevel::Error => log::Level::Error,
				};
				if level.to_level_filter() > log::max_level() {
					return;
				}

				let mut builder = log::Record::builder();
				builder.level(level);

				log::logger().log(
					&builder
						.module_path(Some(record.module_path))
						.line(Some(record.line))
						.args(format_args!("{}", record.args))
						.build(),
				);
			},
			Logger::File(fs) => {
				let mut file = fs.lock().unwrap();
				let mut buffer = BufWriter::new(&mut *file);
				let _ = writeln!(
					&mut buffer,
					"{} {:<5} [{}:{}] {}",
					Utc::now().format("%Y-%m-%d %H:%M:%S"),
					record.level.to_string(),
					record.module_path,
					record.line,
					record.args
				);
			},
		}
	}
}

impl LdkLogger for Logger {
	fn log(&self, record: Record) {
		LogWriter::log(self, record.into());
	}
}
