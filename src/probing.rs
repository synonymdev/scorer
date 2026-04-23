use crate::cli::ProbingConfig;
use crate::disk::FilesystemLogger;
use crate::hex_utils;
use crate::{ChannelManager, NetworkGraph};
use bitcoin::secp256k1::PublicKey;
use lightning::routing::gossip::NodeId;
use lightning::routing::router::{
	PaymentParameters, RouteParameters, ScorerAccountingForInFlightHtlcs,
};
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::types::payment::PaymentHash;
use lightning::util::logger::Logger;
use rand::{thread_rng, Rng};
use std::collections::HashMap as StdHashMap;
use std::convert::TryInto;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const TIMED_OUT_PROBE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_TIMED_OUT_PROBES: usize = 4096;

#[derive(Clone)]
pub(crate) struct ProbingDeps {
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) network_graph: Arc<NetworkGraph>,
	pub(crate) logger: Arc<FilesystemLogger>,
	pub(crate) scorer: Arc<RwLock<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>>,
	pub(crate) scoring_fee_params: ProbabilisticScoringFeeParameters,
	pub(crate) tracker: Arc<Mutex<ProbeTracker>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProbeOutcome {
	Success,
	Failed,
}

pub(crate) struct ProbeTracker {
	pending_probes: StdHashMap<PaymentHash, tokio::sync::oneshot::Sender<ProbeOutcome>>,
	completed_probes: StdHashMap<PaymentHash, ProbeOutcome>,
	timed_out_probes: StdHashMap<PaymentHash, Instant>,
}

impl ProbeTracker {
	pub(crate) fn new() -> Self {
		Self {
			pending_probes: StdHashMap::new(),
			completed_probes: StdHashMap::new(),
			timed_out_probes: StdHashMap::new(),
		}
	}

	fn register_probe(
		&mut self, payment_hash: PaymentHash,
	) -> tokio::sync::oneshot::Receiver<ProbeOutcome> {
		self.prune_timed_out_probes();
		self.timed_out_probes.remove(&payment_hash);

		if let Some(outcome) = self.completed_probes.remove(&payment_hash) {
			let (tx, rx) = tokio::sync::oneshot::channel();
			let _ = tx.send(outcome);
			return rx;
		}

		let (tx, rx) = tokio::sync::oneshot::channel();
		self.pending_probes.insert(payment_hash, tx);
		rx
	}

	pub(crate) fn complete_success(&mut self, payment_hash: &PaymentHash) {
		self.complete_probe(payment_hash, ProbeOutcome::Success);
	}

	pub(crate) fn complete_failed(&mut self, payment_hash: &PaymentHash) {
		self.complete_probe(payment_hash, ProbeOutcome::Failed);
	}

	pub(crate) fn mark_timed_out(&mut self, payment_hash: &PaymentHash) {
		self.prune_timed_out_probes();
		self.pending_probes.remove(payment_hash);
		self.completed_probes.remove(payment_hash);
		self.timed_out_probes.insert(payment_hash.clone(), Instant::now());
	}

	fn complete_probe(&mut self, payment_hash: &PaymentHash, outcome: ProbeOutcome) {
		self.prune_timed_out_probes();
		if self.timed_out_probes.remove(payment_hash).is_some() {
			return;
		}

		if let Some(sender) = self.pending_probes.remove(payment_hash) {
			let _ = sender.send(outcome);
		} else {
			self.completed_probes.insert(payment_hash.clone(), outcome);
		}
	}

	fn prune_timed_out_probes(&mut self) {
		let now = Instant::now();
		self.timed_out_probes.retain(|_, timed_out_at| {
			now.saturating_duration_since(*timed_out_at) <= TIMED_OUT_PROBE_TTL
		});

		if self.timed_out_probes.len() <= MAX_TIMED_OUT_PROBES {
			return;
		}

		let remove_count = self.timed_out_probes.len() - MAX_TIMED_OUT_PROBES;
		let mut by_age = self
			.timed_out_probes
			.iter()
			.map(|(payment_hash, timed_out_at)| (payment_hash.clone(), *timed_out_at))
			.collect::<Vec<_>>();
		by_age.sort_by_key(|(_, timed_out_at)| *timed_out_at);

		for (payment_hash, _) in by_age.into_iter().take(remove_count) {
			self.timed_out_probes.remove(&payment_hash);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{
		ProbeOutcome, ProbeTracker, MAX_TIMED_OUT_PROBES, TIMED_OUT_PROBE_TTL,
	};
	use lightning::types::payment::PaymentHash;
	use rand::thread_rng;
	use std::collections::HashSet;
	use std::time::{Duration, Instant};
	use tokio::sync::oneshot::error::TryRecvError;

	#[test]
	fn complete_before_register_delivers_buffered_success() {
		let mut tracker = ProbeTracker::new();
		let hash = PaymentHash([1; 32]);

		tracker.complete_success(&hash);

		let mut rx = tracker.register_probe(hash);
		assert!(matches!(rx.try_recv(), Ok(ProbeOutcome::Success)));
	}

	#[test]
	fn complete_before_register_delivers_buffered_failed() {
		let mut tracker = ProbeTracker::new();
		let hash = PaymentHash([2; 32]);

		tracker.complete_failed(&hash);

		let mut rx = tracker.register_probe(hash);
		assert!(matches!(rx.try_recv(), Ok(ProbeOutcome::Failed)));
	}

	#[test]
	fn register_before_complete_still_delivers_over_channel() {
		let mut tracker = ProbeTracker::new();
		let hash = PaymentHash([3; 32]);

		let mut rx = tracker.register_probe(hash);
		assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

		tracker.complete_success(&hash);
		assert!(matches!(rx.try_recv(), Ok(ProbeOutcome::Success)));
	}

	#[test]
	fn late_completion_after_timeout_is_dropped() {
		let mut tracker = ProbeTracker::new();
		let hash = PaymentHash([4; 32]);

		tracker.mark_timed_out(&hash);
		tracker.complete_success(&hash);

		assert!(!tracker.completed_probes.contains_key(&hash));
		assert!(!tracker.timed_out_probes.contains_key(&hash));
	}

	#[test]
	fn prune_removes_entries_older_than_ttl() {
		let mut tracker = ProbeTracker::new();
		let hash = PaymentHash([5; 32]);
		let stale_time = Instant::now() - (TIMED_OUT_PROBE_TTL + Duration::from_secs(1));
		tracker.timed_out_probes.insert(hash, stale_time);

		tracker.prune_timed_out_probes();

		assert!(!tracker.timed_out_probes.contains_key(&hash));
	}

	#[test]
	fn prune_enforces_max_size_cap() {
		let mut tracker = ProbeTracker::new();
		let base = Instant::now();

		for i in 0..(MAX_TIMED_OUT_PROBES + 3) {
			let mut bytes = [0u8; 32];
			bytes[0] = (i % 256) as u8;
			bytes[1] = ((i / 256) % 256) as u8;
			bytes[2] = ((i / 65536) % 256) as u8;
			tracker
				.timed_out_probes
				.insert(PaymentHash(bytes), base + Duration::from_nanos(i as u64));
		}

		tracker.prune_timed_out_probes();

		assert!(tracker.timed_out_probes.len() <= MAX_TIMED_OUT_PROBES);
		assert!(!tracker.timed_out_probes.contains_key(&PaymentHash([0; 32])));
	}

	#[test]
	fn extract_peer_pubkey_from_pubkey_and_addr() {
		assert_eq!(super::extract_peer_pubkey("02abc123@1.2.3.4:9735"), Some("02abc123"));
	}

	#[test]
	fn extract_peer_pubkey_from_raw_pubkey() {
		assert_eq!(super::extract_peer_pubkey("02abc123"), Some("02abc123"));
	}

	#[test]
	fn extract_peer_pubkey_rejects_empty_pubkey() {
		assert_eq!(super::extract_peer_pubkey("@1.2.3.4:9735"), None);
		assert_eq!(super::extract_peer_pubkey("   "), None);
	}

	#[test]
	fn reservoir_sample_is_bounded_and_unique() {
		let mut rng = thread_rng();
		let sample = super::reservoir_sample(0..100usize, 8, &mut rng);

		assert_eq!(sample.len(), 8);
		assert!(sample.iter().all(|value| *value < 100));
		let unique = sample.iter().copied().collect::<HashSet<_>>();
		assert_eq!(unique.len(), sample.len());
	}

	#[test]
	fn reservoir_sample_returns_all_when_k_exceeds_population() {
		let mut rng = thread_rng();
		let sample = super::reservoir_sample(vec![1usize, 2, 3], 10, &mut rng);

		assert_eq!(sample.len(), 3);
		let values = sample.into_iter().collect::<HashSet<_>>();
		assert_eq!(values, HashSet::from([1usize, 2, 3]));
	}

	#[test]
	fn reservoir_sample_returns_empty_when_no_candidates() {
		let mut rng = thread_rng();
		let sample = super::reservoir_sample(Vec::<usize>::new(), 5, &mut rng);

		assert!(sample.is_empty());
	}
}

fn truncate_pubkey(pubkey: &str) -> String {
	if pubkey.len() > 12 {
		format!("{}...", &pubkey[..12])
	} else {
		pubkey.to_string()
	}
}

fn extract_peer_pubkey(peer: &str) -> Option<&str> {
	let pubkey = peer.split('@').next().unwrap_or("").trim();
	if pubkey.is_empty() {
		None
	} else {
		Some(pubkey)
	}
}

fn reservoir_sample<T, I, R>(candidates: I, sample_size: usize, rng: &mut R) -> Vec<T>
where
	I: IntoIterator<Item = T>,
	R: Rng,
{
	if sample_size == 0 {
		return Vec::new();
	}

	let mut reservoir = Vec::with_capacity(sample_size);
	let mut seen = 0usize;

	for candidate in candidates {
		seen += 1;
		if reservoir.len() < sample_size {
			reservoir.push(candidate);
			continue;
		}

		let replace_idx = rng.gen_range(0, seen);
		if replace_idx < sample_size {
			reservoir[replace_idx] = candidate;
		}
	}

	reservoir
}

fn prepare_probe(
	channel_manager: &ChannelManager, graph: &NetworkGraph, logger: &FilesystemLogger,
	scorer: &RwLock<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>,
	scoring_fee_params: &ProbabilisticScoringFeeParameters,
	pub_key_hex: &str, probe_amount: u64,
) -> Option<PaymentHash> {
	if probe_amount == 0 {
		return None;
	}
	let pub_key_bytes = hex_utils::to_vec(pub_key_hex)?;
	if let Ok(pk) = PublicKey::from_slice(&pub_key_bytes) {
		return send_probe(
			channel_manager,
			pk,
			graph,
			logger,
			probe_amount,
			scorer,
			scoring_fee_params,
		);
	}
	None
}

fn send_probe(
	channel_manager: &ChannelManager, recipient: PublicKey, graph: &NetworkGraph,
	logger: &FilesystemLogger, amt_msat: u64,
	scorer: &RwLock<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>,
	scoring_fee_params: &ProbabilisticScoringFeeParameters,
) -> Option<PaymentHash> {
	let chans = channel_manager.list_usable_channels();
	let chan_refs = chans.iter().collect::<Vec<_>>();
	let mut payment_params = PaymentParameters::from_node_id(recipient, 144);
	payment_params.max_path_count = 1;
	let in_flight_htlcs = channel_manager.compute_inflight_htlcs();
	let scorer = scorer.read().unwrap();
	let inflight_scorer = ScorerAccountingForInFlightHtlcs::new(&scorer, &in_flight_htlcs);
	let route_res = lightning::routing::router::find_route(
		&channel_manager.get_our_node_id(),
		&RouteParameters::from_payment_params_and_value(payment_params, amt_msat),
		graph,
		Some(&chan_refs),
		logger,
		&inflight_scorer,
		scoring_fee_params,
		&[32; 32],
	);
	if let Ok(route) = route_res {
		if let Some(path) = route.paths.into_iter().next() {
			if let Ok((payment_hash, _)) = channel_manager.send_probe(path) {
				return Some(payment_hash);
			}
		}
	}
	None
}

enum ProbeWaitResult {
	Success,
	Failed,
	Dropped,
	Timeout,
}

async fn await_probe_result(
	tracker: &Arc<Mutex<ProbeTracker>>, payment_hash: PaymentHash, timeout: Duration,
) -> ProbeWaitResult {
	let receiver = tracker.lock().unwrap().register_probe(payment_hash);
	match tokio::time::timeout(timeout, receiver).await {
		Ok(Ok(ProbeOutcome::Success)) => ProbeWaitResult::Success,
		Ok(Ok(ProbeOutcome::Failed)) => ProbeWaitResult::Failed,
		Ok(Err(_)) => ProbeWaitResult::Dropped,
		Err(_) => {
			tracker.lock().unwrap().mark_timed_out(&payment_hash);
			ProbeWaitResult::Timeout
		},
	}
}

pub(crate) fn spawn_probing_loop(probe_config: ProbingConfig, deps: ProbingDeps) {
	let has_peer_targets = !probe_config.peers.is_empty();
	let has_peer_amounts = !probe_config.amount_msats.is_empty();
	let peer_probing_enabled = has_peer_targets && has_peer_amounts;
	let random_amount_enabled = probe_config.random_min_amount_msat > 0;
	let random_count_enabled = probe_config.random_nodes_per_interval > 0;
	let random_probing_enabled = random_amount_enabled && random_count_enabled;

	if !peer_probing_enabled {
		if !has_peer_targets && has_peer_amounts {
			println!("WARNING: probing.peers is empty in config.toml. Peer-list probing disabled.");
		} else if has_peer_targets && !has_peer_amounts {
			println!(
				"WARNING: probing.amount_msats is empty in config.toml. Peer-list probing disabled."
			);
		}
	}

	if !random_probing_enabled {
		if random_amount_enabled && !random_count_enabled {
			println!(
				"WARNING: probing.random_nodes_per_interval is 0 in config.toml. Random-graph probing disabled."
			);
		}
	}

	if !peer_probing_enabled && !random_probing_enabled {
		println!("WARNING: probing config does not enable any probing mode. Probing disabled.");
		return;
	}

	let mut sorted_amounts = probe_config.amount_msats.clone();
	sorted_amounts.sort();

	let probe_timeout = Duration::from_secs(probe_config.timeout_sec);
	let probe_delay = Duration::from_secs(probe_config.probe_delay_sec);
	let peer_delay = Duration::from_secs(probe_config.peer_delay_sec);

	lightning::log_info!(
		&*deps.logger,
		"Probing started: peer_list_mode={} ({} peers, {} amounts {:?} msat), random_graph_mode={} ({} nodes/interval at {} msat), interval={}s, timeout={}s, probe_delay={}s, peer_delay={}s",
		peer_probing_enabled,
		probe_config.peers.len(),
		sorted_amounts.len(),
		sorted_amounts,
		random_probing_enabled,
		probe_config.random_nodes_per_interval,
		probe_config.random_min_amount_msat,
		probe_config.interval_sec,
		probe_config.timeout_sec,
		probe_config.probe_delay_sec,
		probe_config.peer_delay_sec
	);

	tokio::spawn(async move {
		let ProbingDeps {
			channel_manager,
			network_graph,
			logger,
			scorer,
			scoring_fee_params,
			tracker,
		} = deps;
		let mut interval = tokio::time::interval(Duration::from_secs(probe_config.interval_sec));
		let our_node_id = NodeId::from_pubkey(&channel_manager.get_our_node_id());

		loop {
			interval.tick().await;

			if peer_probing_enabled {
				let peer_count = probe_config.peers.len();
				for (peer_idx, peer) in probe_config.peers.iter().enumerate() {
					let Some(peer_pubkey) = extract_peer_pubkey(peer) else {
						lightning::log_warn!(
							&*logger,
							"Probe skipped: invalid peer entry '{}' (expected pubkey@host:port or pubkey)",
							peer
						);
						continue;
					};
					let peer_short = truncate_pubkey(peer_pubkey);
					'amounts: for &amount in &sorted_amounts {
						let payment_hash = prepare_probe(
							&channel_manager,
							&network_graph,
							&logger,
							&scorer,
							&scoring_fee_params,
							peer_pubkey,
							amount,
						);

						if let Some(hash) = payment_hash {
							lightning::log_info!(
								&*logger,
								"Probe SENT to {} for {} msat (hash: {})",
								peer_short,
								amount,
								hash
							);

							match await_probe_result(&tracker, hash, probe_timeout).await {
								ProbeWaitResult::Success => {
									lightning::log_info!(
										&*logger,
										"probe_completed destination_node={} amount_msat={} state=SUCCESS payment_hash={}",
										peer_pubkey,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
								},
								ProbeWaitResult::Failed => {
									lightning::log_warn!(
										&*logger,
										"probe_completed destination_node={} amount_msat={} state=FAILED payment_hash={}",
										peer_pubkey,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
									break 'amounts;
								},
								ProbeWaitResult::Dropped => {
									lightning::log_warn!(
										&*logger,
										"probe_completed destination_node={} amount_msat={} state=DROPPED payment_hash={}",
										peer_pubkey,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
									break 'amounts;
								},
								ProbeWaitResult::Timeout => {
									lightning::log_warn!(
										&*logger,
										"probe_completed destination_node={} amount_msat={} state=TIMEOUT payment_hash={}",
										peer_pubkey,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
									break 'amounts;
								},
							}
						} else {
							lightning::log_warn!(
								&*logger,
								"probe_completed destination_node={} amount_msat={} state=NO_ROUTE",
								peer_pubkey,
								amount
							);
							tokio::time::sleep(probe_delay).await;
							break 'amounts;
						}
					}

					if peer_idx < peer_count - 1 {
						tokio::time::sleep(peer_delay).await;
					}
				}
			}

			if random_probing_enabled {
				let requested_count =
					probe_config.random_nodes_per_interval.try_into().unwrap_or(usize::MAX);
				let random_recipients = {
					let graph_read_only = network_graph.read_only();
					let mut rng = thread_rng();
					reservoir_sample(
						graph_read_only.nodes().unordered_iter().filter_map(|(node_id, _node_info)| {
							if *node_id == our_node_id {
								return None;
							}
							node_id.as_pubkey().ok()
						}),
						requested_count,
						&mut rng,
					)
				};

				if random_recipients.is_empty() {
					lightning::log_warn!(
						&*logger,
						"Random probe skipped: no graph nodes available"
					);
					continue;
				}

				for recipient in random_recipients {
					let recipient_hex = hex_utils::hex_str(&recipient.serialize());
					let peer_short = truncate_pubkey(&recipient_hex);
					let amount = probe_config.random_min_amount_msat;
					let payment_hash = send_probe(
						&channel_manager,
						recipient,
						&network_graph,
						&logger,
						amount,
						&scorer,
						&scoring_fee_params,
					);

					if let Some(hash) = payment_hash {
						lightning::log_info!(
							&*logger,
							"Random probe SENT to {} for {} msat (hash: {})",
							peer_short,
							amount,
							hash
						);

						match await_probe_result(&tracker, hash, probe_timeout).await {
							ProbeWaitResult::Success => {
								lightning::log_info!(
									&*logger,
									"probe_completed destination_node={} amount_msat={} state=SUCCESS payment_hash={}",
									recipient_hex,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
							ProbeWaitResult::Failed => {
								lightning::log_warn!(
									&*logger,
									"probe_completed destination_node={} amount_msat={} state=FAILED payment_hash={}",
									recipient_hex,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
							ProbeWaitResult::Dropped => {
								lightning::log_warn!(
									&*logger,
									"probe_completed destination_node={} amount_msat={} state=DROPPED payment_hash={}",
									recipient_hex,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
							ProbeWaitResult::Timeout => {
								lightning::log_warn!(
									&*logger,
									"probe_completed destination_node={} amount_msat={} state=TIMEOUT payment_hash={}",
									recipient_hex,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
						}
					} else {
						lightning::log_warn!(
							&*logger,
							"probe_completed destination_node={} amount_msat={} state=NO_ROUTE",
							recipient_hex,
							amount
						);
						tokio::time::sleep(probe_delay).await;
					}
				}
			}
		}
	});
}
