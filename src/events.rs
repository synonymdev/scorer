use crate::bitcoind_client::BitcoindClient;
use crate::cli;
use crate::disk::{INBOUND_PAYMENTS_FNAME, OUTBOUND_PAYMENTS_FNAME};
use crate::probing::ProbeTracker;
use crate::{
	BumpTxEventHandler, ChannelManager, HTLCStatus, InboundPaymentInfoStorage, MillisatAmount,
	NetworkGraph, OutboundPaymentInfoStorage, PaymentInfo, PeerManager,
};
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::network::Network;
use bitcoin_bech32::WitnessProgram;
use lightning::events::{Event, PaymentFailureReason, PaymentPurpose};
use lightning::ln::types::ChannelId;
use lightning::routing::gossip::NodeId;
use lightning::sign::EntropySource;
use lightning::util::hash_tables::hash_map::Entry;
use lightning::util::persist::KVStore;
use lightning::util::ser::Writeable;
use lightning_persister::fs_store::FilesystemStore;
use std::collections::HashMap as StdHashMap;
use std::io::Write;
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct EventContext {
	pub(crate) channel_manager: Arc<ChannelManager>,
	pub(crate) bitcoind_client: Arc<BitcoindClient>,
	pub(crate) network_graph: Arc<NetworkGraph>,
	pub(crate) keys_manager: Arc<crate::KeysManager>,
	pub(crate) bump_tx_event_handler: Arc<BumpTxEventHandler>,
	pub(crate) peer_manager: Arc<PeerManager>,
	pub(crate) inbound_payments: Arc<Mutex<InboundPaymentInfoStorage>>,
	pub(crate) outbound_payments: Arc<Mutex<OutboundPaymentInfoStorage>>,
	pub(crate) fs_store: Arc<FilesystemStore>,
	pub(crate) output_sweeper: Arc<crate::OutputSweeper>,
	pub(crate) network: Network,
	pub(crate) probe_tracker: Arc<Mutex<ProbeTracker>>,
}

pub(crate) async fn handle_ldk_events(ctx: EventContext, event: Event) {
	let EventContext {
		channel_manager,
		bitcoind_client,
		network_graph,
		keys_manager,
		bump_tx_event_handler,
		peer_manager,
		inbound_payments,
		outbound_payments,
		fs_store,
		output_sweeper,
		network,
		probe_tracker,
	} = ctx;

	match event {
		Event::FundingGenerationReady {
			temporary_channel_id,
			counterparty_node_id,
			channel_value_satoshis,
			output_script,
			..
		} => {
			let addr = WitnessProgram::from_scriptpubkey(
				output_script.as_bytes(),
				match network {
					Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
					Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
					Network::Signet => bitcoin_bech32::constants::Network::Signet,
					Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
					_ => bitcoin_bech32::constants::Network::Testnet,
				},
			)
			.expect("Lightning funding tx should always be to a SegWit output")
			.to_address();
			let mut outputs = vec![StdHashMap::new()];
			outputs[0].insert(addr, channel_value_satoshis as f64 / 100_000_000.0);
			let raw_tx = bitcoind_client.create_raw_transaction(outputs).await;
			let funded_tx = bitcoind_client.fund_raw_transaction(raw_tx).await;
			let signed_tx = bitcoind_client.sign_raw_transaction_with_wallet(funded_tx.hex).await;
			assert!(signed_tx.complete);
			let final_tx: Transaction =
				encode::deserialize(&crate::hex_utils::to_vec(&signed_tx.hex).unwrap()).unwrap();
			if channel_manager
				.funding_transaction_generated(temporary_channel_id, counterparty_node_id, final_tx)
				.is_err()
			{
				println!(
					"\nERROR: Channel went away before we could fund it. The peer disconnected or refused the channel."
				);
				print!("> ");
				std::io::stdout().flush().unwrap();
			}
		},
		Event::FundingTxBroadcastSafe { .. } => {},
		Event::PaymentClaimable { payment_hash, purpose, amount_msat, .. } => {
			println!(
				"\nEVENT: received payment from payment hash {} of {} millisatoshis",
				payment_hash, amount_msat,
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
			let payment_preimage = match purpose {
				PaymentPurpose::Bolt11InvoicePayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::Bolt12OfferPayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::Bolt12RefundPayment { payment_preimage, .. } => payment_preimage,
				PaymentPurpose::SpontaneousPayment(preimage) => Some(preimage),
			};
			channel_manager.claim_funds(payment_preimage.unwrap());
		},
		Event::PaymentClaimed { payment_hash, purpose, amount_msat, .. } => {
			println!(
				"\nEVENT: claimed payment from payment hash {} of {} millisatoshis",
				payment_hash, amount_msat,
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::Bolt11InvoicePayment {
					payment_preimage, payment_secret, ..
				} => (payment_preimage, Some(payment_secret)),
				PaymentPurpose::Bolt12OfferPayment { payment_preimage, payment_secret, .. } => {
					(payment_preimage, Some(payment_secret))
				},
				PaymentPurpose::Bolt12RefundPayment {
					payment_preimage, payment_secret, ..
				} => (payment_preimage, Some(payment_secret)),
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
			};
			let write_future = {
				let mut inbound = inbound_payments.lock().unwrap();
				match inbound.payments.entry(payment_hash) {
					Entry::Occupied(mut e) => {
						let payment = e.get_mut();
						payment.status = HTLCStatus::Succeeded;
						payment.preimage = payment_preimage;
						payment.secret = payment_secret;
					},
					Entry::Vacant(e) => {
						e.insert(PaymentInfo {
							preimage: payment_preimage,
							secret: payment_secret,
							status: HTLCStatus::Succeeded,
							amt_msat: MillisatAmount(Some(amount_msat)),
						});
					},
				}
				fs_store.write("", "", INBOUND_PAYMENTS_FNAME, inbound.encode())
			};
			write_future.await.unwrap();
		},
		Event::PaymentSent {
			payment_preimage, payment_hash, fee_paid_msat, payment_id, ..
		} => {
			let write_future = {
				let mut outbound = outbound_payments.lock().unwrap();
				for (id, payment) in outbound.payments.iter_mut() {
					if *id == payment_id.unwrap() {
						payment.preimage = Some(payment_preimage);
						payment.status = HTLCStatus::Succeeded;
						println!(
							"\nEVENT: successfully sent payment of {} millisatoshis{} from \
									 payment hash {} with preimage {}",
							payment.amt_msat,
							if let Some(fee) = fee_paid_msat {
								format!(" (fee {} msat)", fee)
							} else {
								"".to_string()
							},
							payment_hash,
							payment_preimage
						);
						print!("> ");
						std::io::stdout().flush().unwrap();
					}
				}
				fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound.encode())
			};
			write_future.await.unwrap();
		},
		Event::OpenChannelRequest {
			ref temporary_channel_id, ref counterparty_node_id, ..
		} => {
			let mut random_bytes = [0u8; 16];
			random_bytes.copy_from_slice(&keys_manager.get_secure_random_bytes()[..16]);
			let user_channel_id = u128::from_be_bytes(random_bytes);
			let res = channel_manager.accept_inbound_channel(
				temporary_channel_id,
				counterparty_node_id,
				user_channel_id,
				None,
			);

			if let Err(e) = res {
				print!(
					"\nEVENT: Failed to accept inbound channel ({}) from {}: {:?}",
					temporary_channel_id,
					crate::hex_utils::hex_str(&counterparty_node_id.serialize()),
					e,
				);
			} else {
				print!(
					"\nEVENT: Accepted inbound channel ({}) from {}",
					temporary_channel_id,
					crate::hex_utils::hex_str(&counterparty_node_id.serialize()),
				);
			}
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::PaymentPathSuccessful { .. } => {},
		Event::PaymentPathFailed { .. } => {},
		Event::ProbeSuccessful { payment_hash, .. } => {
			probe_tracker.lock().unwrap().complete_success(&payment_hash);
		},
		Event::ProbeFailed { payment_hash, .. } => {
			probe_tracker.lock().unwrap().complete_failed(&payment_hash);
		},
		Event::PaymentFailed { payment_hash, reason, payment_id, .. } => {
			if let Some(hash) = payment_hash {
				print!(
					"\nEVENT: Failed to send payment to payment ID {}, payment hash {}: {:?}",
					payment_id,
					hash,
					if let Some(r) = reason { r } else { PaymentFailureReason::RetriesExhausted }
				);
			} else {
				print!(
					"\nEVENT: Failed fetch invoice for payment ID {}: {:?}",
					payment_id,
					if let Some(r) = reason { r } else { PaymentFailureReason::RetriesExhausted }
				);
			}
			print!("> ");
			std::io::stdout().flush().unwrap();

			let write_future = {
				let mut outbound = outbound_payments.lock().unwrap();
				if outbound.payments.contains_key(&payment_id) {
					let payment = outbound.payments.get_mut(&payment_id).unwrap();
					payment.status = HTLCStatus::Failed;
				}
				fs_store.write("", "", OUTBOUND_PAYMENTS_FNAME, outbound.encode())
			};
			write_future.await.unwrap();
		},
		Event::InvoiceReceived { .. } => {},
		Event::PaymentForwarded {
			prev_channel_id,
			next_channel_id,
			total_fee_earned_msat,
			claim_from_onchain_tx,
			outbound_amount_forwarded_msat,
			..
		} => {
			let read_only_network_graph = network_graph.read_only();
			let nodes = read_only_network_graph.nodes();
			let channels = channel_manager.list_channels();

			let node_str = |channel_id: &Option<ChannelId>| match channel_id {
				None => String::new(),
				Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id) {
					None => String::new(),
					Some(channel) => {
						match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
							None => "private node".to_string(),
							Some(node) => match &node.announcement_info {
								None => "unnamed node".to_string(),
								Some(announcement) => {
									format!("node {}", announcement.alias())
								},
							},
						}
					},
				},
			};
			let channel_str = |channel_id: &Option<ChannelId>| {
				channel_id
					.map(|channel_id| format!(" with channel {}", channel_id))
					.unwrap_or_default()
			};
			let from_prev_str =
				format!(" from {}{}", node_str(&prev_channel_id), channel_str(&prev_channel_id),);
			let to_next_str =
				format!(" to {}{}", node_str(&next_channel_id), channel_str(&next_channel_id));

			let from_onchain_str = if claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};
			let amt_args = if let Some(v) = outbound_amount_forwarded_msat {
				format!("{}", v)
			} else {
				"?".to_string()
			};
			if let Some(fee_earned) = total_fee_earned_msat {
				println!(
					"\nEVENT: Forwarded payment for {} msat{}{}, earning {} msat {}",
					amt_args, from_prev_str, to_next_str, fee_earned, from_onchain_str
				);
			} else {
				println!(
					"\nEVENT: Forwarded payment for {} msat{}{}, claiming onchain {}",
					amt_args, from_prev_str, to_next_str, from_onchain_str
				);
			}
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::HTLCHandlingFailed { .. } => {},
		Event::SpendableOutputs { outputs, channel_id } => {
			output_sweeper.track_spendable_outputs(outputs, channel_id, false, None).await.unwrap();
		},
		Event::ChannelPending { channel_id, counterparty_node_id, .. } => {
			println!(
				"\nEVENT: Channel {} with peer {} is pending awaiting funding lock-in!",
				channel_id,
				crate::hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::ChannelReady { ref channel_id, ref counterparty_node_id, .. } => {
			println!(
				"\nEVENT: Channel {} with peer {} is ready to be used!",
				channel_id,
				crate::hex_utils::hex_str(&counterparty_node_id.serialize()),
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::ChannelClosed { channel_id, reason, counterparty_node_id, .. } => {
			println!(
				"\nEVENT: Channel {} with counterparty {} closed due to: {:?}",
				channel_id,
				counterparty_node_id.map(|id| format!("{}", id)).unwrap_or("".to_owned()),
				reason
			);
			print!("> ");
			std::io::stdout().flush().unwrap();
		},
		Event::DiscardFunding { .. } => {},
		Event::HTLCIntercepted { .. } => {},
		Event::OnionMessageIntercepted { .. } => {},
		Event::OnionMessagePeerConnected { .. } => {},
		Event::BumpTransaction(event) => bump_tx_event_handler.handle_event(&event).await,
		Event::ConnectionNeeded { node_id, addresses } => {
			tokio::spawn(async move {
				for address in addresses {
					if let Ok(sockaddrs) = address.to_socket_addrs() {
						for addr in sockaddrs {
							let pm = Arc::clone(&peer_manager);
							if cli::connect_peer_if_necessary(node_id, addr, pm).await.is_ok() {
								return;
							}
						}
					}
				}
			});
		},
		_ => {},
	}
}
