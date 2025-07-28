use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use orange_sdk::bitcoin_payment_instructions::amount::Amount;
use orange_sdk::trusted_wallet::spark::Spark;
use orange_sdk::{
	ChainSource, Event, Mnemonic, PaymentInfo, Seed, SparkWalletConfig, StorageConfig, Tunables,
	Wallet, WalletConfig, bitcoin, bitcoin::Network,
};
use rand::RngCore;
use spark::operator::OperatorPoolConfig;
use spark::ssp::ServiceProviderConfig;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::runtime::Runtime;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
	/// Get wallet balance
	Balance,
	/// Send a payment
	Send {
		/// Destination address or invoice
		destination: String,
		/// Amount in sats
		amount: u64,
	},
	/// Receive a payment
	Receive {
		/// Amount in sats (optional)
		amount: Option<u64>,
	},
	/// Show wallet status
	Status,
	/// List recent transactions
	Transactions,
	/// Clear the screen
	Clear,
	/// Exit the application
	Exit,
}

struct WalletState {
	wallet: Wallet<Spark>,
	_runtime: Arc<Runtime>, // Keep runtime alive
}

impl WalletState {
	async fn new(runtime: Arc<Runtime>) -> Result<Self> {
		// Hardcoded configuration
		let network = Network::Regtest;
		let storage_path = "./wallet_data";

		// Generate or load seed
		let seed = generate_or_load_seed(storage_path)?;

		// Hardcoded LSP config for demo
		let lsp_address = "185.150.162.100:3551"
			.parse()
			.map_err(|_| anyhow::anyhow!("Failed to parse LSP address"))?;
		let lsp_pubkey = "02a88abd44b3cfc9c0eb7cd93f232dc473de4f66bcea0ee518be70c3b804c90201"
			.parse()
			.context("Failed to parse LSP public key")?;

		// pool config json
		let pool_config = r#"
		{
			"coordinator_index": 0,
			"operators": [
				{
					"id": 0,
					"identifier": "0000000000000000000000000000000000000000000000000000000000000001",
					"address": "https://0.spark.lightspark.com",
					"identity_public_key": "03dfbdff4b6332c220f8fa2ba8ed496c698ceada563fa01b67d9983bfc5c95e763"
				},
				{
					"id": 1,
					"identifier": "0000000000000000000000000000000000000000000000000000000000000002",
					"address": "https://1.spark.lightspark.com",
					"identity_public_key": "03e625e9768651c9be268e287245cc33f96a68ce9141b0b4769205db027ee8ed77"
				},
				{
					"id": 2,
					"identifier": "0000000000000000000000000000000000000000000000000000000000000003",
					"address": "https://2.spark.flashnet.xyz",
					"identity_public_key": "022eda13465a59205413086130a65dc0ed1b8f8e51937043161f8be0c369b1a410"
				}
			]
		}"#;
		let operator_pool: OperatorPoolConfig = serde_json::from_str(pool_config)?;

		let config = WalletConfig {
			storage_config: StorageConfig::LocalSQLite(storage_path.to_string()),
			log_file: PathBuf::from(format!("{storage_path}/wallet.log")),
			chain_source: ChainSource::Electrum(
				"tcp://spark-regtest.benthecarman.com:50001".to_string(),
			),
			lsp: (lsp_address, lsp_pubkey, None),
			network,
			seed,
			tunables: Tunables::default(),
			extra_config: SparkWalletConfig {
				network: Network::Regtest.try_into().unwrap(),
				operator_pool,
				reconnect_interval_seconds: 1,
				service_provider_config: ServiceProviderConfig {
					base_url: "https://api.lightspark.com".to_string(),
					schema_endpoint: Some("graphql/spark/rc".to_string()),
					identity_public_key: bitcoin::secp256k1::PublicKey::from_str(
						"022bf283544b16c0622daecb79422007d167eca6ce9f0c98c0c49833b1f7170bfe",
					)?,
				},
				split_secret_threshold: 2,
			},
		};

		println!("{} Initializing wallet...", "⚡".bright_yellow());

		match Wallet::new(runtime.clone(), config).await {
			Ok(wallet) => {
				println!("{} Wallet initialized successfully!", "✅".bright_green());
				println!("Network: {}", "regtest".bright_cyan());
				println!("Storage: {}", storage_path.bright_cyan());

				let w = wallet.clone();
				runtime.spawn(async move {
					let event = w.next_event_async().await;
					match event {
						Event::PaymentSuccessful { payment_id, .. } => {
							println!("{} Payment successful: {}", "✅".bright_green(), payment_id);
						},
						Event::PaymentFailed { payment_id, .. } => {
							println!("{} Payment failed: {}", "❌".bright_red(), payment_id);
						},
						Event::PaymentReceived { payment_id, amount_msat, .. } => {
							println!(
								"{} Payment received: {} ({} msat)",
								"📥".bright_green(),
								payment_id,
								amount_msat
							);
						},
						Event::OnchainPaymentReceived { txid, amount_sat, .. } => {
							println!(
								"{} On-chain payment received: {} ({} sats)",
								"📥".bright_green(),
								txid,
								amount_sat
							);
						},
						Event::ChannelOpened { .. } => {
							println!("{} Channel opened", "🔓".bright_green());
						},
						Event::ChannelClosed { reason, .. } => {
							println!(
								"{} Channel closed: {}",
								"🔒".bright_red(),
								reason
									.map(|r| r.to_string())
									.unwrap_or_else(|| "Unknown reason".to_string())
							);
						},
						Event::RebalanceInitiated { amount_msat, .. } => {
							println!(
								"{} Rebalance initiated: {} msat",
								"🔄".bright_yellow(),
								amount_msat
							);
						},
						Event::RebalanceSuccessful { amount_msat, fee_msat, .. } => {
							println!(
								"{} Rebalance successful: {} msat (fee: {} msat)",
								"✅".bright_green(),
								amount_msat,
								fee_msat
							);
						},
					}

					w.event_handled().unwrap();
				});

				Ok(WalletState { wallet, _runtime: runtime })
			},
			Err(e) => Err(anyhow::anyhow!("Failed to initialize wallet: {:?}", e)),
		}
	}

	fn wallet(&self) -> &Wallet<Spark> {
		&self.wallet
	}
}

fn main() -> Result<()> {
	let cli = Cli::parse();

	println!("{}", "🟠 Orange CLI Wallet".bright_yellow().bold());
	println!("{}", "Type 'help' for available commands or 'exit' to quit".dimmed());
	println!();

	// Create runtime outside async context to avoid drop issues
	let runtime = Arc::new(Runtime::new().context("Failed to create tokio runtime")?);

	// Initialize wallet once at startup
	let mut state = runtime.block_on(WalletState::new(runtime.clone()))?;

	// If a command was provided via command line, execute it and start interactive mode
	if let Some(command) = cli.command {
		runtime.block_on(execute_command(command, &mut state))?;
		println!();
	}

	// Start interactive mode
	runtime.block_on(start_interactive_mode(state))
}

async fn start_interactive_mode(mut state: WalletState) -> Result<()> {
	let mut rl = DefaultEditor::new().context("Failed to create readline editor")?;

	loop {
		let prompt = format!("{} ", "orange>".bright_green().bold());

		let readline = rl.readline(&prompt);
		match readline {
			Ok(line) => {
				let line = line.trim();
				if line.is_empty() {
					continue;
				}

				rl.add_history_entry(line).ok();

				match parse_command(line) {
					Ok(command) => {
						if let Err(e) = execute_command(command, &mut state).await {
							println!("{} {}", "Error:".bright_red().bold(), e);
						}
					},
					Err(e) => {
						let error_msg = e.to_string();
						if !error_msg.is_empty() {
							println!("{} {}", "Parse error:".bright_red().bold(), e);
							println!(
								"{} Type 'help' for available commands",
								"Hint:".bright_yellow().bold()
							);
						}
					},
				}
			},
			Err(ReadlineError::Interrupted) => {
				println!("{}", "Use 'exit' to quit".bright_yellow());
				continue;
			},
			Err(ReadlineError::Eof) => {
				println!("{}", "Goodbye!".bright_green());
				break;
			},
			Err(err) => {
				println!("{} {:?}", "Error:".bright_red().bold(), err);
				break;
			},
		}
	}

	Ok(())
}

fn parse_command(input: &str) -> Result<Commands> {
	let parts: Vec<&str> = input.split_whitespace().collect();
	if parts.is_empty() {
		return Err(anyhow::anyhow!("Empty command"));
	}

	match parts[0].to_lowercase().as_str() {
		"balance" => Ok(Commands::Balance),
		"send" => {
			if parts.len() < 3 {
				return Err(anyhow::anyhow!("Usage: send <destination> <amount>"));
			}
			let destination = parts[1].to_string();
			let amount = parts[2].parse::<u64>().context("Amount must be a valid number")?;
			Ok(Commands::Send { destination, amount })
		},
		"receive" => {
			let amount = if parts.len() > 1 {
				Some(parts[1].parse::<u64>().context("Amount must be a valid number")?)
			} else {
				None
			};
			Ok(Commands::Receive { amount })
		},
		"status" => Ok(Commands::Status),
		"transactions" | "tx" => Ok(Commands::Transactions),
		"clear" | "cls" => Ok(Commands::Clear),
		"exit" | "quit" => Ok(Commands::Exit),
		"help" => {
			print_help();
			Err(anyhow::anyhow!(""))
		},
		_ => Err(anyhow::anyhow!("Unknown command: {}", parts[0])),
	}
}

async fn execute_command(command: Commands, state: &mut WalletState) -> Result<()> {
	match command {
		Commands::Balance => {
			let wallet = state.wallet();

			println!("{} Fetching balance...", "💰".bright_yellow());

			match wallet.get_balance().await {
				Ok(balance) => {
					println!(
						"Available balance: {} sats",
						balance.available_balance.sats().unwrap_or(0)
					);
					println!(
						"Pending balance: {} sats",
						balance.pending_balance.sats().unwrap_or(0)
					);
				},
				Err(e) => {
					return Err(anyhow::anyhow!("Failed to get balance: {:?}", e));
				},
			}
		},
		Commands::Send { destination, amount } => {
			let wallet = state.wallet();

			println!("{} Sending payment...", "📤".bright_yellow());
			println!("Destination: {}", destination.bright_cyan());
			println!("Amount: {} sats", amount.to_string().bright_green().bold());

			let amount =
				Amount::from_sats(amount).map_err(|_| anyhow::anyhow!("Invalid amount"))?;

			match wallet.parse_payment_instructions(&destination).await {
				Ok(instructions) => match PaymentInfo::build(instructions, amount) {
					Ok(payment_info) => match wallet.pay(&payment_info).await {
						Ok(()) => {
							println!("{} Payment initiated successfully!", "✅".bright_green());
						},
						Err(e) => {
							return Err(anyhow::anyhow!("Failed to send payment: {:?}", e));
						},
					},
					Err(_) => {
						return Err(anyhow::anyhow!(
							"Payment amount doesn't match instruction requirements"
						));
					},
				},
				Err(e) => {
					return Err(anyhow::anyhow!("Failed to parse payment instructions: {:?}", e));
				},
			}
		},
		Commands::Receive { amount } => {
			let wallet = state.wallet();

			println!("{} Generating payment request...", "📥".bright_yellow());

			let amount = amount
				.map(|amt| Amount::from_sats(amt))
				.transpose()
				.map_err(|_| anyhow::anyhow!("Invalid amount"))?;

			match wallet.get_single_use_receive_uri(amount).await {
				Ok(uri) => {
					match amount {
						Some(amt) => {
							println!("Invoice for {} sats:", amt.sats().unwrap_or(0));
						},
						None => {
							println!("Invoice for any amount:");
						},
					}
					println!("{}", uri.invoice.to_string().bright_cyan());
					if let Some(ref address) = uri.address {
						println!("On-chain address: {}", address.to_string().bright_cyan());
					}
					println!("Full URI: {}", uri.to_string().bright_cyan());
				},
				Err(e) => {
					return Err(anyhow::anyhow!("Failed to generate receive URI: {:?}", e));
				},
			}
		},
		Commands::Status => {
			let wallet = state.wallet();

			println!("{} Wallet Status", "📊".bright_blue().bold());
			println!("Status: {}", "Initialized".bright_green());
			println!("Node ID: {}", wallet.node_id().to_string().bright_cyan());
			println!(
				"LSP Connected: {}",
				if wallet.is_connected_to_lsp() { "Yes".bright_green() } else { "No".bright_red() }
			);
			println!(
				"Rebalance Enabled: {}",
				if wallet.get_rebalance_enabled() {
					"Yes".bright_green()
				} else {
					"No".bright_red()
				}
			);

			let tunables = wallet.get_tunables();
			println!("Tunables:");
			println!(
				"  Trusted balance limit: {} sats",
				tunables.trusted_balance_limit.sats().unwrap_or(0)
			);
			println!("  Rebalance minimum: {} sats", tunables.rebalance_min.sats().unwrap_or(0));
			println!(
				"  On-chain receive threshold: {} sats",
				tunables.onchain_receive_threshold.sats().unwrap_or(0)
			);
		},
		Commands::Transactions => {
			let wallet = state.wallet();

			println!("{} Fetching transactions...", "📋".bright_yellow());

			match wallet.list_transactions().await {
				Ok(transactions) => {
					if transactions.is_empty() {
						println!("No transactions found.");
					} else {
						println!("Found {} transactions:", transactions.len());
						for (i, tx) in transactions.iter().enumerate() {
							let status_icon = match tx.status {
								orange_sdk::TxStatus::Pending => "⏳",
								orange_sdk::TxStatus::Completed => "✅",
								orange_sdk::TxStatus::Failed => "❌",
							};
							let direction_icon = if tx.outbound { "📤" } else { "📥" };

							println!(
								"{} {} {} {} {} sats",
								i + 1,
								status_icon,
								direction_icon,
								format!("{:?}", tx.payment_type).bright_cyan(),
								tx.amount
									.map(|a| a.sats().unwrap_or(0).to_string())
									.unwrap_or_else(|| "?".to_string())
									.bright_green()
							);
						}
					}
				},
				Err(e) => {
					return Err(anyhow::anyhow!("Failed to list transactions: {:?}", e));
				},
			}
		},
		Commands::Clear => {
			print!("\x1B[2J\x1B[1;1H");
			std::io::stdout().flush().unwrap();
		},
		Commands::Exit => {
			println!("{} Stopping wallet...", "⏹️".bright_yellow());
			state.wallet().stop().await;
			println!("{} Goodbye!", "👋".bright_green());
			std::process::exit(0);
		},
	}
	Ok(())
}

fn print_help() {
	println!("{}", "Available Commands:".bright_blue().bold());
	println!();
	println!("  {}", "balance".bright_green().bold());
	println!("    Show wallet balance (auto-initializes wallet if needed)");
	println!();
	println!("  {} <destination> <amount>", "send".bright_green().bold());
	println!("    Send a payment to an address or invoice");
	println!();
	println!("  {} [amount]", "receive".bright_green().bold());
	println!("    Generate a payment request (invoice)");
	println!();
	println!("  {}", "status".bright_green().bold());
	println!("    Show wallet status information");
	println!();
	println!("  {}", "transactions".bright_green().bold());
	println!("    List recent transactions");
	println!();
	println!("  {}", "events".bright_green().bold());
	println!("    Process pending wallet events");
	println!();
	println!("  {}", "clear".bright_green().bold());
	println!("    Clear the terminal screen");
	println!();
	println!("  {}", "exit".bright_green().bold());
	println!("    Exit the application");
	println!();
	println!("{}", "Examples:".bright_blue().bold());
	println!("  balance");
	println!("  send lnbc1... 10000");
	println!("  receive 25000");
	println!("  events");
	println!();
	println!("{}", "Note:".bright_yellow().bold());
	println!("  The wallet will be auto-initialized on first use with:");
	println!("  - Network: regtest");
	println!("  - Storage: ./wallet_data");
	println!("  - Generated seed phrase (displayed on init)");
}

fn generate_or_load_seed(storage_path: &str) -> Result<Seed> {
	let seed_file_path = format!("{}/seed.txt", storage_path);

	// Try to load existing seed
	if let Ok(seed_content) = fs::read_to_string(&seed_file_path) {
		let mnemonic_str = seed_content.trim();
		match Mnemonic::from_str(mnemonic_str) {
			Ok(mnemonic) => {
				println!("{} Loaded existing seed from {}", "🔑".bright_green(), seed_file_path);
				return Ok(Seed::Mnemonic { mnemonic, passphrase: None });
			},
			Err(e) => {
				println!(
					"{} Warning: Failed to parse existing seed file: {}",
					"⚠️".bright_yellow(),
					e
				);
				println!("{} Generating new seed...", "🔄".bright_yellow());
			},
		}
	}

	// Generate new seed
	let mut entropy = [0u8; 16]; // 128 bits for 12-word mnemonic
	rand::thread_rng().fill_bytes(&mut entropy);
	let mnemonic = Mnemonic::from_entropy(&entropy)?;
	println!("{} Generated new seed: {}", "🔑".bright_yellow(), mnemonic);

	// Create storage directory if it doesn't exist
	fs::create_dir_all(storage_path)
		.with_context(|| format!("Failed to create storage directory: {}", storage_path))?;

	// Save seed to file
	fs::write(&seed_file_path, mnemonic.to_string())
		.with_context(|| format!("Failed to write seed to file: {}", seed_file_path))?;

	println!("{seed_file_path} Seed saved to {}", "💾".bright_green());
	println!(
		"{} Keep this seed phrase safe - it's needed to recover your wallet!",
		"⚠️".bright_red().bold()
	);

	Ok(Seed::Mnemonic { mnemonic, passphrase: None })
}
