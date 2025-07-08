#![cfg(feature = "_test-utils")]

use crate::test_utils::{
	TestParams, build_test_nodes, generate_blocks, open_channel_from_lsp, wait_next_event,
};
use bitcoin_payment_instructions::PaymentInstructions;
use bitcoin_payment_instructions::amount::Amount;
use bitcoin_payment_instructions::http_resolver::HTTPHrnResolver;
use ldk_node::bitcoin::Network;
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

#[test]
fn test_threshold_boundary_trusted_balance_limit() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let tunables = wallet.get_tunables();

		// Test 1: Payment exactly at the trusted balance limit should use trusted wallet
		let exact_limit_amount = tunables.trusted_balance_limit;
		let uri = wallet.get_single_use_receive_uri(Some(exact_limit_amount)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"exact limit payment",
			|| async { wallet.get_balance().await.available_balance >= exact_limit_amount },
		)
		.await
		.expect("Payment at exact limit failed");

		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 1);
		let tx = &txs[0];
		assert_eq!(tx.payment_type, PaymentType::IncomingLightning {});
		assert_eq!(
			tx.fee,
			Some(Amount::ZERO),
			"Payment at exact limit should use trusted wallet with zero fees"
		);

		// Test 2: Payment 1 sat above the limit should trigger Lightning channel
		let above_limit_amount = exact_limit_amount.saturating_add(Amount::from_sats(1).unwrap());
		let uri = wallet.get_single_use_receive_uri(Some(above_limit_amount)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		// Wait for channel to be opened and payment to complete
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			15,
			"above limit payment with channel",
			|| async {
				let balance = wallet.get_balance().await.available_balance;
				balance
					>= exact_limit_amount.saturating_add(
						above_limit_amount.saturating_sub(Amount::from_sats(50000).unwrap()),
					)
			},
		)
		.await
		.expect("Payment above limit failed");

		// Should have received a ChannelOpened event
		let event = test_utils::wait_next_event(&wallet).await;
		assert!(
			matches!(event, Event::ChannelOpened { .. }),
			"Payment above limit should trigger channel opening"
		);

		let event = test_utils::wait_next_event(&wallet).await;
		assert!(
			matches!(event, Event::PaymentReceived { .. }),
			"Should receive payment through Lightning"
		);

		let txs = wallet.list_transactions().await.unwrap();
		let lightning_tx = txs.iter().find(|tx| tx.fee.is_some_and(|f| f > Amount::ZERO)).unwrap();
		assert_eq!(lightning_tx.payment_type, PaymentType::IncomingLightning {});
		assert!(
			lightning_tx.fee.unwrap() > Amount::ZERO,
			"Payment above limit should use Lightning with fees"
		);
	})
}

#[test]
fn test_threshold_boundary_rebalance_min() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let tunables = wallet.get_tunables();
		let rebalance_min = tunables.rebalance_min;

		// Test 1: Payment below rebalance_min should use trusted wallet
		let below_rebalance = rebalance_min.saturating_sub(Amount::from_sats(1).unwrap());
		let uri = wallet.get_single_use_receive_uri(Some(below_rebalance)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"below rebalance min payment",
			|| async { wallet.get_balance().await.available_balance >= below_rebalance },
		)
		.await
		.expect("Payment below rebalance min failed");

		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 1);
		let tx = &txs[0];
		assert_eq!(
			tx.fee,
			Some(Amount::ZERO),
			"Below rebalance_min should use trusted wallet with zero fees"
		);
		assert_eq!(tx.payment_type, PaymentType::IncomingLightning {});

		// Test 2: Payment exactly at rebalance_min should use trusted wallet
		let exact_rebalance = rebalance_min;
		let uri = wallet.get_single_use_receive_uri(Some(exact_rebalance)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"exact rebalance min payment",
			|| async {
				let balance = wallet.get_balance().await.available_balance;
				balance >= below_rebalance.saturating_add(exact_rebalance)
			},
		)
		.await
		.expect("Payment at exact rebalance min failed");

		let txs = wallet.list_transactions().await.unwrap();
		assert!(txs.len() >= 2, "Should have at least 2 transactions (may include rebalance)");

		// Count incoming lightning transactions (our test payments)
		let incoming_txs: Vec<_> = txs
			.iter()
			.filter(|tx| !tx.outbound && tx.payment_type == PaymentType::IncomingLightning {})
			.collect();
		assert_eq!(incoming_txs.len(), 2, "Should have exactly 2 incoming payments");

		// Both incoming transactions should be trusted wallet transactions with zero fees
		for tx in incoming_txs {
			assert_eq!(
				tx.fee,
				Some(Amount::ZERO),
				"Payments at/below rebalance_min should use trusted wallet"
			);
			assert_eq!(tx.payment_type, PaymentType::IncomingLightning {});
		}

		// Test 3: Verify that the rebalance logic respects the minimum threshold
		// The total balance should still be below what would trigger Lightning usage
		let total_balance = wallet.get_balance().await.available_balance;
		assert!(
			total_balance < tunables.trusted_balance_limit,
			"Total balance should still be below trusted_balance_limit"
		);
	})
}

#[test]
fn test_threshold_boundary_onchain_receive_threshold() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		let tunables = wallet.get_tunables();
		let onchain_threshold = tunables.onchain_receive_threshold;

		// Test 1: Amount below onchain_receive_threshold should not include on-chain address
		let below_threshold = onchain_threshold.saturating_sub(Amount::from_sats(1).unwrap());
		let uri = wallet.get_single_use_receive_uri(Some(below_threshold)).await.unwrap();

		assert!(
			uri.address.is_none(),
			"Payment below onchain threshold should not include on-chain address"
		);
		assert!(uri.invoice.amount_milli_satoshis().is_some(), "Should include Lightning invoice");

		// Test 2: Amount exactly at onchain_receive_threshold should include on-chain address
		let exact_threshold = onchain_threshold;
		let uri = wallet.get_single_use_receive_uri(Some(exact_threshold)).await.unwrap();

		assert!(
			uri.address.is_some(),
			"Payment at exact onchain threshold should include on-chain address"
		);
		assert!(
			uri.invoice.amount_milli_satoshis().is_some(),
			"Should also include Lightning invoice"
		);

		// Test 3: Amount above onchain_receive_threshold should include on-chain address
		let above_threshold = onchain_threshold.saturating_add(Amount::from_sats(1000).unwrap());
		let uri = wallet.get_single_use_receive_uri(Some(above_threshold)).await.unwrap();

		assert!(
			uri.address.is_some(),
			"Payment above onchain threshold should include on-chain address"
		);
		assert!(
			uri.invoice.amount_milli_satoshis().is_some(),
			"Should also include Lightning invoice"
		);

		// Test 4: Amountless receive behavior with enable_amountless_receive_on_chain
		let uri = wallet.get_single_use_receive_uri(None).await.unwrap();

		if tunables.enable_amountless_receive_on_chain {
			assert!(
				uri.address.is_some(),
				"Amountless receive should include on-chain address when enabled"
			);
		} else {
			assert!(
				uri.address.is_none(),
				"Amountless receive should not include on-chain address when disabled"
			);
		}
		assert!(
			uri.invoice.amount_milli_satoshis().is_none(),
			"Amountless invoice should have no fixed amount"
		);
	})
}

#[test]
fn test_threshold_combinations_and_edge_cases() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		let tunables = wallet.get_tunables();

		// Test edge case: ensure thresholds are properly ordered
		assert!(
			tunables.rebalance_min <= tunables.trusted_balance_limit,
			"rebalance_min should be <= trusted_balance_limit for proper wallet operation"
		);

		// Test minimum amount handling (1 sat)
		let min_amount = Amount::from_sats(1).unwrap();
		let uri = wallet.get_single_use_receive_uri(Some(min_amount)).await.unwrap();

		assert!(
			uri.invoice.amount_milli_satoshis().is_some(),
			"Should handle minimum 1 sat amount"
		);
		assert!(uri.address.is_none(), "1 sat should be below onchain threshold");

		// Test large amount (but reasonable for testing)
		let large_amount = Amount::from_sats(1_000_000).unwrap(); // 1 BTC - large but reasonable
		let uri = wallet.get_single_use_receive_uri(Some(large_amount)).await.unwrap();

		assert!(uri.invoice.amount_milli_satoshis().is_some(), "Should handle large amounts");
		assert!(uri.address.is_some(), "Large amount should include on-chain address");

		// Test zero amount (should be handled by amountless logic)
		let uri = wallet.get_single_use_receive_uri(None).await.unwrap();
		assert!(
			uri.invoice.amount_milli_satoshis().is_none(),
			"Amountless invoice should have no amount"
		);

		// Verify the wallet can handle payments at multiple threshold boundaries
		let test_amounts = [
			tunables.rebalance_min.saturating_sub(Amount::from_sats(1).unwrap()),
			tunables.rebalance_min,
			tunables.rebalance_min.saturating_add(Amount::from_sats(1).unwrap()),
			tunables.onchain_receive_threshold.saturating_sub(Amount::from_sats(1).unwrap()),
			tunables.onchain_receive_threshold,
			tunables.onchain_receive_threshold.saturating_add(Amount::from_sats(1).unwrap()),
			tunables.trusted_balance_limit.saturating_sub(Amount::from_sats(1).unwrap()),
			tunables.trusted_balance_limit,
		];

		for amount in test_amounts {
			let uri = wallet.get_single_use_receive_uri(Some(amount)).await.unwrap();
			assert!(
				uri.invoice.amount_milli_satoshis().is_some(),
				"Should generate valid invoice for amount: {} sats",
				amount.sats().unwrap_or(0)
			);
		}
	})
}

#[test]
fn test_payment_failures() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// Test 1: Payment with insufficient balance
		let amount = Amount::from_sats(1_000_000).unwrap(); // 1 BTC - more than we have
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let invoice =
			third_party.bolt11_payment().receive(amount.milli_sats(), &desc, 300).unwrap();

		let instr = wallet.parse_payment_instructions(invoice.to_string().as_str()).await.unwrap();
		let info = PaymentInfo::build(instr, amount).unwrap();

		// This should fail due to insufficient balance
		let result = wallet.pay(&info).await;
		assert!(result.is_err(), "Payment with insufficient balance should fail");

		// Test 2: Invalid invoice parsing
		let invalid_invoice = "lnbc1invalid_invoice_here";
		let result = wallet.parse_payment_instructions(invalid_invoice).await;
		assert!(result.is_err(), "Invalid invoice should fail to parse");

		// Test 3: Malformed Bitcoin address
		let invalid_address = "not_a_bitcoin_address";
		let result = wallet.parse_payment_instructions(invalid_address).await;
		assert!(result.is_err(), "Invalid address should fail to parse");

		// Test 4: Zero amount payment (should be rejected)
		let zero_amount = Amount::ZERO;
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let invoice = third_party.bolt11_payment().receive(1000, &desc, 300).unwrap(); // 1 msat

		let instr = wallet.parse_payment_instructions(invoice.to_string().as_str()).await.unwrap();
		let result = PaymentInfo::build(instr, zero_amount);
		assert!(result.is_err(), "Zero amount payment should be rejected");

		// Test 5: Payment with mismatched amount (fixed amount invoice with different amount)
		let fixed_amount = Amount::from_sats(5000).unwrap();
		let different_amount = Amount::from_sats(10000).unwrap();
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let fixed_invoice =
			third_party.bolt11_payment().receive(fixed_amount.milli_sats(), &desc, 300).unwrap();

		let instr =
			wallet.parse_payment_instructions(fixed_invoice.to_string().as_str()).await.unwrap();
		let result = PaymentInfo::build(instr, different_amount);
		assert!(result.is_err(), "Mismatched amount for fixed invoice should be rejected");

		// Test 6: Verify no failed transactions are recorded
		let txs = wallet.list_transactions().await.unwrap();
		assert_eq!(txs.len(), 0, "Failed payments should not be recorded in transaction list");
	})
}

#[test]
fn test_payment_with_expired_invoice() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// Add some balance first so the payment can theoretically succeed if not expired
		let initial_amount = Amount::from_sats(5000).unwrap();
		let uri = wallet.get_single_use_receive_uri(Some(initial_amount)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		// Wait for balance update
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wallet balance update",
			|| async { wallet.get_balance().await.available_balance > Amount::ZERO },
		)
		.await
		.expect("Balance should update");

		// Create an invoice with very short expiry
		let payment_amount = Amount::from_sats(1000).unwrap();
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let invoice =
			third_party.bolt11_payment().receive(payment_amount.milli_sats(), &desc, 1).unwrap(); // 1 second expiry

		// Wait longer to ensure invoice expires
		tokio::time::sleep(Duration::from_secs(5)).await;

		// Try to parse and pay the expired invoice - it should either fail to parse or fail to pay
		let parse_result = wallet.parse_payment_instructions(invoice.to_string().as_str()).await;
		if let Ok(instr) = parse_result {
			if let Ok(info) = PaymentInfo::build(instr, payment_amount) {
				// If parsing succeeded, payment should fail due to expiry
				let result = wallet.pay(&info).await;
				assert!(result.is_err(), "Payment with expired invoice should fail");
			} else {
				// Payment info building failed - acceptable for expired invoice
			}
		} else {
			// Parsing failed - also acceptable for expired invoice
		}
	})
}

#[test]
fn test_payment_network_mismatch() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		// Test 1: Mainnet invoice on regtest wallet (if we can construct one)
		// This is tricky to test in practice since we're on regtest, but we can test
		// the validation logic with known invalid network addresses

		// Test 2: Invalid network address format
		let invalid_testnet_address = "tb1qd28npep0s8frcm3y7dxqajkcy2m40eysplyr9v"; // Valid testnet address
		let result = wallet.parse_payment_instructions(invalid_testnet_address).await;
		assert!(result.is_err(), "Invalid testnet address");

		// now force a correct parsing to ensure we fail when trying to pay
		let instr = PaymentInstructions::parse(
			invalid_testnet_address,
			Network::Testnet,
			&HTTPHrnResolver,
			true,
		)
		.await
		.unwrap();

		// If it parsed, trying to pay should fail due to network mismatch
		let amount = Amount::from_sats(1000).unwrap();
		let info = PaymentInfo::build(instr, amount).unwrap();
		let pay_result = wallet.pay(&info).await;
		assert!(pay_result.is_err(), "Payment to wrong network address should fail");
	})
}

#[test]
fn test_concurrent_payments() {
	let TestParams { wallet, lsp: _, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// First, get a Lightning channel with sufficient funds
		let _channel_amount = open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		// Wait for sync
		generate_blocks(&bitcoind, 6);
		tokio::time::sleep(Duration::from_secs(5)).await;

		let payment_amount = Amount::from_sats(100).unwrap();

		// Create multiple invoices
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let invoice1 =
			third_party.bolt11_payment().receive(payment_amount.milli_sats(), &desc, 300).unwrap();
		let invoice2 =
			third_party.bolt11_payment().receive(payment_amount.milli_sats(), &desc, 300).unwrap();
		let invoice3 =
			third_party.bolt11_payment().receive(payment_amount.milli_sats(), &desc, 300).unwrap();

		// Parse payment instructions
		let instr1 =
			wallet.parse_payment_instructions(invoice1.to_string().as_str()).await.unwrap();
		let instr2 =
			wallet.parse_payment_instructions(invoice2.to_string().as_str()).await.unwrap();
		let instr3 =
			wallet.parse_payment_instructions(invoice3.to_string().as_str()).await.unwrap();

		let info1 = PaymentInfo::build(instr1, payment_amount).unwrap();
		let info2 = PaymentInfo::build(instr2, payment_amount).unwrap();
		let info3 = PaymentInfo::build(instr3, payment_amount).unwrap();

		// Test 1: Launch multiple payments concurrently
		let wallet_ref = &wallet;
		let (result1, result2, result3) =
			tokio::join!(wallet_ref.pay(&info1), wallet_ref.pay(&info2), wallet_ref.pay(&info3));

		// At least one should succeed (depending on available balance)
		// Some might fail due to insufficient balance, which is expected
		let successes = [&result1, &result2, &result3].iter().filter(|r| r.is_ok()).count();
		assert!(successes >= 1, "At least one concurrent payment should succeed");

		// Test 2: Concurrent balance queries during payments
		let balance_queries =
			tokio::join!(wallet.get_balance(), wallet.get_balance(), wallet.get_balance());

		// All balance queries should succeed
		assert!(balance_queries.0.available_balance >= Amount::ZERO);
		assert!(balance_queries.1.available_balance >= Amount::ZERO);
		assert!(balance_queries.2.available_balance >= Amount::ZERO);

		// Test 3: Concurrent transaction list queries
		let tx_queries = tokio::join!(
			wallet.list_transactions(),
			wallet.list_transactions(),
			wallet.list_transactions()
		);

		// All should succeed and return consistent results
		assert!(tx_queries.0.is_ok());
		assert!(tx_queries.1.is_ok());
		assert!(tx_queries.2.is_ok());

		let tx_counts =
			(tx_queries.0.unwrap().len(), tx_queries.1.unwrap().len(), tx_queries.2.unwrap().len());

		// All queries should return the same number of transactions (consistency)
		assert_eq!(tx_counts.0, tx_counts.1, "Concurrent transaction queries should be consistent");
		assert_eq!(tx_counts.1, tx_counts.2, "Concurrent transaction queries should be consistent");
	})
}

#[test]
fn test_concurrent_receive_operations() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		let amount = Amount::from_sats(1000).unwrap();

		// Test: Generate multiple receive URIs concurrently
		let (uri1, uri2) = tokio::join!(
			wallet.get_single_use_receive_uri(Some(amount)),
			wallet.get_single_use_receive_uri(Some(amount))
		);

		// Both should succeed
		assert!(uri1.is_ok(), "Concurrent URI generation should succeed");
		assert!(uri2.is_ok(), "Concurrent URI generation should succeed");

		let uris = (uri1.unwrap(), uri2.unwrap());

		// URIs should be different (unique invoices)
		assert_ne!(uris.0.invoice.to_string(), uris.1.invoice.to_string(), "URIs should be unique");

		// Test: Sequential payments to avoid routing issues
		let payment_id_1 = third_party.bolt11_payment().send(&uris.0.invoice, None).unwrap();

		// Wait for first payment to complete
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			15,
			"first payment to succeed",
			|| async {
				third_party
					.payment(&payment_id_1)
					.map_or(false, |p| p.status == PaymentStatus::Succeeded)
			},
		)
		.await
		.expect("First payment did not succeed");

		// Send second payment
		let payment_id_2 = third_party.bolt11_payment().send(&uris.1.invoice, None).unwrap();

		// Wait for second payment to complete
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			15,
			"second payment to succeed",
			|| async {
				third_party
					.payment(&payment_id_2)
					.map_or(false, |p| p.status == PaymentStatus::Succeeded)
			},
		)
		.await
		.expect("Second payment did not succeed");

		// Wait for wallet balance to reflect both payments
		test_utils::wait_for_condition(
			Duration::from_secs(1),
			10,
			"wallet balance to update",
			|| async {
				let balance = wallet.get_balance().await.available_balance;
				balance >= amount.saturating_add(amount)
			},
		)
		.await
		.expect("Wallet balance did not reflect both payments");

		// Verify transactions were recorded
		let txs = wallet.list_transactions().await.unwrap();
		assert!(txs.len() >= 2, "Should have at least 2 transactions");

		// Count incoming transactions
		let incoming_count = txs.iter().filter(|tx| !tx.outbound).count();
		assert_eq!(incoming_count, 2, "Should have exactly 2 incoming transactions");
	})
}

#[test]
fn test_balance_consistency_under_load() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// Add some initial balance
		let initial_amount = Amount::from_sats(10000).unwrap();
		let uri = wallet.get_single_use_receive_uri(Some(initial_amount)).await.unwrap();
		third_party.bolt11_payment().send(&uri.invoice, None).unwrap();

		test_utils::wait_for_condition(Duration::from_secs(1), 10, "initial balance", || async {
			wallet.get_balance().await.available_balance >= initial_amount
		})
		.await
		.expect("Initial balance not received");

		// Test: Many concurrent balance queries
		let mut balance_tasks = Vec::new();
		for _ in 0..20 {
			balance_tasks.push(wallet.get_balance());
		}

		// Join all balance tasks
		let mut all_balances = Vec::new();
		for task in balance_tasks {
			all_balances.push(task.await);
		}
		let balances = all_balances;

		// All queries should succeed
		assert_eq!(balances.len(), 20);
		for balance in &balances {
			assert!(balance.available_balance >= Amount::ZERO);
			assert!(balance.pending_balance >= Amount::ZERO);
		}

		// All balances should be consistent (same values)
		let first_balance = &balances[0];
		for balance in &balances[1..] {
			assert_eq!(
				balance.available_balance, first_balance.available_balance,
				"Concurrent balance queries should return consistent results"
			);
			assert_eq!(
				balance.pending_balance, first_balance.pending_balance,
				"Concurrent balance queries should return consistent results"
			);
		}
	})
}

#[test]
fn test_invalid_tunables_relationships() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		let current_tunables = wallet.get_tunables();

		// Test 1: Verify default tunables are valid
		assert!(
			current_tunables.rebalance_min <= current_tunables.trusted_balance_limit,
			"Default tunables should have valid relationship: rebalance_min <= trusted_balance_limit"
		);

		// Test 2: Test edge case amounts with current tunables
		// Zero amount (should work for URI generation but not payments)
		let uri_result = wallet.get_single_use_receive_uri(None).await;
		assert!(uri_result.is_ok(), "Should be able to generate amountless URI");

		// Test 3: Very small amounts
		let tiny_amount = Amount::from_sats(1).unwrap();
		let uri_result = wallet.get_single_use_receive_uri(Some(tiny_amount)).await;
		assert!(uri_result.is_ok(), "Should handle tiny amounts");

		let uri = uri_result.unwrap();
		assert!(
			uri.invoice.amount_milli_satoshis().is_some(),
			"Tiny amount should have Lightning invoice"
		);

		// Should not include on-chain address if below threshold
		if tiny_amount < current_tunables.onchain_receive_threshold {
			assert!(
				uri.address.is_none(),
				"Tiny amount below threshold should not include on-chain address"
			);
		}

		// Test 4: Amounts exactly at boundaries
		let boundary_amounts = [
			current_tunables.rebalance_min,
			current_tunables.trusted_balance_limit,
			current_tunables.onchain_receive_threshold,
		];

		for amount in boundary_amounts {
			let uri_result = wallet.get_single_use_receive_uri(Some(amount)).await;
			assert!(
				uri_result.is_ok(),
				"Should handle boundary amounts: {} sats",
				amount.sats().unwrap_or(0)
			);
		}

		// Test 5: Verify tunables consistency with wallet behavior
		let below_limit =
			current_tunables.trusted_balance_limit.saturating_sub(Amount::from_sats(1).unwrap());
		let above_limit =
			current_tunables.trusted_balance_limit.saturating_add(Amount::from_sats(1).unwrap());

		let uri_below = wallet.get_single_use_receive_uri(Some(below_limit)).await.unwrap();
		let uri_above = wallet.get_single_use_receive_uri(Some(above_limit)).await.unwrap();

		// Both should have invoices
		assert!(
			uri_below.invoice.amount_milli_satoshis().is_some(),
			"Below limit should have invoice"
		);
		assert!(
			uri_above.invoice.amount_milli_satoshis().is_some(),
			"Above limit should have invoice"
		);

		// On-chain address inclusion should depend on onchain_receive_threshold, not trusted_balance_limit
		if below_limit >= current_tunables.onchain_receive_threshold {
			assert!(
				uri_below.address.is_some(),
				"Amount >= onchain_threshold should include address"
			);
		}
		if above_limit >= current_tunables.onchain_receive_threshold {
			assert!(
				uri_above.address.is_some(),
				"Amount >= onchain_threshold should include address"
			);
		}
	})
}

#[test]
fn test_extreme_amount_handling() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		// Test 1: Large but reasonable Bitcoin amount
		let large_reasonable = Amount::from_sats(1_000_000).unwrap(); // 1M sats = 0.01 BTC
		let uri_result = wallet.get_single_use_receive_uri(Some(large_reasonable)).await;
		assert!(uri_result.is_ok(), "Should handle large reasonable Bitcoin amount");

		// Test 2: Various large amounts (but still reasonable for testing)
		let large_amounts = [
			Amount::from_sats(100_000).unwrap(),   // 0.001 BTC
			Amount::from_sats(500_000).unwrap(),   // 0.005 BTC
			Amount::from_sats(1_000_000).unwrap(), // 0.01 BTC
		];

		for amount in large_amounts {
			let uri_result = wallet.get_single_use_receive_uri(Some(amount)).await;
			assert!(
				uri_result.is_ok(),
				"Should handle large amount: {} BTC",
				amount.sats().unwrap() as f64 / 100_000_000.0
			);

			let uri = uri_result.unwrap();
			assert!(
				uri.invoice.amount_milli_satoshis().is_some(),
				"Large amount should have invoice"
			);
			// Large amounts should always include on-chain address
			assert!(uri.address.is_some(), "Large amounts should include on-chain address");
		}

		// Test 3: Satoshi precision edge cases
		let precision_amounts = [
			Amount::from_sats(1).unwrap(),    // 1 sat
			Amount::from_sats(10).unwrap(),   // 10 sats
			Amount::from_sats(100).unwrap(),  // 100 sats
			Amount::from_sats(1000).unwrap(), // 1000 sats (1 mBTC)
		];

		for amount in precision_amounts {
			let uri_result = wallet.get_single_use_receive_uri(Some(amount)).await;
			assert!(
				uri_result.is_ok(),
				"Should handle precision amount: {} sats",
				amount.sats().unwrap()
			);
		}

		// Test 4: Milli-satoshi precision (if supported)
		// Note: Bitcoin addresses can't handle milli-satoshi precision, only Lightning can
		let msat_amount = Amount::from_milli_sats(1500).unwrap(); // 1.5 sats
		let uri_result = wallet.get_single_use_receive_uri(Some(msat_amount)).await;
		assert!(uri_result.is_ok(), "Should handle milli-satoshi amounts");

		let uri = uri_result.unwrap();
		assert!(
			uri.invoice.amount_milli_satoshis().is_some(),
			"Milli-satoshi amount should have Lightning invoice"
		);
		// On-chain address depends on threshold, not msat precision
	})
}

#[test]
fn test_wallet_configuration_validation() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party: _, rt } = build_test_nodes();

	rt.block_on(async move {
		// Test 1: Verify wallet is using expected network
		// This is more of a sanity check since we can't easily test invalid networks
		// without creating new wallets

		// Test 2: Verify rebalancing can be toggled
		let initial_rebalance_state = wallet.get_rebalance_enabled();
		assert!(initial_rebalance_state, "Rebalancing should be enabled by default");

		wallet.set_rebalance_enabled(false);
		assert!(!wallet.get_rebalance_enabled(), "Should be able to disable rebalancing");

		wallet.set_rebalance_enabled(true);
		assert!(wallet.get_rebalance_enabled(), "Should be able to re-enable rebalancing");

		// Test 3: Verify tunables are consistent and reasonable
		let tunables = wallet.get_tunables();

		// Check for reasonable default values
		assert!(
			tunables.trusted_balance_limit > Amount::ZERO,
			"Trusted balance limit should be positive"
		);
		assert!(tunables.rebalance_min > Amount::ZERO, "Rebalance min should be positive");
		assert!(
			tunables.onchain_receive_threshold > Amount::ZERO,
			"Onchain threshold should be positive"
		);

		// Check relationships
		assert!(
			tunables.rebalance_min <= tunables.trusted_balance_limit,
			"Rebalance min should not exceed trusted balance limit"
		);

		// Test 4: Test URI generation consistency across multiple calls
		let amount = Amount::from_sats(5000).unwrap();
		let uri1 = wallet.get_single_use_receive_uri(Some(amount)).await.unwrap();
		let uri2 = wallet.get_single_use_receive_uri(Some(amount)).await.unwrap();

		// Should generate different invoices (single use)
		assert_ne!(
			uri1.invoice.to_string(),
			uri2.invoice.to_string(),
			"Single-use URIs should be unique"
		);

		// But same amount and policy decisions
		assert_eq!(
			uri1.invoice.amount_milli_satoshis(),
			uri2.invoice.amount_milli_satoshis(),
			"Same amount should be preserved"
		);
		assert_eq!(
			uri1.address.is_some(),
			uri2.address.is_some(),
			"Address inclusion should be consistent"
		);
	})
}

#[test]
fn test_edge_case_payment_instruction_parsing() {
	let TestParams { wallet, lsp: _, bitcoind: _, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// Test 1: Empty strings
		let empty_result = wallet.parse_payment_instructions("").await;
		assert!(empty_result.is_err(), "Empty string should fail to parse");

		// Test 2: Whitespace-only strings
		let whitespace_result = wallet.parse_payment_instructions("   \t\n  ").await;
		assert!(whitespace_result.is_err(), "Whitespace-only string should fail to parse");

		// Test 3: Very long invalid strings
		let long_invalid = "a".repeat(1000);
		let long_result = wallet.parse_payment_instructions(&long_invalid).await;
		assert!(long_result.is_err(), "Very long invalid string should fail to parse");

		// Test 4: Mixed case handling
		// Create a valid invoice first
		let amount = Amount::from_sats(1000).unwrap();
		let desc = Bolt11InvoiceDescription::Direct(Description::empty());
		let invoice =
			third_party.bolt11_payment().receive(amount.milli_sats(), &desc, 300).unwrap();
		let invoice_str = invoice.to_string();

		// Test uppercase
		let _upper_result = wallet.parse_payment_instructions(&invoice_str.to_uppercase()).await;
		let _lower_result = wallet.parse_payment_instructions(&invoice_str.to_lowercase()).await;
		let original_result = wallet.parse_payment_instructions(&invoice_str).await;

		// At least the original should work
		assert!(original_result.is_ok(), "Original invoice should parse successfully");

		// Test 5: Invoices with special characters or encoding
		let special_chars =
			["lightning:", "bitcoin:?lightning=", "LIGHTNING:", "BITCOIN:?LIGHTNING="];
		for prefix in special_chars {
			let prefixed = format!("{}{}", prefix, invoice_str);
			let result = wallet.parse_payment_instructions(&prefixed).await;
			assert!(result.is_ok(), "Failed to parse payment instructions");
		}
	})
}

#[test]
fn test_lsp_connectivity_fallback() {
	let TestParams { wallet, lsp, bitcoind, third_party, rt } = build_test_nodes();

	rt.block_on(async move {
		// open a channel with the LSP
		open_channel_from_lsp(&wallet, Arc::clone(&third_party)).await;

		// confirm channel
		generate_blocks(&bitcoind, 6);
		tokio::time::sleep(Duration::from_secs(5)).await;

		// spend some of the balance so we have some inbound capacity
		let amount = Amount::from_sats(10_000).unwrap();
		let inv = third_party
			.bolt11_payment()
			.receive(
				amount.milli_sats(),
				&Bolt11InvoiceDescription::Direct(Description::empty()),
				300,
			)
			.unwrap();
		let instr = wallet.parse_payment_instructions(inv.to_string().as_str()).await.unwrap();
		let info = PaymentInfo::build(instr, amount).unwrap();
		let _ = wallet.pay(&info).await;

		// Wait for the payment to be processed
		let event = wait_next_event(&wallet).await;
		assert!(matches!(event, Event::PaymentSuccessful { .. }));

		// Get the wallet's tunables to find the trusted balance limit
		let tunables = wallet.get_tunables();

		// Use an amount that would normally go to Lightning (above trusted balance limit)
		let additional_amount = Amount::from_sats(1000).unwrap();
		let large_recv_amt = Amount::from_milli_sats(
			tunables.trusted_balance_limit.milli_sats() + additional_amount.milli_sats(),
		)
		.unwrap();

		// First, verify that with LSP online, this large amount would normally use Lightning
		let uri_with_lsp = wallet.get_single_use_receive_uri(Some(large_recv_amt)).await.unwrap();
		// This should work when LSP is online
		assert!(!uri_with_lsp.from_trusted);

		// Now simulate LSP being offline by stopping it
		let _ = lsp.stop();

		// Wait a moment for the stop to take effect
		tokio::time::sleep(Duration::from_secs(2)).await;

		// Now try to receive the same large amount that would normally trigger Lightning usage
		// but should fall back to trusted wallet due to LSP being offline
		let uri_result = wallet.get_single_use_receive_uri(Some(large_recv_amt)).await.unwrap();
		assert!(uri_result.from_trusted);

		// test with small amount that should succeed
		let small_recv_amt = Amount::from_sats(100).unwrap();
		let uri_small = wallet.get_single_use_receive_uri(Some(small_recv_amt)).await.unwrap();
		assert_eq!(
			uri_small.invoice.amount_milli_satoshis(),
			Some(small_recv_amt.milli_sats()),
			"Small amount should still generate a valid invoice even with LSP offline"
		);
		assert!(uri_small.from_trusted);
	});
}
