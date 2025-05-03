# orange-sdk

`orange-sdk` aims to be the simplest library for integrating bitcoin and lightning capabilities into your applications.

## Key Features

*   **Simplified Bitcoin & Lightning:** Provides an easy-to-use interface for common wallet operations.
*   **Hybrid Custody Model:** Offers a unique approach to balance management, starting with a trusted setup and transitioning to self-custody.
*   **Automatic Channel Management:** Intelligently opens Lightning channels to move funds into user self-custody based on configurable thresholds.

## How it Works

1.  **Receive Funds:** Initially, funds are received into a trusted wallet backend. Currently, [Spark](https://www.spark.money/) is the supported trusted backend.
2.  **Monitor Balance:** The SDK monitors the balance held in the trusted wallet.
3.  **Transition to Self-Custody:** When the balance exceeds a predefined threshold (`trusted_balance_limit`), the SDK automatically initiates the process of opening a Lightning Network channel with a configured Lightning Service Provider (LSP). This moves the funds from the trusted environment into a self-custodial Lightning channel managed by the SDK using LDK Node.

## Getting Started

*(Add instructions on how to install and configure the SDK)*

## Usage

*(Add basic examples of how to use the SDK's core functions, like creating a wallet, getting balance, sending/receiving payments)*

## Contributing

*(Add guidelines for contributors)*

## License

*(Specify the license for the project)*
