use crate::cli::LdkUserInfo;
use std::env;
use std::fs;

pub(crate) fn parse_startup_args() -> Result<LdkUserInfo, ()> {
	let args: Vec<String> = env::args().collect();
	if args.len() < 2 {
		println!("Usage: {} <ldk_storage_directory_path>", args[0]);
		println!();
		println!(
			"The config.toml file should be located at <ldk_storage_directory_path>/.ldk/config.toml",
		);
		crate::config::print_config_help();
		return Err(());
	}

	let ldk_storage_dir_path = args[1].clone();
	let ldk_data_dir = format!("{}/.ldk", ldk_storage_dir_path);

	if let Err(e) = fs::create_dir_all(&ldk_data_dir) {
		println!("ERROR: Failed to create LDK data directory {}: {}", ldk_data_dir, e);
		return Err(());
	}

	let config = match crate::config::NodeConfig::load(&ldk_data_dir) {
		Ok(c) => c,
		Err(crate::config::ConfigError::FileNotFound(path)) => {
			println!("ERROR: Config file not found at {}", path);
			println!();
			crate::config::print_config_help();
			return Err(());
		},
		Err(crate::config::ConfigError::DeprecatedJsonConfig(path)) => {
			println!(
				"ERROR: Deprecated config file detected at {}. config.json is no longer supported.",
				path
			);
			println!(
				"Please migrate to <ldk_storage_directory_path>/.ldk/config.toml using config.example.toml as a template."
			);
			println!();
			crate::config::print_config_help();
			return Err(());
		},
		Err(crate::config::ConfigError::ParseError(msg)) => {
			println!("ERROR: {}", msg);
			println!();
			crate::config::print_config_help();
			return Err(());
		},
		Err(crate::config::ConfigError::ValidationError(msg)) => {
			println!("ERROR: Config validation failed: {}", msg);
			return Err(());
		},
	};

	Ok(config.into_ldk_user_info(ldk_storage_dir_path))
}
