use super::DnsBootstrapError;
use bitcoin::secp256k1::PublicKey;
use lightning::routing::gossip::NodeId;

/// Decode a Lightning node public key from a bech32-encoded DNS virtual hostname.
///
/// Hostname format: `<bech32(pubkey)>.<seed_root>.`
/// Example: `ln1qwktpe6jxltmpphyl578eax6fcjc2m807qalr76a5gfmx7k9qqfjwy4mctz.nodes.lightning.wiki.`
///
/// The first DNS label is a bech32 string with HRP "ln" encoding the 33-byte
/// compressed secp256k1 public key of the node.
pub fn decode_pubkey_from_hostname(
	hostname: &str,
) -> Result<(PublicKey, NodeId), DnsBootstrapError> {
	// Split on '.' and take the first label (the bech32-encoded pubkey).
	let bech32_label =
		hostname.split('.').next().ok_or_else(|| DnsBootstrapError::InvalidBech32NodeId {
			hostname: hostname.to_string(),
			reason: "empty hostname".to_string(),
		})?;

	if bech32_label.is_empty() {
		return Err(DnsBootstrapError::InvalidBech32NodeId {
			hostname: hostname.to_string(),
			reason: "empty first label".to_string(),
		});
	}

	// Decode bech32. Returns (hrp, data_as_5bit_words, variant).
	let (hrp, data, _variant) =
		bech32::decode(bech32_label).map_err(|e| DnsBootstrapError::InvalidBech32NodeId {
			hostname: hostname.to_string(),
			reason: format!("bech32 decode failed: {}", e),
		})?;

	// Verify HRP is "ln" (Lightning Network).
	if hrp != "ln" {
		return Err(DnsBootstrapError::InvalidBech32NodeId {
			hostname: hostname.to_string(),
			reason: format!("expected HRP 'ln', got '{}'", hrp),
		});
	}

	// Convert 5-bit words to 8-bit bytes.
	let pubkey_bytes = bech32::convert_bits(&data, 5, 8, false).map_err(|e| {
		DnsBootstrapError::InvalidBech32NodeId {
			hostname: hostname.to_string(),
			reason: format!("bit conversion failed: {}", e),
		}
	})?;

	// Parse as a compressed secp256k1 public key (33 bytes).
	let pubkey = PublicKey::from_slice(&pubkey_bytes).map_err(|e| {
		DnsBootstrapError::InvalidPublicKey { reason: format!("secp256k1 parse failed: {}", e) }
	})?;

	let node_id = NodeId::from_pubkey(&pubkey);
	Ok((pubkey, node_id))
}

#[cfg(test)]
mod tests {
	use super::*;
	use bech32::ToBase32;

	/// Helper: encode raw bytes as bech32 with given HRP.
	fn encode_bech32(hrp: &str, data: &[u8]) -> String {
		bech32::encode(hrp, data.to_base32(), bech32::Variant::Bech32).unwrap()
	}

	fn hex_to_bytes(hex: &str) -> Vec<u8> {
		(0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
	}

	#[test]
	fn test_decode_valid_hostname() {
		let pubkey_hex = "02e89ca9e8da72b33d896bae51d20e7e1f0571cece852d1387b4e2f4ae48c80a85";
		let pubkey_bytes = hex_to_bytes(pubkey_hex);
		let pubkey = PublicKey::from_slice(&pubkey_bytes).unwrap();

		let encoded = encode_bech32("ln", &pubkey_bytes);
		let hostname = format!("{}.nodes.lightning.wiki.", encoded);

		let (decoded_pubkey, node_id) = decode_pubkey_from_hostname(&hostname).unwrap();
		assert_eq!(decoded_pubkey, pubkey);
		assert_eq!(node_id, NodeId::from_pubkey(&pubkey));
	}

	#[test]
	fn test_decode_invalid_hrp() {
		let pubkey_hex = "02e89ca9e8da72b33d896bae51d20e7e1f0571cece852d1387b4e2f4ae48c80a85";
		let pubkey_bytes = hex_to_bytes(pubkey_hex);
		let encoded = encode_bech32("bc", &pubkey_bytes);

		let hostname = format!("{}.nodes.lightning.wiki.", encoded);
		let result = decode_pubkey_from_hostname(&hostname);
		assert!(result.is_err());
		match result.unwrap_err() {
			DnsBootstrapError::InvalidBech32NodeId { reason, .. } => {
				assert!(reason.contains("expected HRP 'ln'"), "got: {}", reason);
			},
			other => panic!("unexpected error: {:?}", other),
		}
	}

	#[test]
	fn test_decode_empty_hostname() {
		let result = decode_pubkey_from_hostname("");
		assert!(result.is_err());
	}

	#[test]
	fn test_decode_garbage_bech32() {
		let result = decode_pubkey_from_hostname("not-valid-bech32.example.com.");
		assert!(result.is_err());
	}

	#[test]
	fn test_decode_valid_bech32_invalid_pubkey() {
		let short_data = vec![0x01, 0x02, 0x03];
		let encoded = encode_bech32("ln", &short_data);

		let hostname = format!("{}.example.com.", encoded);
		let result = decode_pubkey_from_hostname(&hostname);
		assert!(result.is_err());
		match result.unwrap_err() {
			DnsBootstrapError::InvalidPublicKey { .. } => {},
			other => panic!("unexpected error: {:?}", other),
		}
	}

	#[test]
	fn test_roundtrip_multiple_pubkeys() {
		// Test with a few different valid compressed pubkeys.
		let pubkeys = [
			"02e89ca9e8da72b33d896bae51d20e7e1f0571cece852d1387b4e2f4ae48c80a85",
			"03e7156ae33b0a208d0744199163177e909e80176e55d97a2f221ede0f934dd9ad",
		];
		for hex in &pubkeys {
			let bytes = hex_to_bytes(hex);
			let pubkey = PublicKey::from_slice(&bytes).unwrap();
			let encoded = encode_bech32("ln", &bytes);
			let hostname = format!("{}.lseed.bitcoinstats.com.", encoded);

			let (decoded, _) = decode_pubkey_from_hostname(&hostname).unwrap();
			assert_eq!(decoded, pubkey);
		}
	}
}
