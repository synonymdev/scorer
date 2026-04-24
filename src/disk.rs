use crate::{InboundPaymentInfoStorage, NetworkGraph, OutboundPaymentInfoStorage};
use bitcoin::Network;
use chrono::{NaiveDate, Utc};
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringDecayParameters};
use lightning::util::hash_tables::new_hash_map;
use lightning::util::logger::{Level, Logger, Record};
use lightning::util::ser::{Readable, ReadableArgs};
use std::fs;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) const INBOUND_PAYMENTS_FNAME: &str = "inbound_payments";
pub(crate) const OUTBOUND_PAYMENTS_FNAME: &str = "outbound_payments";

struct LogWriter {
	file: BufWriter<File>,
	current_date: NaiveDate,
	logs_dir: String,
}

impl LogWriter {
	fn open_log_file(logs_dir: &str, date: NaiveDate) -> std::io::Result<BufWriter<File>> {
		let path = format!("{}/logs-{}.txt", logs_dir, date.format("%Y-%m-%d"));
		let file = fs::OpenOptions::new().create(true).append(true).open(path)?;
		Ok(BufWriter::new(file))
	}
}

pub(crate) struct FilesystemLogger {
	writer: Mutex<LogWriter>,
}
impl FilesystemLogger {
	pub(crate) fn new(data_dir: String) -> Self {
		let logs_dir = format!("{}/logs", data_dir);
		fs::create_dir_all(logs_dir.clone()).unwrap();
		let today = Utc::now().date_naive();
		let file =
			LogWriter::open_log_file(&logs_dir, today).expect("Failed to open initial log file");
		Self { writer: Mutex::new(LogWriter { file, current_date: today, logs_dir }) }
	}
}
impl Logger for FilesystemLogger {
	fn log(&self, record: Record) {
		if record.level == Level::Gossip {
			// Gossip-level logs are incredibly verbose, and thus we skip them by default.
			return;
		}
		let now = Utc::now();
		let raw_log = record.args.to_string();
		let log = format!(
			"{} {:<5} [{}:{}] {}\n",
			// Note that a "real" lightning node almost certainly does *not* want subsecond
			// precision for message-receipt information as it makes log entries a target for
			// deanonymization attacks. For testing, however, its quite useful.
			now.format("%Y-%m-%d %H:%M:%S%.3f"),
			record.level.to_string(),
			record.module_path,
			record.line,
			raw_log
		);
		let Ok(mut writer) = self.writer.lock() else {
			// Mutex poisoned — another thread panicked while holding it.
			// Falling back to stderr is the safest option; never panic in a logger.
			eprintln!("{}", log);
			return;
		};
		// Daily rotation: if the date has changed, open a new log file.
		let today = now.date_naive();
		if today != writer.current_date {
			if let Ok(new_file) = LogWriter::open_log_file(&writer.logs_dir, today) {
				writer.file = new_file;
				writer.current_date = today;
			}
			// If the new file can't be opened, keep writing to the old one.
		}
		let _ = writer.file.write_all(log.as_bytes());
		let _ = writer.file.flush();
	}
}

pub(crate) fn read_network(
	path: &Path, network: Network, logger: Arc<FilesystemLogger>,
) -> NetworkGraph {
	if let Ok(file) = File::open(path) {
		if let Ok(graph) = NetworkGraph::read(&mut BufReader::new(file), logger.clone()) {
			return graph;
		}
		lightning::log_warn!(
			&*logger,
			"Failed to deserialize network graph from {}. Starting with empty graph.",
			path.display()
		);
	} else if path.exists() {
		lightning::log_warn!(
			&*logger,
			"Failed to open network graph at {}. Starting with empty graph.",
			path.display()
		);
	}
	NetworkGraph::new(network, logger)
}

pub(crate) fn read_inbound_payment_info(path: &Path) -> InboundPaymentInfoStorage {
	if let Ok(file) = File::open(path) {
		if let Ok(info) = InboundPaymentInfoStorage::read(&mut BufReader::new(file)) {
			return info;
		}
		eprintln!(
			"WARN: Failed to deserialize inbound payment info from {}. Starting with empty state.",
			path.display()
		);
	} else if path.exists() {
		eprintln!(
			"WARN: Failed to open inbound payment info at {}. Starting with empty state.",
			path.display()
		);
	}
	InboundPaymentInfoStorage { payments: new_hash_map() }
}

pub(crate) fn read_outbound_payment_info(path: &Path) -> OutboundPaymentInfoStorage {
	if let Ok(file) = File::open(path) {
		if let Ok(info) = OutboundPaymentInfoStorage::read(&mut BufReader::new(file)) {
			return info;
		}
		eprintln!(
			"WARN: Failed to deserialize outbound payment info from {}. Starting with empty state.",
			path.display()
		);
	} else if path.exists() {
		eprintln!(
			"WARN: Failed to open outbound payment info at {}. Starting with empty state.",
			path.display()
		);
	}
	OutboundPaymentInfoStorage { payments: new_hash_map() }
}

pub(crate) fn read_scorer(
	path: &Path, graph: Arc<NetworkGraph>, logger: Arc<FilesystemLogger>,
) -> ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>> {
	let params = ProbabilisticScoringDecayParameters::default();
	if let Ok(file) = File::open(path) {
		let args = (params, Arc::clone(&graph), Arc::clone(&logger));
		if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
			return scorer;
		}
		lightning::log_warn!(
			&*logger,
			"Failed to deserialize scorer from {}. Starting with fresh scorer.",
			path.display()
		);
	} else if path.exists() {
		lightning::log_warn!(
			&*logger,
			"Failed to open scorer at {}. Starting with fresh scorer.",
			path.display()
		);
	}
	ProbabilisticScorer::new(params, graph, logger)
}
