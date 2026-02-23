use serde::Deserialize;

/// Configuration for DNS-based peer bootstrapping (BOLT-0010).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsBootstrapConfig {
	/// Whether DNS bootstrap is enabled.
	#[serde(default = "default_enabled")]
	pub enabled: bool,

	/// DNS seed servers to query.
	#[serde(default = "default_seeds")]
	pub seeds: Vec<String>,

	/// Timeout for individual DNS queries in seconds.
	#[serde(default = "default_timeout_secs")]
	pub timeout_secs: u64,

	/// Target number of peers to discover per bootstrap round.
	#[serde(default = "default_num_peers")]
	pub num_peers: usize,

	/// Interval between bootstrap attempts in seconds.
	#[serde(default = "default_interval_secs")]
	pub interval_secs: u64,
}

fn default_enabled() -> bool {
	true
}

fn default_seeds() -> Vec<String> {
	vec!["nodes.lightning.wiki".to_string(), "lseed.bitcoinstats.com".to_string()]
}

fn default_timeout_secs() -> u64 {
	30
}

fn default_num_peers() -> usize {
	10
}

fn default_interval_secs() -> u64 {
	300
}

impl Default for DnsBootstrapConfig {
	fn default() -> Self {
		Self {
			enabled: default_enabled(),
			seeds: default_seeds(),
			timeout_secs: default_timeout_secs(),
			num_peers: default_num_peers(),
			interval_secs: default_interval_secs(),
		}
	}
}
