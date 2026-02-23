pub mod config;
pub mod pubkey;
pub mod query;

mod bootstrapper;

pub use bootstrapper::DnsBootstrapper;
pub use config::DnsBootstrapConfig;

use std::fmt;

/// Errors that can occur during DNS bootstrap operations.
#[derive(Debug)]
pub enum DnsBootstrapError {
	/// SRV query failed on all seeds.
	SrvQueryFailed(String),

	/// A/AAAA resolution failed for a node hostname.
	HostResolutionFailed { hostname: String, reason: String },

	/// Bech32 decoding of node ID from hostname failed.
	InvalidBech32NodeId { hostname: String, reason: String },

	/// Decoded bytes are not a valid secp256k1 public key.
	InvalidPublicKey { reason: String },

	/// DNS operation timed out.
	Timeout,

	/// Internal DNS resolver error.
	ResolverError(String),
}

impl fmt::Display for DnsBootstrapError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::SrvQueryFailed(msg) => write!(f, "SRV query failed: {}", msg),
			Self::HostResolutionFailed { hostname, reason } => {
				write!(f, "host resolution failed for {}: {}", hostname, reason)
			},
			Self::InvalidBech32NodeId { hostname, reason } => {
				write!(f, "invalid bech32 node ID in {}: {}", hostname, reason)
			},
			Self::InvalidPublicKey { reason } => {
				write!(f, "invalid public key: {}", reason)
			},
			Self::Timeout => write!(f, "DNS operation timed out"),
			Self::ResolverError(msg) => write!(f, "resolver error: {}", msg),
		}
	}
}

impl std::error::Error for DnsBootstrapError {}
