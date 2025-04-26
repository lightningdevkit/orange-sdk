use chrono::Utc;
use ldk_node::lightning::util::logger::Logger as LdkLogger;
use ldk_node::lightning::util::logger::{Level, Record};
use ldk_node::logger::{LogRecord, LogWriter};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

pub(crate) struct Logger(Mutex<fs::File>);

impl Logger {
	pub(crate) fn new(path: &Path) -> Result<Logger, ()> {
		Ok(Self(Mutex::new(
			fs::OpenOptions::new().create(true).append(true).open(path).map_err(|_| ())?,
		)))
	}
}

impl LogWriter for Logger {
	fn log(&self, record: LogRecord) {
		if record.level == Level::Gossip {
			return;
		}
		let mut file = self.0.lock().unwrap();
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
	}
}

impl LdkLogger for Logger {
	fn log(&self, record: Record) {
		LogWriter::log(self, record.into());
	}
}
