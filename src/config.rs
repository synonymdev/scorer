use crate::cli::LdkUserInfo;
use crate::dns_bootstrap::DnsBootstrapConfig;
use bitcoin::network::Network;
use lightning::ln::msgs::SocketAddress;
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug)]
pub enum ConfigError {
	FileNotFound(String),
	ParseError(String),
	ValidationError(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
	pub bitcoind: BitcoindConfig,
	#[serde(default = "default_network")]
	pub network: String,
	#[serde(default)]
	pub ldk: LdkConfig,
	#[serde(default)]
	pub rapid_gossip_sync: RapidGossipSyncConfig,
	pub probing: Option<ProbingConfig>,
	pub dns_bootstrap: Option<DnsBootstrapConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BitcoindConfig {
	pub rpc_host: String,
	pub rpc_port: u16,
	pub rpc_username: String,
	pub rpc_password: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LdkConfig {
	#[serde(default = "default_peer_port")]
	pub peer_listening_port: u16,
	pub announced_node_name: Option<String>,
	#[serde(default)]
	pub announced_listen_addr: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RapidGossipSyncConfig {
	#[serde(default = "default_true")]
	pub enabled: bool,
	pub url: Option<String>,
	#[serde(default = "default_rgs_interval")]
	pub interval_hours: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbingConfig {
	pub interval_sec: u64,
	pub peers: Vec<String>,
	pub amount_msats: Vec<u64>,
	#[serde(default = "default_random_min_probe_amount")]
	pub random_min_amount_msat: u64,
	#[serde(default = "default_random_nodes_per_interval")]
	pub random_nodes_per_interval: u64,
	#[serde(default = "default_probe_timeout")]
	pub timeout_sec: u64,
	#[serde(default = "default_probe_delay")]
	pub probe_delay_sec: u64,
	#[serde(default = "default_peer_delay")]
	pub peer_delay_sec: u64,
}

// Default functions
fn default_network() -> String {
	"testnet".to_string()
}

fn default_peer_port() -> u16 {
	9735
}

fn default_true() -> bool {
	true
}

fn default_rgs_interval() -> u64 {
	6
}

fn default_probe_timeout() -> u64 {
	60
}

fn default_probe_delay() -> u64 {
	1
}

fn default_peer_delay() -> u64 {
	2
}

fn default_random_min_probe_amount() -> u64 {
	0
}

fn default_random_nodes_per_interval() -> u64 {
	1
}

impl Default for RapidGossipSyncConfig {
	fn default() -> Self {
		Self { enabled: true, url: None, interval_hours: 6 }
	}
}

impl NodeConfig {
	pub fn load(ldk_data_dir: &str) -> Result<Self, ConfigError> {
		let config_path = format!("{}/config.toml", ldk_data_dir);
		if !Path::new(&config_path).exists() {
			return Err(ConfigError::FileNotFound(config_path));
		}
		let content = fs::read_to_string(&config_path)
			.map_err(|e| ConfigError::ParseError(format!("Failed to read config: {}", e)))?;
		let config: NodeConfig = toml::from_str(&content)
			.map_err(|e| ConfigError::ParseError(format!("Invalid TOML: {}", e)))?;
		config.validate()?;
		Ok(config)
	}

	fn validate(&self) -> Result<(), ConfigError> {
		// Validate network
		match self.network.as_str() {
			"mainnet" | "testnet" | "regtest" | "signet" => {},
			_ => {
				return Err(ConfigError::ValidationError(format!(
					"Invalid network '{}'. Must be: mainnet, testnet, regtest, signet",
					self.network
				)));
			},
		}

		// Validate announced addresses
		for addr in &self.ldk.announced_listen_addr {
			if SocketAddress::from_str(addr).is_err() {
				return Err(ConfigError::ValidationError(format!(
					"Invalid announced address: {}",
					addr
				)));
			}
		}

		// Validate node name length
		if let Some(name) = &self.ldk.announced_node_name {
			if name.len() > 32 {
				return Err(ConfigError::ValidationError(
					"Node name cannot exceed 32 bytes".to_string(),
				));
			}
		}

		Ok(())
	}

	pub fn get_network(&self) -> Network {
		match self.network.as_str() {
			"mainnet" => Network::Bitcoin,
			"testnet" => Network::Testnet,
			"regtest" => Network::Regtest,
			"signet" => Network::Signet,
			_ => Network::Testnet,
		}
	}

	pub fn get_announced_node_name(&self) -> [u8; 32] {
		let mut bytes = [0u8; 32];
		if let Some(name) = &self.ldk.announced_node_name {
			let name_bytes = name.as_bytes();
			let len = core::cmp::min(name_bytes.len(), 32);
			bytes[..len].copy_from_slice(&name_bytes[..len]);
		}
		bytes
	}

	pub fn get_announced_listen_addr(&self) -> Vec<SocketAddress> {
		self.ldk
			.announced_listen_addr
			.iter()
			.filter_map(|s| SocketAddress::from_str(s).ok())
			.collect()
	}

	pub fn into_ldk_user_info(self, ldk_storage_dir_path: String) -> LdkUserInfo {
		let announced_listen_addr = self.get_announced_listen_addr();
		let announced_node_name = self.get_announced_node_name();
		let network = self.get_network();
		let probing = self.probing.map(|p| crate::cli::ProbingConfig {
			interval_sec: p.interval_sec,
			peers: p.peers,
			amount_msats: p.amount_msats,
			random_min_amount_msat: p.random_min_amount_msat,
			random_nodes_per_interval: p.random_nodes_per_interval,
			timeout_sec: p.timeout_sec,
			probe_delay_sec: p.probe_delay_sec,
			peer_delay_sec: p.peer_delay_sec,
		});
		LdkUserInfo {
			bitcoind_rpc_username: self.bitcoind.rpc_username,
			bitcoind_rpc_password: self.bitcoind.rpc_password,
			bitcoind_rpc_port: self.bitcoind.rpc_port,
			bitcoind_rpc_host: self.bitcoind.rpc_host,
			ldk_storage_dir_path,
			ldk_peer_listening_port: self.ldk.peer_listening_port,
			ldk_announced_listen_addr: announced_listen_addr,
			ldk_announced_node_name: announced_node_name,
			network,
			rapid_gossip_sync_enabled: self.rapid_gossip_sync.enabled,
			rapid_gossip_sync_url: self.rapid_gossip_sync.url,
			rapid_gossip_sync_interval_hours: self.rapid_gossip_sync.interval_hours,
			probing,
			dns_bootstrap: self.dns_bootstrap,
		}
	}
}

pub fn print_config_help() {
	println!("ERROR: Config file not found or invalid.");
	println!();
	println!(
		"Please create a config.toml file in your LDK data directory with the following structure:"
	);
	println!();
	println!(
		r#"[bitcoind]
rpc_host = "127.0.0.1"
rpc_port = 8332
rpc_username = "your_rpc_user"
rpc_password = "your_rpc_password"

network = "testnet"

[ldk]
peer_listening_port = 9735
announced_node_name = "MyNode"
announced_listen_addr = []

[rapid_gossip_sync]
enabled = true
interval_hours = 6

[probing]
interval_sec = 300
peers = []
amount_msats = [1000, 10000, 100000]
random_min_amount_msat = 1000
random_nodes_per_interval = 1
timeout_sec = 60
probe_delay_sec = 1
peer_delay_sec = 2

[dns_bootstrap]
enabled = true
seeds = [ "nodes.lightning.wiki", "lseed.bitcoinstats.com" ]
timeout_secs = 30
num_peers = 10
interval_secs = 300

"#
	);
}
