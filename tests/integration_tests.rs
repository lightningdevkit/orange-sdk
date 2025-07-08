#![cfg(feature = "_test-utils")]

use crate::test_utils::{
	TestParams, build_test_nodes, generate_blocks, open_channel_from_lsp, wait_next_event,
};
use bitcoin_payment_instructions::amount::Amount;
use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Description};
use ldk_node::payment::{ConfirmationStatus, PaymentDirection, PaymentStatus};
use orange_sdk::{Event, PaymentInfo, PaymentType, TxStatus};
use std::sync::Arc;
use std::time::Duration;

mod test_utils;

#[test]
fn test_node_start() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		let bal = wallet.get_balance().await;
		assert_eq!(bal.available_balance, Amount::ZERO);
		assert_eq!(bal.pending_balance, Amount::ZERO);
	})
}

#[test]
fn test_receive_to_trusted() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let starting_bal = wallet.get_balance().await;
		assert_eq!(starting_bal.available_balance, Amount::ZERO);
		assert_eq!(starting_bal.pending_balance, Amount::ZERO);

		let recv_amt = Amount::from_sats(100).unwrap();

		let limit = wallet.get_tunables();
		assert!(recv_amt < limit.trusted_balance_limit);

		let uri = wallet.get_single_use_receive_uri(Some(recv_amt)).await.unwrap();
		let payment_id = third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		// wait for payment success from payer side
		let p = Arc::clone(&third_party);
		test_utils::wait_for_condition(Duration::from_secs(1), 10, "payer payment success", || {
			let res = p.payment(&payment_id).is_some_and(|p| p.status == PaymentStatus::Succeeded);
			async move { res }
		})
		.await
		.expect("Payer payment did not succeed in time");

		// wait for balance update on wallet side
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wallet balance update after receive",
			|| async { wallet.get_balance().await.available_balance > Amount::ZERO },
		)
		.await
		.expect("Wallet balance did not update in time after receive");

		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 1);
		let tx = txs.into_iter().next().unwrap();

		// Comprehensive validation for trusted wallet receive
		assert_eq!(tx.fee, Some(Amount::ZERO), "Trusted wallet receive should have zero fees");
		assert!(!tx.outbound, "Incoming payment should not be outbound");
		assert_eq!(tx.status, TxStatus::Completed, "Payment should be completed");
		assert_eq!(
			tx.payment_type,
			PaymentType::IncomingLightning {},
			"Payment type should be IncomingLightning"
		);
		assert_ne!(tx.time_since_epoch, Duration::ZERO, "Time should be set");
		assert_eq!(
			tx.amount,
			Some(recv_amt),
			"Amount should equal received amount for trusted wallet (no fees deducted)"
		);
	})
}

#[test]
fn test_sweep_to_ln() {
	let TestParams { wallet, lsp, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let starting_bal = wallet.get_balance().await;
		assert_eq!(starting_bal.available_balance, Amount::ZERO);
		assert_eq!(starting_bal.pending_balance, Amount::ZERO);

		let starting_lsp_channels = lsp.list_channels();

		// start with receiving half the limit
		let limit = wallet.get_tunables();
		let recv_amt =
			Amount::from_milli_sats(limit.trusted_balance_limit.milli_sats() / 2).unwrap();

		let uri = wallet.get_single_use_receive_uri(Some(recv_amt)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		// wait for balance update on wallet side
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wallet balance update after receive",
			|| async { wallet.get_balance().await.available_balance > Amount::ZERO },
		)
		.await
		.expect("Wallet balance did not update in time after receive");

		let intermediate_amt = wallet.get_balance().await.available_balance;

		// next receive the limit to trigger the rebalance
		let recv_amt = Amount::from_milli_sats(limit.trusted_balance_limit.milli_sats()).unwrap();

		let uri = wallet.get_single_use_receive_uri(Some(recv_amt)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		// wait for balance update on wallet side
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wallet balance update after receive",
			|| async { wallet.get_balance().await.available_balance > intermediate_amt },
		)
		.await
		.expect("Wallet balance did not update in time after receive");

		let event = wait_next_event(&wallet).await;
		assert!(matches!(event, Event::RebalanceInitiated { .. }));

		// wait for rebalance
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wait for new channel to be opened",
			|| async { starting_lsp_channels.len() < lsp.list_channels().len() },
		)
		.await
		.expect("Wallet did not receive new channel");

		// wait for payment received
		let event = wait_next_event(&wallet).await;
		match event {
			Event::ChannelOpened { counterparty_node_id, .. } => {
				assert_eq!(counterparty_node_id, lsp.node_id());
			},
			_ => panic!("Expected ChannelOpened event"),
		}

		let event = wait_next_event(&wallet).await;
		assert!(matches!(event, Event::PaymentReceived { .. }));

		let event = wait_next_event(&wallet).await;
		match event {
			Event::RebalanceSuccessful { amount_msat, fee_msat, .. } => {
				assert!(fee_msat > 0);
				assert_eq!(amount_msat, intermediate_amt.saturating_add(recv_amt).milli_sats());
			},
			_ => panic!("Expected RebalanceSuccessful event"),
		}

		assert_eq!(wallet.next_event(), None);
	})
}

#[test]
fn test_receive_to_ln() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let recv_amt = open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 1);
		let tx = txs.into_iter().next().unwrap();

		// Comprehensive validation for lightning receive
		assert!(
			tx.fee.is_some_and(|f| f > Amount::ZERO),
			"Lightning receive should have non-zero fees"
		);
		assert!(!tx.outbound, "Incoming payment should not be outbound");
		assert_eq!(tx.status, TxStatus::Completed, "Payment should be completed");
		assert_eq!(
			tx.payment_type,
			PaymentType::IncomingLightning {},
			"Payment type should be IncomingLightning"
		);
		assert_ne!(tx.time_since_epoch, Duration::ZERO, "Time should be set");
		assert_eq!(
			tx.amount,
			Some(recv_amt.saturating_sub(tx.fee.unwrap())),
			"Amount should be receive amount minus fees"
		);

		// Validate fee is reasonable (should be less than 10% of received amount)
		let fee_ratio = tx.fee.unwrap().milli_sats() as f64 / recv_amt.milli_sats() as f64;
		assert!(
			fee_ratio < 0.1,
			"Fee should be less than 10% of received amount, got {:.2}%",
			fee_ratio * 100.0
		);
	})
}

#[test]
fn test_receive_to_onchain() {
	let TestParams { wallet, lsp, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let starting_bal = wallet.get_balance().await;
		assert_eq!(starting_bal.available_balance, Amount::ZERO);
		assert_eq!(starting_bal.pending_balance, Amount::ZERO);

		let recv_amt = Amount::from_sats(200_000).unwrap();

		let uri = wallet.get_single_use_receive_uri(Some(recv_amt)).await.unwrap();
		let sent_txid = third_party
			.onchain_payment()
			.send_to_address(&uri.address.unwrap(), recv_amt.sats().unwrap(), None)
			.unwrap();

		// confirm transaction
		generate_blocks(&bitcoind, 6);

		// check we received on-chain, should be pending
		// wait for payment success
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"pending balance to update",
			|| async {
				// onchain balance is always listed as pending until we splice it into the channel.
				wallet.get_balance().await.pending_balance == recv_amt
			},
		)
		.await
		.expect("Pending balance did not update in time");

		let event = wait_next_event(&wallet).await;

		match event {
			Event::OnchainPaymentReceived { txid, amount_sat, status, .. } => {
				assert_eq!(txid, sent_txid);
				assert_eq!(amount_sat, recv_amt.sats().unwrap());
				assert!(matches!(status, ConfirmationStatus::Confirmed { .. }));
			},
			_ => panic!("Expected OnchainPaymentReceived event"),
		}

		assert!(wallet.next_event().is_none());

		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 1);
		let tx = txs.into_iter().next().unwrap();

		// Comprehensive validation for on-chain receive
		assert!(!tx.outbound, "Incoming payment should not be outbound");
		assert_eq!(tx.status, TxStatus::Completed, "Payment should be completed");
		assert_eq!(
			tx.payment_type,
			PaymentType::IncomingOnChain { txid: Some(sent_txid) },
			"Payment type should be IncomingOnChain with correct txid"
		);
		assert_ne!(tx.time_since_epoch, Duration::ZERO, "Time should be set");
		assert_eq!(tx.amount, Some(recv_amt), "Amount should equal received amount");
		assert_eq!(
			tx.fee,
			Some(Amount::ZERO),
			"On-chain receive should have zero fees (paid by sender)"
		);

		// a rebalance should be initiated, we need to mine the channel opening transaction
		// for it to be confirmed and reflected in the wallet's history
		generate_blocks(&bitcoind, 6);
		tokio::time::sleep(Duration::from_secs(5)).await; // wait for sync
		generate_blocks(&bitcoind, 6); // confirm the channel opening transaction
		tokio::time::sleep(Duration::from_secs(5)).await; // wait for sync

		//  wait for rebalance to be initiated
		let event = wait_next_event(&wallet).await;
		match event {
			Event::ChannelOpened { counterparty_node_id, .. } => {
				assert_eq!(counterparty_node_id, lsp.node_id());
			},
			_ => panic!("Expected ChannelOpened event"),
		}

		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 1);
		let tx = txs.into_iter().next().unwrap();

		// Comprehensive validation for on-chain receive after rebalance
		assert!(!tx.outbound, "Incoming payment should not be outbound");
		assert_eq!(tx.status, TxStatus::Completed, "Payment should be completed");
		assert_eq!(
			tx.payment_type,
			PaymentType::IncomingOnChain { txid: Some(sent_txid) },
			"Payment type should be IncomingOnChain with correct txid"
		);
		assert_ne!(tx.time_since_epoch, Duration::ZERO, "Time should be set");
		assert_eq!(tx.amount, Some(recv_amt), "Amount should equal received amount");
		assert!(
			tx.fee.unwrap() > Amount::ZERO,
			"On-chain receive should have rebalance fees after channel opening"
		);

		// Validate fee is reasonable (should be less than 5% of received amount for rebalance)
		let fee_ratio = tx.fee.unwrap().milli_sats() as f64 / recv_amt.milli_sats() as f64;
		assert!(
			fee_ratio < 0.05,
			"Rebalance fee should be less than 5% of received amount, got {:.2}%",
			fee_ratio * 100.0
		);
	})
}

#[test]
fn test_pay_lightning_from_self_custody() {
	let TestParams { wallet, lsp: _, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// get a channel so we can make a payment
		open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		// wait for sync
		generate_blocks(&bitcoind, 6);
		tokio::time::sleep(Duration::from_secs(5)).await;

		let starting_bal = wallet.get_balance().await;

		let amount = Amount::from_sats(1_000).unwrap();

		// get invoice from third party node
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let invoice =
			third_party.bolt11_payment().receive(amount.milli_sats(), &desc, 300).unwrap();

		let instr = wallet.parse_payment_instructions(invoice.to_string().as_str()).await.unwrap();
		let info = PaymentInfo::build(instr, amount).unwrap();
		wallet.pay(&info).await.unwrap();

		let event = wait_next_event(&wallet).await;
		assert!(matches!(event, Event::PaymentSuccessful { .. }));
		assert_eq!(wallet.next_event(), None);

		// check the payment is correct
		let payments = wallet.list_transactions().await.unwrap();
		let payment = payments.into_iter().find(|p| p.outbound).unwrap();

		// Comprehensive validation for outgoing lightning bolt11 payment
		assert_eq!(payment.amount, Some(amount), "Amount should equal sent amount");
		assert!(
			payment.fee.is_some_and(|f| f > Amount::ZERO),
			"Lightning payment should have non-zero fees"
		);
		assert!(payment.outbound, "Outgoing payment should be outbound");
		assert!(
			matches!(payment.payment_type, PaymentType::OutgoingLightningBolt11 { .. }),
			"Payment type should be OutgoingLightningBolt11"
		);
		assert_eq!(payment.status, TxStatus::Completed, "Payment should be completed");
		assert_ne!(payment.time_since_epoch, Duration::ZERO, "Time should be set");

		// Validate fee is reasonable
		let fee_ratio = payment.fee.unwrap().milli_sats() as f64 / amount.milli_sats() as f64;
		assert!(
			fee_ratio < 0.1,
			"Fee should be less than 10% of sent amount, got {:.2}%",
			fee_ratio * 100.0
		);

		// Check that payment_type contains payment_preimage for completed payments
		if let PaymentType::OutgoingLightningBolt11 { payment_preimage } = &payment.payment_type {
			assert!(payment_preimage.is_some(), "Completed payment should have payment_preimage");
		}

		// check balance left our wallet
		let bal = wallet.get_balance().await;
		assert_eq!(bal.pending_balance, Amount::ZERO);
		assert!(bal.available_balance <= starting_bal.available_balance.saturating_sub(amount));

		// make sure 3rd party node got payment
		let payments = third_party.list_payments();
		assert!(payments.iter().any(|p| p.status == PaymentStatus::Succeeded
			&& p.direction == PaymentDirection::Inbound
			&& p.amount_msat == Some(amount.milli_sats())));
	})
}

#[test]
fn test_pay_bolt12_from_self_custody() {
	let TestParams { wallet, lsp: _, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// get a channel so we can make a payment
		open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		// wait for sync
		generate_blocks(&bitcoind, 6);
		tokio::time::sleep(Duration::from_secs(5)).await;

		let starting_bal = wallet.get_balance().await;

		let amount = Amount::from_sats(1_000).unwrap();

		// get offer from third party node
		let offer =
			third_party.bolt12_payment().receive(amount.milli_sats(), "test", None, None).unwrap();

		let instr = wallet.parse_payment_instructions(offer.to_string().as_str()).await.unwrap();
		let info = PaymentInfo::build(instr, amount).unwrap();
		wallet.pay(&info).await.unwrap();

		let event = wait_next_event(&wallet).await;
		assert!(matches!(event, Event::PaymentSuccessful { .. }));
		assert_eq!(wallet.next_event(), None);

		// check the payment is correct
		let payments = wallet.list_transactions().await.unwrap();
		let payment = payments.into_iter().find(|p| p.outbound).unwrap();

		// Comprehensive validation for outgoing lightning bolt12 payment
		assert_eq!(payment.amount, Some(amount), "Amount should equal sent amount");
		assert!(
			payment.fee.is_some_and(|f| f > Amount::ZERO),
			"Lightning payment should have non-zero fees"
		);
		assert!(payment.outbound, "Outgoing payment should be outbound");
		assert!(
			matches!(payment.payment_type, PaymentType::OutgoingLightningBolt12 { .. }),
			"Payment type should be OutgoingLightningBolt12"
		);
		assert_eq!(payment.status, TxStatus::Completed, "Payment should be completed");
		assert_ne!(payment.time_since_epoch, Duration::ZERO, "Time should be set");

		// Validate fee is reasonable
		let fee_ratio = payment.fee.unwrap().milli_sats() as f64 / amount.milli_sats() as f64;
		assert!(
			fee_ratio < 0.1,
			"Fee should be less than 10% of sent amount, got {:.2}%",
			fee_ratio * 100.0
		);

		// Check that payment_type contains payment_preimage for completed payments
		if let PaymentType::OutgoingLightningBolt12 { payment_preimage } = &payment.payment_type {
			assert!(payment_preimage.is_some(), "Completed payment should have payment_preimage");
		}

		// check balance left our wallet
		let bal = wallet.get_balance().await;
		assert_eq!(bal.pending_balance, Amount::ZERO);
		assert!(bal.available_balance <= starting_bal.available_balance.saturating_sub(amount));

		// make sure 3rd party node got payment
		let payments = third_party.list_payments();
		assert!(payments.iter().any(|p| p.status == PaymentStatus::Succeeded
			&& p.direction == PaymentDirection::Inbound
			&& p.amount_msat == Some(amount.milli_sats())));
	})
}

#[test]
fn test_pay_onchain_from_self_custody() {
	let TestParams { wallet, lsp: _, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// disable rebalancing so we have on-chain funds
		wallet.set_rebalance_enabled(false);

		let starting_bal = wallet.get_balance().await;
		assert_eq!(starting_bal.available_balance, Amount::ZERO);
		assert_eq!(starting_bal.pending_balance, Amount::ZERO);

		// fund wallet with on-chain
		let recv_amount = Amount::from_sats(1_000_000).unwrap();
		let uri = wallet.get_single_use_receive_uri(Some(recv_amount)).await.unwrap();
		bitcoind
			.client
			.send_to_address(
				&uri.address.unwrap(),
				ldk_node::bitcoin::Amount::from_sat(recv_amount.sats().unwrap()),
			)
			.unwrap();

		// confirm tx
		generate_blocks(&bitcoind, 6);

		// wait for node to sync and see the balance update
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wallet sync after on-chain receive",
			|| async { wallet.get_balance().await.pending_balance > starting_bal.pending_balance },
		)
		.await
		.expect("Wallet did not sync balance in time");

		// get address from third party node
		let addr = third_party.onchain_payment().new_address().unwrap();
		let send_amount = Amount::from_sats(100_000).unwrap();

		let instr = wallet.parse_payment_instructions(addr.to_string().as_str()).await.unwrap();
		let info = PaymentInfo::build(instr, send_amount).unwrap();
		wallet.pay(&info).await.unwrap();

		// confirm the tx
		generate_blocks(&bitcoind, 6);

		// wait for payment to complete
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"on-chain payment completion",
			|| async {
				let payments = wallet.list_transactions().await.unwrap();
				let payment = payments.into_iter().find(|p| p.outbound);
				if payment.as_ref().is_some_and(|p| p.status == TxStatus::Failed) {
					panic!("Payment failed");
				}
				payment.is_some_and(|p| p.status == TxStatus::Completed)
			},
		)
		.await
		.expect("Payment did not complete in time");

		// check the payment is correct
		let payments = wallet.list_transactions().await.unwrap();
		let payment = payments.into_iter().find(|p| p.outbound).unwrap();

		// Comprehensive validation for outgoing on-chain payment
		assert_eq!(payment.amount, Some(send_amount), "Amount should equal sent amount");
		assert!(
			payment.fee.is_some_and(|f| f > Amount::ZERO),
			"On-chain payment should have non-zero fees"
		);
		assert!(payment.outbound, "Outgoing payment should be outbound");
		assert!(
			matches!(payment.payment_type, PaymentType::OutgoingOnChain { .. }),
			"Payment type should be OutgoingOnChain"
		);
		assert_eq!(payment.status, TxStatus::Completed, "Payment should be completed");
		assert_ne!(payment.time_since_epoch, Duration::ZERO, "Time should be set");

		// Validate fee is reasonable for on-chain (should be less than 1% of sent amount)
		let fee_ratio = payment.fee.unwrap().milli_sats() as f64 / send_amount.milli_sats() as f64;
		assert!(
			fee_ratio < 0.01,
			"On-chain fee should be less than 1% of sent amount, got {:.2}%",
			fee_ratio * 100.0
		);

		// Check that payment_type contains txid for completed payments
		if let PaymentType::OutgoingOnChain { txid } = &payment.payment_type {
			assert!(txid.is_some(), "Completed on-chain payment should have txid");
		}

		// check balance left our wallet
		let bal = wallet.get_balance().await;
		assert_eq!(
			bal.pending_balance,
			recv_amount.saturating_sub(send_amount).saturating_sub(payment.fee.unwrap())
		);

		// Wait for third party node to receive it
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"on-chain payment received",
			|| async {
				let payments = third_party.list_payments();
				payments.iter().any(|p| {
					p.status == PaymentStatus::Succeeded
						&& p.direction == PaymentDirection::Inbound
						&& p.amount_msat == Some(send_amount.milli_sats())
				})
			},
		)
		.await
		.expect("Payment did not complete in time");
	})
}

#[test]
fn test_force_close_handling() {
	let TestParams { wallet, lsp, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let starting_bal = wallet.get_balance().await;
		assert_eq!(starting_bal.available_balance, Amount::ZERO);
		assert_eq!(starting_bal.pending_balance, Amount::ZERO);

		let rebalancing = wallet.get_rebalance_enabled();
		assert!(rebalancing);

		// get a channel so we can make a payment
		open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		// mine some blocks to ensure the channel is confirmed
		generate_blocks(&bitcoind, 6);

		// get channel details
		let channel = lsp
			.list_channels()
			.into_iter()
			.find(|c| c.counterparty_node_id == wallet.node_id())
			.unwrap();

		// force close the channel
		lsp.force_close_channel(&channel.user_channel_id, channel.counterparty_node_id, None)
			.unwrap();

		// wait for the channel to be closed
		let event = wait_next_event(&wallet).await;
		match event {
			Event::ChannelClosed { counterparty_node_id, .. } => {
				assert_eq!(counterparty_node_id, lsp.node_id());
			},
			_ => panic!("Expected ChannelClosed event"),
		}

		// rebalancing should be disabled after a force close
		let rebalancing = wallet.get_rebalance_enabled();
		assert!(!rebalancing);
	})
}

#[test]
fn test_close_all_channels() {
	let TestParams { wallet, lsp, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let starting_bal = wallet.get_balance().await;
		assert_eq!(starting_bal.available_balance, Amount::ZERO);
		assert_eq!(starting_bal.pending_balance, Amount::ZERO);

		let rebalancing = wallet.get_rebalance_enabled();
		assert!(rebalancing);

		// get a channel so we can make a payment
		open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		// mine some blocks to ensure the channel is confirmed
		generate_blocks(&bitcoind, 6);

		// init closing all channels
		wallet.close_channels().unwrap();

		// wait for the channels to be closed
		let event = wait_next_event(&wallet).await;
		match event {
			Event::ChannelClosed { counterparty_node_id, .. } => {
				assert_eq!(counterparty_node_id, lsp.node_id());
			},
			_ => panic!("Expected ChannelClosed event"),
		}

		// rebalancing should be disabled after closing all channels
		let rebalancing = wallet.get_rebalance_enabled();
		assert!(!rebalancing);
	})
}
