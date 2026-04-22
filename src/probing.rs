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
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct ProbingDeps {
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) network_graph: Arc<NetworkGraph>,
	pub(crate) logger: Arc<FilesystemLogger>,
	pub(crate) scorer: Arc<RwLock<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>>,
	pub(crate) tracker: Arc<Mutex<ProbeTracker>>,
}

#[derive(Clone, Debug)]
enum ProbeOutcome {
	Success,
	Failed,
}

pub(crate) struct ProbeTracker {
	pending_probes: StdHashMap<PaymentHash, tokio::sync::oneshot::Sender<ProbeOutcome>>,
}

impl ProbeTracker {
	pub(crate) fn new() -> Self {
		Self { pending_probes: StdHashMap::new() }
	}

	fn register_probe(
		&mut self, payment_hash: PaymentHash,
	) -> tokio::sync::oneshot::Receiver<ProbeOutcome> {
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

	pub(crate) fn remove_pending(&mut self, payment_hash: &PaymentHash) {
		self.pending_probes.remove(payment_hash);
	}

	fn complete_probe(&mut self, payment_hash: &PaymentHash, outcome: ProbeOutcome) {
		if let Some(sender) = self.pending_probes.remove(payment_hash) {
			let _ = sender.send(outcome);
		}
	}
}

fn truncate_pubkey(pubkey: &str) -> String {
	if pubkey.len() > 12 {
		format!("{}...", &pubkey[..12])
	} else {
		pubkey.to_string()
	}
}

fn prepare_probe(
	channel_manager: &ChannelManager, graph: &NetworkGraph, logger: &FilesystemLogger,
	scorer: &RwLock<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>,
	pub_key_hex: &str, probe_amount: u64,
) -> Option<PaymentHash> {
	if probe_amount == 0 {
		return None;
	}
	let pub_key_bytes = hex_utils::to_vec(pub_key_hex)?;
	if let Ok(pk) = PublicKey::from_slice(&pub_key_bytes) {
		return send_probe(channel_manager, pk, graph, logger, probe_amount, scorer);
	}
	None
}

fn send_probe(
	channel_manager: &ChannelManager, recipient: PublicKey, graph: &NetworkGraph,
	logger: &FilesystemLogger, amt_msat: u64,
	scorer: &RwLock<ProbabilisticScorer<Arc<NetworkGraph>, Arc<FilesystemLogger>>>,
) -> Option<PaymentHash> {
	let chans = channel_manager.list_usable_channels();
	let chan_refs = chans.iter().collect::<Vec<_>>();
	let mut payment_params = PaymentParameters::from_node_id(recipient, 144);
	payment_params.max_path_count = 1;
	let in_flight_htlcs = channel_manager.compute_inflight_htlcs();
	let scorer = scorer.read().unwrap();
	let inflight_scorer = ScorerAccountingForInFlightHtlcs::new(&scorer, &in_flight_htlcs);
	let score_params: ProbabilisticScoringFeeParameters = Default::default();
	let route_res = lightning::routing::router::find_route(
		&channel_manager.get_our_node_id(),
		&RouteParameters::from_payment_params_and_value(payment_params, amt_msat),
		graph,
		Some(&chan_refs),
		logger,
		&inflight_scorer,
		&score_params,
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
			tracker.lock().unwrap().remove_pending(&payment_hash);
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
		let ProbingDeps { channel_manager, network_graph, logger, scorer, tracker } = deps;
		let mut interval = tokio::time::interval(Duration::from_secs(probe_config.interval_sec));
		let our_node_id = NodeId::from_pubkey(&channel_manager.get_our_node_id());

		loop {
			interval.tick().await;

			if peer_probing_enabled {
				let peer_count = probe_config.peers.len();
				for (peer_idx, peer) in probe_config.peers.iter().enumerate() {
					let peer_short = truncate_pubkey(peer);
					'amounts: for &amount in &sorted_amounts {
						let payment_hash = prepare_probe(
							&channel_manager,
							&network_graph,
							&logger,
							&scorer,
							peer,
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
										"Probe SUCCESS to {} for {} msat (hash: {})",
										peer_short,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
								},
								ProbeWaitResult::Failed => {
									lightning::log_warn!(
										&*logger,
										"Probe FAILED to {} for {} msat (hash: {}), skipping remaining amounts",
										peer_short,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
									break 'amounts;
								},
								ProbeWaitResult::Dropped => {
									lightning::log_warn!(
										&*logger,
										"Probe DROPPED to {} for {} msat (hash: {}), channel closed, skipping remaining amounts",
										peer_short,
										amount,
										hash
									);
									tokio::time::sleep(probe_delay).await;
									break 'amounts;
								},
								ProbeWaitResult::Timeout => {
									lightning::log_warn!(
										&*logger,
										"Probe TIMEOUT to {} for {} msat (hash: {}), skipping remaining amounts",
										peer_short,
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
								"Probe NO_ROUTE to {} for {} msat, skipping remaining amounts",
								peer_short,
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
				let mut random_candidates = Vec::new();
				{
					let graph_read_only = network_graph.read_only();
					for (node_id, _node_info) in graph_read_only.nodes().unordered_iter() {
						if *node_id == our_node_id {
							continue;
						}
						if let Ok(pubkey) = node_id.as_pubkey() {
							random_candidates.push(pubkey);
						}
					}
				}

				if random_candidates.is_empty() {
					lightning::log_warn!(
						&*logger,
						"Random probe skipped: no graph nodes available"
					);
					continue;
				}

				let random_recipients = {
					let mut rng = thread_rng();
					rng.shuffle(&mut random_candidates);

					let requested_count =
						probe_config.random_nodes_per_interval.try_into().unwrap_or(usize::MAX);
					let random_probe_count =
						std::cmp::min(requested_count, random_candidates.len());

					random_candidates.into_iter().take(random_probe_count).collect::<Vec<_>>()
				};

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
									"Random probe SUCCESS to {} for {} msat (hash: {})",
									peer_short,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
							ProbeWaitResult::Failed => {
								lightning::log_warn!(
									&*logger,
									"Random probe FAILED to {} for {} msat (hash: {})",
									peer_short,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
							ProbeWaitResult::Dropped => {
								lightning::log_warn!(
									&*logger,
									"Random probe DROPPED to {} for {} msat (hash: {}), channel closed",
									peer_short,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
							ProbeWaitResult::Timeout => {
								lightning::log_warn!(
									&*logger,
									"Random probe TIMEOUT to {} for {} msat (hash: {})",
									peer_short,
									amount,
									hash
								);
								tokio::time::sleep(probe_delay).await;
							},
						}
					} else {
						lightning::log_warn!(
							&*logger,
							"Random probe NO_ROUTE to {} for {} msat",
							peer_short,
							amount
						);
						tokio::time::sleep(probe_delay).await;
					}
				}
			}
		}
	});
}
