# ldk-sample
Sample node implementation using LDK.

## Installation
```
git clone https://github.com/lightningdevkit/ldk-sample
```

## Usage
```
cd ldk-sample
cargo run <ldk_storage_directory_path>
```
The only CLI argument is the storage directory path. All configuration is read from
`<ldk_storage_directory_path>/.ldk/config.toml`.

Use `config.example.toml` in the repo root as a template for your config file.

## Configuration

Config is loaded from `<storage_dir>/.ldk/config.toml` and strictly validated. Unknown
fields cause an error (`deny_unknown_fields`).

Required sections: `bitcoind`.
Optional sections: `ldk`, `rapid_gossip_sync`, and `probing` (probing is disabled if
missing).

Network options: `mainnet`, `testnet`, `regtest`, `signet` (default is `testnet`).

### Example config

```toml
network = "testnet"

[bitcoind]
rpc_host = "127.0.0.1"
rpc_port = 8332
rpc_username = "your_rpc_user"
rpc_password = "your_rpc_password"

# [ldk]
# peer_listening_port = 9735
# announced_node_name = "MyLDKNode"
# announced_listen_addr = []

[rapid_gossip_sync]
enabled = true
url = "https://rapidsync.lightningdevkit.org/snapshot/"
interval_hours = 6

[probing]
interval_sec = 300
peers = ["02abc123...@1.2.3.4:9735"]
amount_msats = [1000, 10000, 100000, 1000000]
timeout_sec = 60

probe_delay_sec = 1
peer_delay_sec = 2
```

### Key options

`bitcoind`:
RPC details are required. `rpc_username` and `rpc_password` can usually be found
via `cat ~/.bitcoin/.cookie`.

`ldk`:
`peer_listening_port` defaults to 9735. `announced_listen_addr` and
`announced_node_name` default to empty, disabling public announcements.
`announced_listen_addr` can be set to an IPv4 or IPv6 address to announce a
publicly-connectable address. `announced_node_name` can be any string up to 32
bytes, representing this node's alias.

`rapid_gossip_sync`:
Enabled by default to speed up initial network graph sync. On startup, the node
attempts a RapidGossipSync download up to 3 times with exponential backoff. If it
fails, the node falls back to P2P gossip sync. `url` defaults to
`https://rapidsync.lightningdevkit.org/snapshot/`, `interval_hours` defaults to 6.

`probing`:
Optional. If omitted, probing is disabled. Configure probe interval, peers, probe
amounts in millisatoshis, and timeout.

## License

Licensed under either:

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
