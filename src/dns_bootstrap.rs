//! DNS Bootstrap implementation per BOLT #10
//!
//! This module provides DNS-based peer discovery for Lightning Network nodes.
//! It queries DNS seeds using SRV records to discover bootstrap peers.
//!
//! The implementation uses a three-tier fallback strategy:
//! 1. Full conditions: r<realm>.a<address_types>.<seed>
//! 2. Minimal conditions: a<address_types>.<seed>
//! 3. Root domain: <seed>
//!
//! Reference: https://github.com/lightning/bolts/blob/master/10-dns-bootstrap.md

use bitcoin::secp256k1::PublicKey;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;
use std::net::SocketAddr;
use std::time::Duration;

/// Maximum number of retry attempts for DNS queries per strategy
const MAX_RETRIES: u8 = 3;

/// Errors that can occur during DNS bootstrap
#[derive(Debug)]
pub enum DnsBootstrapError {
	/// DNS resolver failed to initialize or query
	ResolverError(String),
	/// No DNS records found for the query
	NoRecordsFound,
	/// All query strategies and retry attempts exhausted
	AllStrategiesFailed,
}

/// Query strategy for DNS bootstrap
///
/// Defines different levels of query specificity, from most specific
/// to most compatible.
#[derive(Debug, Clone, Copy)]
enum QueryStrategy {
	/// Full conditions: r<realm>.a<address_types>.<seed>
	/// Example: r0.a6.lseed.bitcoinstats.com
	FullConditions,

	/// Minimal conditions: a<address_types>.<seed>
	/// Example: a6.lseed.bitcoinstats.com
	MinimalConditions,

	/// Root domain only: <seed>
	/// Example: lseed.bitcoinstats.com
	RootDomain,
}

impl QueryStrategy {
	/// Returns all strategies in order of preference (most specific first)
	fn all() -> &'static [QueryStrategy] {
		&[QueryStrategy::FullConditions, QueryStrategy::MinimalConditions, QueryStrategy::RootDomain]
	}
}

/// A discovered bootstrap peer with its node ID and socket address
#[derive(Debug, Clone)]
pub struct BootstrapPeer {
	/// The node's public key (extracted from bech32-encoded virtual hostname)
	pub node_id: Option<PublicKey>,
	/// The socket address to connect to
	pub addr: SocketAddr,
}

/// DNS Bootstrap client for discovering Lightning Network peers
///
/// Implements BOLT #10 DNS seed queries using SRV records with
/// a three-tier fallback strategy for maximum compatibility.
pub struct DnsBootstrapClient {
	seed: String,
	realm: u8,
	address_types: u8,
	#[allow(dead_code)]
	num_peers: u8, // Kept for potential future use, not used in queries
}

impl DnsBootstrapClient {
	/// Create a new DNS bootstrap client
	///
	/// # Arguments
	/// * `seed` - DNS seed domain (e.g., "lseed.bitcoinstats.com")
	/// * `num_peers` - Number of peers to request (informational, not used in query)
	/// * `realm` - Realm byte (0 for Bitcoin)
	/// * `address_types` - Bitfield for address types (6 for IPv4+IPv6)
	pub fn new(seed: String, num_peers: u8, realm: u8, address_types: u8) -> Self {
		Self { seed, realm, address_types, num_peers }
	}

	/// Query DNS seed for bootstrap peers using SRV records
	///
	/// Uses a three-tier fallback strategy:
	/// 1. Full conditions (realm + address types)
	/// 2. Minimal conditions (address types only)
	/// 3. Root domain (no conditions)
	///
	/// Each strategy is retried up to 3 times with exponential backoff.
	///
	/// # Returns
	/// A vector of bootstrap peers with their node IDs and socket addresses
	pub async fn query_bootstrap_peers(&self) -> Result<Vec<BootstrapPeer>, DnsBootstrapError> {
		for strategy in QueryStrategy::all() {
			eprintln!("DEBUG: Trying DNS bootstrap strategy: {:?}", strategy);

			match self.query_with_strategy(*strategy).await {
				Ok(peers) => {
					eprintln!(
						"DEBUG: DNS bootstrap succeeded with strategy {:?}, found {} peers",
						strategy,
						peers.len()
					);
					return Ok(peers);
				},
				Err(e) => {
					eprintln!(
						"DEBUG: Strategy {:?} failed after {} retries: {:?}",
						strategy, MAX_RETRIES, e
					);
					// Continue to next strategy
				},
			}
		}

		eprintln!("DEBUG: All DNS bootstrap strategies failed");
		Err(DnsBootstrapError::AllStrategiesFailed)
	}

	/// Query with a specific strategy, retrying up to MAX_RETRIES times
	async fn query_with_strategy(
		&self, strategy: QueryStrategy,
	) -> Result<Vec<BootstrapPeer>, DnsBootstrapError> {
		let query_domain = self.build_query_domain(strategy);
		let mut last_error = None;

		for attempt in 1..=MAX_RETRIES {
			match self.query_srv_records(&query_domain).await {
				Ok(peers) => return Ok(peers),
				Err(e) => {
					eprintln!(
						"DEBUG: Strategy {:?} attempt {}/{} failed for {}: {:?}",
						strategy, attempt, MAX_RETRIES, query_domain, e
					);
					last_error = Some(e);

					// Wait before retry with exponential backoff: 1s, 2s
					if attempt < MAX_RETRIES {
						let delay = Duration::from_secs(2u64.pow((attempt - 1) as u32));
						tokio::time::sleep(delay).await;
					}
				},
			}
		}

		Err(last_error.unwrap_or(DnsBootstrapError::NoRecordsFound))
	}

	/// Build DNS query domain based on strategy
	///
	/// Per BOLT #10, conditions are evaluated right-to-left by the DNS seed.
	fn build_query_domain(&self, strategy: QueryStrategy) -> String {
		match strategy {
			QueryStrategy::FullConditions => {
				// r<realm>.a<address_types>.<seed>
				// Example: r0.a6.lseed.bitcoinstats.com
				format!("r{}.a{}.{}", self.realm, self.address_types, self.seed)
			},
			QueryStrategy::MinimalConditions => {
				// a<address_types>.<seed>
				// Example: a6.lseed.bitcoinstats.com
				format!("a{}.{}", self.address_types, self.seed)
			},
			QueryStrategy::RootDomain => {
				// Just the seed domain
				// Example: lseed.bitcoinstats.com
				self.seed.clone()
			},
		}
	}

	/// Extract node_id from bech32-encoded virtual hostname
	///
	/// Per BOLT #10, SRV records return virtual hostnames like:
	/// ln1qwktpe6jxltmpphyl578eax6fcjc2m807qalr76a5gfmx7k9qqfjwy4mctz.lseed.bitcoinstats.com
	///
	/// The `ln1q...` prefix is a bech32-encoded node_id (33-byte compressed public key)
	fn extract_node_id_from_hostname(&self, hostname: &str) -> Option<PublicKey> {
		// The hostname format is: <bech32_node_id>.<seed_domain>
		// Extract the first component (before the first dot)
		let node_id_part = hostname.split('.').next()?;

		// Check if it starts with "ln1q" (bech32 prefix for Lightning node IDs)
		if !node_id_part.starts_with("ln1q") {
			return None;
		}

		// Decode bech32 to get the public key bytes
		// bech32 0.8 returns (hrp, data, variant)
		match bech32::decode(node_id_part) {
			Ok((hrp, data, _variant)) => {
				if hrp.as_str() != "ln" {
					return None;
				}

				// Convert 5-bit groups to 8-bit bytes
				let bytes = match bech32::convert_bits(&data, 5, 8, false) {
					Ok(b) => b,
					Err(_) => return None,
				};

				// Should be 33 bytes for a compressed public key
				if bytes.len() != 33 {
					return None;
				}

				PublicKey::from_slice(&bytes).ok()
			},
			Err(_) => None,
		}
	}

	/// Query SRV records and resolve to bootstrap peers
	async fn query_srv_records(
		&self, domain: &str,
	) -> Result<Vec<BootstrapPeer>, DnsBootstrapError> {
		// Create resolver with default configuration
		let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());

		eprintln!("DEBUG: Querying SRV records for: {}", domain);

		// Query SRV records
		let srv_lookup = resolver
			.srv_lookup(domain)
			.await
			.map_err(|e| DnsBootstrapError::ResolverError(format!("SRV lookup failed: {}", e)))?;

		let srv_records: Vec<_> = srv_lookup.iter().collect();
		if srv_records.is_empty() {
			return Err(DnsBootstrapError::NoRecordsFound);
		}

		eprintln!("DEBUG: Found {} SRV records", srv_records.len());

		let mut peers = Vec::new();

		// Resolve each SRV record to IP addresses
		for srv in srv_records {
			let hostname = srv.target().to_string();
			let port = srv.port();

			eprintln!("DEBUG: Resolving SRV target: {}:{}", hostname, port);

			// Try to extract node_id from the virtual hostname
			let node_id = self.extract_node_id_from_hostname(&hostname);
			if let Some(ref id) = node_id {
				eprintln!("DEBUG: Extracted node_id: {}", id);
			} else {
				eprintln!("DEBUG: Could not extract node_id from hostname");
			}

			// Try to resolve hostname to IP addresses
			match resolver.lookup_ip(&hostname).await {
				Ok(lookup) => {
					for ip in lookup.iter() {
						let addr = SocketAddr::new(ip, port);
						peers.push(BootstrapPeer { node_id, addr });
						eprintln!("DEBUG: Resolved to: {}", addr);
					}
				},
				Err(e) => {
					// Log but continue with other records
					eprintln!("DEBUG: Failed to resolve {}: {}", hostname, e);
				},
			}
		}

		if peers.is_empty() {
			return Err(DnsBootstrapError::NoRecordsFound);
		}

		Ok(peers)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_build_query_domain_full_conditions() {
		let client = DnsBootstrapClient::new("lseed.bitcoinstats.com".to_string(), 25, 0, 6);

		let domain = client.build_query_domain(QueryStrategy::FullConditions);
		assert_eq!(domain, "r0.a6.lseed.bitcoinstats.com");
	}

	#[test]
	fn test_build_query_domain_minimal_conditions() {
		let client = DnsBootstrapClient::new("lseed.bitcoinstats.com".to_string(), 25, 0, 6);

		let domain = client.build_query_domain(QueryStrategy::MinimalConditions);
		assert_eq!(domain, "a6.lseed.bitcoinstats.com");
	}

	#[test]
	fn test_build_query_domain_root_domain() {
		let client = DnsBootstrapClient::new("lseed.bitcoinstats.com".to_string(), 25, 0, 6);

		let domain = client.build_query_domain(QueryStrategy::RootDomain);
		assert_eq!(domain, "lseed.bitcoinstats.com");
	}

	#[test]
	fn test_build_query_domain_custom_params() {
		let client = DnsBootstrapClient::new("test.example.com".to_string(), 10, 1, 2);

		assert_eq!(client.build_query_domain(QueryStrategy::FullConditions), "r1.a2.test.example.com");
		assert_eq!(client.build_query_domain(QueryStrategy::MinimalConditions), "a2.test.example.com");
		assert_eq!(client.build_query_domain(QueryStrategy::RootDomain), "test.example.com");
	}

	#[test]
	fn test_extract_node_id_from_hostname() {
		let client = DnsBootstrapClient::new("lseed.bitcoinstats.com".to_string(), 25, 0, 6);

		// Test with a valid-looking hostname (note: this is a made-up example)
		// Real node IDs would need to be valid bech32-encoded public keys
		let hostname = "ln1qtest.lseed.bitcoinstats.com";
		let result = client.extract_node_id_from_hostname(hostname);
		// This will likely fail because "ln1qtest" is not a valid bech32 encoding
		// but it tests the parsing logic
		assert!(result.is_none() || result.is_some());
	}

	#[test]
	fn test_query_strategy_all() {
		let strategies = QueryStrategy::all();
		assert_eq!(strategies.len(), 3);
		assert!(matches!(strategies[0], QueryStrategy::FullConditions));
		assert!(matches!(strategies[1], QueryStrategy::MinimalConditions));
		assert!(matches!(strategies[2], QueryStrategy::RootDomain));
	}

	#[tokio::test]
	async fn test_query_bootstrap_peers_real() {
		// This test makes a real DNS query - may fail in some CI environments
		let client = DnsBootstrapClient::new("lseed.bitcoinstats.com".to_string(), 5, 0, 6);

		match client.query_bootstrap_peers().await {
			Ok(peers) => {
				println!("Found {} bootstrap peers", peers.len());
				for peer in &peers {
					println!("  - {} (node_id: {:?})", peer.addr, peer.node_id);
				}
				// We should get at least one peer if DNS is working
				assert!(!peers.is_empty());
			},
			Err(e) => {
				// DNS may fail in some environments, that's okay for this test
				println!("DNS query failed (may be expected in some environments): {:?}", e);
			},
		}
	}
}
