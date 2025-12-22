use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use orange_sdk::bitcoin_payment_instructions::amount::Amount;
use orange_sdk::{
	ChainSource, Event, ExtraConfig, LoggerType, Mnemonic, PaymentInfo, Seed, SparkWalletConfig,
	StorageConfig, Tunables, Wallet, WalletConfig, bitcoin::Network,
};
use rand::RngCore;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal;

const NETWORK: Network = Network::Bitcoin; // Supports Bitcoin and Regtest

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
		/// Amount in sats (optional)
		amount: Option<u64>,
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
	/// List our lightning Channels
	Channels,
	/// Clear the screen
	Clear,
	/// Exit the application
	Exit,
}

struct WalletState {
	wallet: Wallet,
	shutdown: Arc<AtomicBool>,
}

fn get_config(network: Network) -> Result<WalletConfig> {
	let storage_path = format!("./wallet_data/{network}");

	// Generate or load seed
	let seed = generate_or_load_seed(&storage_path)?;

	match network {
		Network::Regtest => {
			let lsp_address = "185.150.162.100:3551"
				.parse()
				.map_err(|_| anyhow::anyhow!("Failed to parse LSP address"))?;
			let lsp_pubkey = "02a88abd44b3cfc9c0eb7cd93f232dc473de4f66bcea0ee518be70c3b804c90201"
				.parse()
				.context("Failed to parse LSP public key")?;

			Ok(WalletConfig {
				storage_config: StorageConfig::LocalSQLite(storage_path.to_string()),
				logger_type: LoggerType::File {
					path: PathBuf::from(format!("{storage_path}/wallet.log")),
				},
				chain_source: ChainSource::Electrum(
					"tcp://spark-regtest.benthecarman.com:50001".to_string(),
				),
				lsp: (lsp_address, lsp_pubkey, None),
				scorer_url: None,
				rgs_url: None,
				network,
				seed,
				tunables: Tunables::default(),
				extra_config: ExtraConfig::Spark(SparkWalletConfig::default()),
			})
		},
		Network::Bitcoin => {
			// Matt's LSP config for demo
			let lsp_address = "69.59.18.144:9735"
				.parse()
				.map_err(|_| anyhow::anyhow!("Failed to parse LSP address"))?;
			let lsp_pubkey = "021deaa26ce6bb7cc63bd30e83a2bba1c0368269fa3bb9b616a24f40d941ac7d32"
				.parse()
				.context("Failed to parse LSP public key")?;
			let lsp_token = Some("DeveloperTestingOnly".to_string());

			Ok(WalletConfig {
				storage_config: StorageConfig::LocalSQLite(storage_path.to_string()),
				logger_type: LoggerType::File {
					path: PathBuf::from(format!("{storage_path}/wallet.log")),
				},
				chain_source: ChainSource::Esplora {
					url: "https://blockstream.info/api".to_string(),
					username: None,
					password: None,
				},
				lsp: (lsp_address, lsp_pubkey, lsp_token),
				scorer_url: None,
				rgs_url: None,
				network,
				seed,
				tunables: Tunables::default(),
				extra_config: ExtraConfig::Spark(SparkWalletConfig::default()),
			})
		},
		_ => Err(anyhow::anyhow!("Unsupported network: {network:?}")),
	}
}

impl WalletState {
	async fn new() -> Result<Self> {
		let shutdown = Arc::new(AtomicBool::new(false));
		let config = get_config(NETWORK)
			.with_context(|| format!("Failed to get wallet config for network: {NETWORK:?}"))?;

		println!("{} Initializing wallet...", "⚡".bright_yellow());

		match Wallet::new(config).await {
			Ok(wallet) => {
				println!("{} Wallet initialized successfully!", "✅".bright_green());
				println!("Network: {}", NETWORK.to_string().bright_cyan());

				let w = wallet.clone();
				tokio::spawn(async move {
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
						Event::ChannelOpened { funding_txo, .. } => {
							println!("{} Channel opened: {funding_txo}", "🔓".bright_green());
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
						Event::SplicePending { new_funding_txo, .. } => {
							println!(
								"{} Splice pending: {}",
								"🔄".bright_yellow(),
								new_funding_txo
							);
						},
					}

					w.event_handled().unwrap();
				});

				Ok(WalletState { wallet, shutdown })
			},
			Err(e) => Err(anyhow::anyhow!("Failed to initialize wallet: {:?}", e)),
		}
	}

	fn wallet(&self) -> &Wallet {
		&self.wallet
	}

	fn is_shutdown_requested(&self) -> bool {
		self.shutdown.load(Ordering::Relaxed)
	}
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
	let cli = Cli::parse();

	println!("{}", "🟠 Orange CLI Wallet".bright_yellow().bold());
	println!("{}", "Type 'help' for available commands or 'exit' to quit".dimmed());
	println!();

	// Initialize wallet once at startup
	let mut state = WalletState::new().await?;

	// Set up signal handling for graceful shutdown
	let shutdown_state = state.shutdown.clone();
	let shutdown_wallet = state.wallet.clone();
	tokio::task::spawn(async move {
		if let Ok(()) = signal::ctrl_c().await {
			println!("\n{} Shutdown signal received, stopping wallet...", "⏹️".bright_yellow());
			shutdown_state.store(true, Ordering::Relaxed);
			shutdown_wallet.stop().await;
			println!("{} Goodbye!", "👋".bright_green());
			std::process::exit(0);
		}
	});

	// If a command was provided via command line, execute it and start interactive mode
	if let Some(command) = cli.command {
		execute_command(command, &mut state).await?;
		println!();
	}

	// Start interactive mode
	start_interactive_mode(state).await
}

async fn start_interactive_mode(mut state: WalletState) -> Result<()> {
	let mut rl = DefaultEditor::new().context("Failed to create readline editor")?;

	loop {
		// Check if shutdown was requested by signal handler
		if state.is_shutdown_requested() {
			break;
		}

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
				println!("\n{} Stopping wallet...", "⏹️".bright_yellow());
				state.wallet().stop().await;
				println!("{} Goodbye!", "👋".bright_green());
				break;
			},
			Err(ReadlineError::Eof) => {
				println!("\n{} Stopping wallet...", "⏹️".bright_yellow());
				state.wallet().stop().await;
				println!("{} Goodbye!", "👋".bright_green());
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
		"balance" | "bal" => Ok(Commands::Balance),
		"send" | "pay" => {
			if parts.len() < 2 {
				return Err(anyhow::anyhow!("Usage: send <destination> <amount>"));
			}
			let destination = parts[1].to_string();

			let amount = if parts.len() > 2 {
				Some(parts[2].parse::<u64>().context("Amount must be a valid number")?)
			} else {
				None
			};

			Ok(Commands::Send { destination, amount })
		},
		"receive" | "recv" => {
			let amount = if parts.len() > 1 {
				Some(parts[1].parse::<u64>().context("Amount must be a valid number")?)
			} else {
				None
			};
			Ok(Commands::Receive { amount })
		},
		"status" => Ok(Commands::Status),
		"transactions" | "txs" | "tx" => Ok(Commands::Transactions),
		"channels" | "chan" => Ok(Commands::Channels),
		"clear" | "cls" => Ok(Commands::Clear),
		"exit" | "quit" | "q" => Ok(Commands::Exit),
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
					println!("Trusted balance: {} sats", balance.trusted.sats_rounding_up());
					println!("LN balance: {} sats", balance.lightning.sats_rounding_up());
					println!(
						"Pending balance: {} sats",
						balance.pending_balance.sats_rounding_up()
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
			if let Some(amount) = amount {
				println!("Amount: {} sats", amount.to_string().bright_green().bold());
			}

			let amount = amount
				.map(|a| Amount::from_sats(a).map_err(|_| anyhow::anyhow!("Invalid amount")))
				.transpose()?;

			match wallet.parse_payment_instructions(&destination).await {
				Ok(instructions) => match PaymentInfo::build(instructions, amount) {
					Ok(payment_info) => match wallet.pay(&payment_info).await {
						Ok(_) => {
							println!("{} Payment initiated successfully!", "✅".bright_green());
						},
						Err(e) => {
							return Err(anyhow::anyhow!("Failed to send payment: {e:?}"));
						},
					},
					Err(_) => {
						return Err(anyhow::anyhow!(
							"Payment amount doesn't match instruction requirements"
						));
					},
				},
				Err(e) => {
					return Err(anyhow::anyhow!("Failed to parse payment instructions: {e:?}"));
				},
			}
		},
		Commands::Receive { amount } => {
			let wallet = state.wallet();

			println!("{} Generating payment request...", "📥".bright_yellow());

			let amount = amount
				.map(Amount::from_sats)
				.transpose()
				.map_err(|_| anyhow::anyhow!("Invalid amount"))?;

			match wallet.get_single_use_receive_uri(amount).await {
				Ok(uri) => {
					match amount {
						Some(amt) => {
							println!("Invoice for {} sats:", amt.sats_rounding_up());
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
				tunables.trusted_balance_limit.sats_rounding_up()
			);
			println!("  Rebalance minimum: {} sats", tunables.rebalance_min.sats_rounding_up());
			println!(
				"  On-chain receive threshold: {} sats",
				tunables.onchain_receive_threshold.sats_rounding_up()
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
									.map(|a| a.sats_rounding_up().to_string())
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
		Commands::Channels => {
			let wallet = state.wallet();

			let channels = wallet.channels();

			if channels.is_empty() {
				println!("{} No channels found.", "🔒".bright_yellow());
			} else {
				println!("{} Found {} channels:", "🔒".bright_green(), channels.len());
				for channel in channels {
					let status = if channel.is_usable {
						"Usable".bright_green()
					} else if channel.is_channel_ready {
						"Ready".bright_yellow()
					} else {
						"Inactive".bright_red()
					};
					println!(
						"{} Channel TXO: {}, Status: {}, Inbound Capacity: {} sats, Outbound Capacity: {} sats",
						"🔗".bright_cyan(),
						channel
							.funding_txo
							.map(|t| t.to_string())
							.unwrap_or("Unknown".to_string())
							.bright_cyan(),
						status.bright_yellow(),
						(channel.inbound_capacity_msat / 1_000).to_string().bright_green(),
						(channel.outbound_capacity_msat / 1_000).to_string().bright_green(),
					);
				}
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
	println!("  {}", "channels".bright_green().bold());
	println!("    List channels");
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
	println!("  - Network: {NETWORK}");
	println!("  - Storage: ./wallet_data/{NETWORK}");
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
