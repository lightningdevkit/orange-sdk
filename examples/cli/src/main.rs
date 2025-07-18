use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use orange_sdk::bitcoin_payment_instructions::amount::Amount;
use orange_sdk::trusted_wallet::spark::Spark;
use orange_sdk::{
	ChainSource, Mnemonic, PaymentInfo, Seed, SparkWalletConfig, StorageConfig, Tunables, Wallet,
	WalletConfig, bitcoin, bitcoin::Network,
};
use spark::operator::OperatorPoolConfig;
use spark::ssp::ServiceProviderConfig;
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
	/// Process wallet events
	Events,
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

		// Use a test mnemonic for demo
		let mnemonic = Mnemonic::from_str(
			"abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
		)
		.context("Failed to parse mnemonic")?;
		println!("{} Using test seed: {}", "🔑".bright_yellow(), mnemonic);
		let seed = Seed::Mnemonic { mnemonic, passphrase: None };

		// Hardcoded LSP config for demo
		let lsp_address = "185.150.162.100:3552"
			.parse()
			.map_err(|_| anyhow::anyhow!("Failed to parse LSP address"))?;
		let lsp_pubkey = "034200de55aeb3126b3b7a2cc7489b88496e9a84347196f563fdba29f79a6f8084"
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
					"address": "https://0.spark.loadtest.dev.sparkinfra.net/",
					"identity_public_key": "03d8d2d331e07f572636dfd371a30dfa139a8bdc99ea98f1f48e27dcc664589ecc"
				},
				{
					"id": 1,
					"identifier": "0000000000000000000000000000000000000000000000000000000000000002",
					"address": "https://1.spark.loadtest.dev.sparkinfra.net/",
					"identity_public_key": "023b1f3e062137ffc541a8edeaab7a4648aafa506d0208956123507d66d3886ac6"
				},
				{
					"id": 2,
					"identifier": "0000000000000000000000000000000000000000000000000000000000000003",
					"address": "https://2.spark.loadtest.dev.sparkinfra.net/",
					"identity_public_key": "02a2c62aa3230d9a51759b3d67399f57223455656369d28120fb39ef062b4469c8"
				}
			]
		}"#;
		let operator_pool: OperatorPoolConfig = serde_json::from_str(pool_config)?;

		let config = WalletConfig {
			storage_config: StorageConfig::LocalSQLite(storage_path.to_string()),
			log_file: PathBuf::from(format!("{}/wallet.log", storage_path)),
			chain_source: ChainSource::Esplora {
				url: "https://regtest-mempool.loadtest.dev.sparkinfra.net/api".to_string(),
				username: Some("spark-sdk".to_string()),
				password: Some("mCMk1JqlBNtetUNy".to_string()),
			},
			lsp: (lsp_address, lsp_pubkey, None),
			network,
			seed,
			tunables: Tunables::default(),
			extra_config: SparkWalletConfig {
				network: Network::Regtest.try_into().unwrap(),
				operator_pool,
				reconnect_interval_seconds: 1,
				service_provider_config: ServiceProviderConfig {
					base_url: "https://api.loadtest.dev.sparkinfra.net".to_string(),
					schema_endpoint: Some("graphql/spark/rc".to_string()),
					identity_public_key: bitcoin::secp256k1::PublicKey::from_str(
						"03e23a4912c275d1ba8742cfdfc7e9befdc2243a74be2412b7b77d227643353a1f",
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
		"events" => Ok(Commands::Events),
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
						format_amount(balance.available_balance)
					);
					println!("Pending balance: {} sats", format_amount(balance.pending_balance));
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
							println!("{} Check events for payment status", "💡".bright_yellow());
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
							println!("Invoice for {} sats:", format_amount(amt));
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
				format_amount(tunables.trusted_balance_limit)
			);
			println!("  Rebalance minimum: {} sats", format_amount(tunables.rebalance_min));
			println!(
				"  On-chain receive threshold: {} sats",
				format_amount(tunables.onchain_receive_threshold)
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
									.map(|a| format_amount(a))
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
		Commands::Events => {
			let wallet = state.wallet();

			println!("{} Processing events...", "🔔".bright_yellow());

			let mut event_count = 0;
			while let Some(event) = wallet.next_event() {
				event_count += 1;
				println!("{} Event {}: {:?}", "📧".bright_blue(), event_count, event);

				match wallet.event_handled() {
					Ok(()) => {},
					Err(_) => {
						println!("{} Failed to mark event as handled", "⚠️".bright_yellow());
					},
				}
			}

			if event_count == 0 {
				println!("No pending events.");
			} else {
				println!("Processed {} events.", event_count);
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

fn format_amount(amount: Amount) -> String {
	amount.sats().map(|s| s.to_string()).unwrap_or_else(|_| "?".to_string())
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
