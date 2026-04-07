// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Tests that test the creation of dual-funded channels in ChannelManager.

use crate::chain::chaininterface::ConfirmationTarget;
use crate::events::Event;
use crate::ln::functional_test_utils::*;
use crate::ln::funding::FundingTxInput;
use crate::ln::msgs::{BaseMessageHandler, ChannelMessageHandler, MessageSendEvent};
use crate::util::config::UserConfig;
use crate::util::wallet_utils::WalletSourceSync;

use bitcoin::hashes::Hash;
use bitcoin::transaction::Version;
use bitcoin::{Amount, ScriptBuf, Transaction, TxOut, WPubkeyHash};

fn create_dual_funding_config() -> UserConfig {
	let mut config = test_default_channel_config();
	config.enable_dual_funded_channels = true;
	config
}

/// Creates a mock funding input (P2WPKH UTXO) with the given value in satoshis.
/// This uses a dummy script and cannot be signed by the test wallet.
fn create_dummy_funding_input(value_sats: u64) -> FundingTxInput {
	let prevout = TxOut {
		value: Amount::from_sat(value_sats),
		script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::all_zeros()),
	};
	let prevtx = Transaction {
		input: vec![],
		output: vec![prevout],
		version: Version::TWO,
		lock_time: bitcoin::absolute::LockTime::ZERO,
	};
	FundingTxInput::new_p2wpkh(prevtx, 0).unwrap()
}

/// Creates a funding input that can be signed by the given node's test wallet.
/// Also registers the UTXO with the wallet so `sign_tx` can find it.
fn create_wallet_funding_input<'a, 'b, 'c>(
	node: &Node<'a, 'b, 'c>, value_sats: u64,
) -> FundingTxInput {
	let script_pubkey = node.wallet_source.get_change_script().unwrap();
	let prevout = TxOut { value: Amount::from_sat(value_sats), script_pubkey };
	let prevtx = Transaction {
		input: vec![],
		output: vec![prevout],
		version: Version::TWO,
		lock_time: bitcoin::absolute::LockTime::ZERO,
	};
	node.wallet_source.add_utxo(prevtx.clone(), 0);
	FundingTxInput::new_p2wpkh(prevtx, 0).unwrap()
}

/// Drives the interactive tx negotiation loop between initiator and acceptor.
/// Returns when both sides have sent TxComplete.
fn drive_interactive_tx_negotiation<'a, 'b, 'c>(
	initiator: &Node<'a, 'b, 'c>, acceptor: &Node<'a, 'b, 'c>,
) {
	let node_id_initiator = initiator.node.get_our_node_id();
	let node_id_acceptor = acceptor.node.get_our_node_id();

	let mut initiator_sent_tx_complete;
	let mut acceptor_sent_tx_complete = false;
	loop {
		// Initiator's turn
		let msg_events = initiator.node.get_and_clear_pending_msg_events();
		assert_eq!(msg_events.len(), 1, "Expected exactly one message from initiator: {msg_events:?}");
		match &msg_events[0] {
			MessageSendEvent::SendTxAddInput { msg, .. } => {
				acceptor.node.handle_tx_add_input(node_id_initiator, msg);
				initiator_sent_tx_complete = false;
			},
			MessageSendEvent::SendTxAddOutput { msg, .. } => {
				acceptor.node.handle_tx_add_output(node_id_initiator, msg);
				initiator_sent_tx_complete = false;
			},
			MessageSendEvent::SendTxComplete { msg, .. } => {
				acceptor.node.handle_tx_complete(node_id_initiator, msg);
				initiator_sent_tx_complete = true;
				if acceptor_sent_tx_complete {
					break;
				}
			},
			_ => panic!("Unexpected initiator message: {:?}", msg_events[0]),
		}

		// Acceptor's turn
		let msg_events = acceptor.node.get_and_clear_pending_msg_events();
		assert_eq!(msg_events.len(), 1, "Expected exactly one message from acceptor: {msg_events:?}");
		match &msg_events[0] {
			MessageSendEvent::SendTxAddInput { msg, .. } => {
				initiator.node.handle_tx_add_input(node_id_acceptor, msg);
				acceptor_sent_tx_complete = false;
			},
			MessageSendEvent::SendTxAddOutput { msg, .. } => {
				initiator.node.handle_tx_add_output(node_id_acceptor, msg);
				acceptor_sent_tx_complete = false;
			},
			MessageSendEvent::SendTxComplete { msg, .. } => {
				initiator.node.handle_tx_complete(node_id_acceptor, msg);
				acceptor_sent_tx_complete = true;
				if initiator_sent_tx_complete {
					break;
				}
			},
			_ => panic!("Unexpected acceptor message: {:?}", msg_events[0]),
		}
	}
}

#[test]
fn test_v2_channel_open_send_open_channel_v2() {
	// Test that create_v2_channel sends an OpenChannelV2 message and that the counterparty
	// receives it and generates an OpenChannelRequest event.
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let dual_fund_config = create_dual_funding_config();
	let node_chanmgrs = create_node_chanmgrs(
		2,
		&node_cfgs,
		&[Some(dual_fund_config.clone()), Some(dual_fund_config.clone())],
	);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let funding_satoshis = 100_000;
	let funding_input = create_dummy_funding_input(funding_satoshis + 1_000); // extra for fees

	// Node 0 creates a v2 channel to node 1.
	let temp_channel_id = nodes[0]
		.node
		.create_v2_channel(
			nodes[1].node.get_our_node_id(),
			funding_satoshis,
			vec![funding_input],
			42, // user_channel_id
			None,
			ConfirmationTarget::NonAnchorChannelFee,
		)
		.unwrap();

	// Verify that an OpenChannelV2 message was queued.
	let msg_events = nodes[0].node.get_and_clear_pending_msg_events();
	assert_eq!(msg_events.len(), 1);
	let open_channel_v2_msg = match &msg_events[0] {
		MessageSendEvent::SendOpenChannelV2 { node_id, msg } => {
			assert_eq!(*node_id, nodes[1].node.get_our_node_id());
			assert_eq!(msg.common_fields.funding_satoshis, funding_satoshis);
			msg.clone()
		},
		_ => panic!("Expected SendOpenChannelV2, got {:?}", msg_events[0]),
	};

	// Node 1 handles the OpenChannelV2 message.
	nodes[1].node.handle_open_channel_v2(nodes[0].node.get_our_node_id(), &open_channel_v2_msg);

	// Node 1 should generate an OpenChannelRequest event.
	let events = nodes[1].node.get_and_clear_pending_events();
	assert_eq!(events.len(), 1);
	match &events[0] {
		Event::OpenChannelRequest { temporary_channel_id, counterparty_node_id, .. } => {
			assert_eq!(*counterparty_node_id, nodes[0].node.get_our_node_id());
			assert_eq!(*temporary_channel_id, temp_channel_id);
		},
		_ => panic!("Expected OpenChannelRequest, got {:?}", events[0]),
	};
}

#[test]
fn test_v2_channel_accept_and_interactive_tx_begins() {
	// Test the full flow through accept: open_channel_v2 -> accept_channel_v2 -> interactive tx
	// messages start flowing.
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let dual_fund_config = create_dual_funding_config();
	let node_chanmgrs = create_node_chanmgrs(
		2,
		&node_cfgs,
		&[Some(dual_fund_config.clone()), Some(dual_fund_config.clone())],
	);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let funding_satoshis = 100_000;
	let funding_input = create_dummy_funding_input(funding_satoshis + 1_000);

	// Node 0 creates a v2 channel.
	let _temp_channel_id = nodes[0]
		.node
		.create_v2_channel(
			nodes[1].node.get_our_node_id(),
			funding_satoshis,
			vec![funding_input],
			42,
			None,
			ConfirmationTarget::NonAnchorChannelFee,
		)
		.unwrap();

	// Get and deliver the OpenChannelV2.
	let open_msg = get_event_msg!(
		nodes[0],
		MessageSendEvent::SendOpenChannelV2,
		nodes[1].node.get_our_node_id()
	);
	nodes[1].node.handle_open_channel_v2(nodes[0].node.get_our_node_id(), &open_msg);

	// Node 1 receives OpenChannelRequest event and accepts with zero contribution.
	let events = nodes[1].node.get_and_clear_pending_events();
	assert_eq!(events.len(), 1);
	let temp_channel_id = match &events[0] {
		Event::OpenChannelRequest { temporary_channel_id, .. } => *temporary_channel_id,
		_ => panic!("Expected OpenChannelRequest"),
	};

	nodes[1]
		.node
		.accept_inbound_channel(&temp_channel_id, &nodes[0].node.get_our_node_id(), 43, None)
		.unwrap();

	// Node 1 should send AcceptChannelV2.
	let accept_msg = get_event_msg!(
		nodes[1],
		MessageSendEvent::SendAcceptChannelV2,
		nodes[0].node.get_our_node_id()
	);

	// Node 0 handles AcceptChannelV2 — this should start interactive tx.
	nodes[0].node.handle_accept_channel_v2(nodes[1].node.get_our_node_id(), &accept_msg);

	// Node 0 should have queued the first interactive tx message (TxAddInput, TxAddOutput, or
	// TxComplete).
	let msg_events = nodes[0].node.get_and_clear_pending_msg_events();
	assert!(!msg_events.is_empty(), "Expected interactive tx messages after accept_channel_v2");

	// Verify the message is an interactive tx message type.
	match &msg_events[0] {
		MessageSendEvent::SendTxAddInput { .. }
		| MessageSendEvent::SendTxAddOutput { .. }
		| MessageSendEvent::SendTxComplete { .. } => {},
		_ => panic!(
			"Expected interactive tx message (TxAddInput/TxAddOutput/TxComplete), got {:?}",
			msg_events[0]
		),
	}
}

#[test]
fn test_v2_feature_gate_disabled() {
	// Verify that dual-funded channels are rejected when the feature is disabled.
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	// Use default config (dual funding disabled).
	let node_chanmgrs = create_node_chanmgrs(2, &node_cfgs, &[None, None]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let funding_input = create_dummy_funding_input(101_000);

	// create_v2_channel should fail when dual funding is disabled.
	let result = nodes[0].node.create_v2_channel(
		nodes[1].node.get_our_node_id(),
		100_000,
		vec![funding_input],
		42,
		None,
		ConfirmationTarget::NonAnchorChannelFee,
	);
	assert!(result.is_err());
	match result.unwrap_err() {
		crate::util::errors::APIError::APIMisuseError { err } => {
			assert!(err.contains("Dual-funded channels are not enabled"), "Unexpected error: {}", err);
		},
		e => panic!("Expected APIMisuseError, got {:?}", e),
	}
}

#[test]
fn test_v2_channel_minimum_value() {
	// Verify that creating a v2 channel with less than 1000 sats is rejected.
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let dual_fund_config = create_dual_funding_config();
	let node_chanmgrs = create_node_chanmgrs(
		2,
		&node_cfgs,
		&[Some(dual_fund_config.clone()), Some(dual_fund_config.clone())],
	);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let funding_input = create_dummy_funding_input(2_000);

	let result = nodes[0].node.create_v2_channel(
		nodes[1].node.get_our_node_id(),
		999, // below minimum
		vec![funding_input],
		42,
		None,
		ConfirmationTarget::NonAnchorChannelFee,
	);
	assert!(result.is_err());
}

#[test]
fn test_v2_handle_open_channel_v2_feature_disabled() {
	// Verify that handle_open_channel_v2 sends an error when dual funding is disabled.
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let dual_fund_config = create_dual_funding_config();
	// Only node 0 has dual funding enabled; node 1 does not.
	let node_chanmgrs =
		create_node_chanmgrs(2, &node_cfgs, &[Some(dual_fund_config.clone()), None]);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let funding_input = create_dummy_funding_input(101_000);

	// Node 0 creates a v2 channel.
	let _temp_channel_id = nodes[0]
		.node
		.create_v2_channel(
			nodes[1].node.get_our_node_id(),
			100_000,
			vec![funding_input],
			42,
			None,
			ConfirmationTarget::NonAnchorChannelFee,
		)
		.unwrap();

	let open_msg = get_event_msg!(
		nodes[0],
		MessageSendEvent::SendOpenChannelV2,
		nodes[1].node.get_our_node_id()
	);

	// Node 1 (without dual funding) should reject the OpenChannelV2.
	nodes[1].node.handle_open_channel_v2(nodes[0].node.get_our_node_id(), &open_msg);

	// Should get an error message back, not an OpenChannelRequest event.
	let events = nodes[1].node.get_and_clear_pending_events();
	assert!(events.is_empty(), "Should not get any events when dual funding is disabled");

	let msg_events = nodes[1].node.get_and_clear_pending_msg_events();
	assert!(!msg_events.is_empty(), "Should get an error message");
	match &msg_events[0] {
		MessageSendEvent::HandleError { node_id, .. } => {
			assert_eq!(*node_id, nodes[0].node.get_our_node_id());
		},
		_ => panic!("Expected HandleError, got {:?}", msg_events[0]),
	}
}

#[test]
fn test_v2_channel_full_lifecycle() {
	// Full end-to-end test: create_v2_channel → open_channel_v2 → accept_channel_v2 →
	// interactive tx negotiation → FundingTransactionReadyForSigning → sign →
	// commitment_signed exchange → tx_signatures exchange → funding tx broadcast.
	//
	// In this test, only the initiator contributes inputs. The acceptor has zero contribution.
	// The protocol flow for this case is:
	//   1. Both sides complete interactive tx negotiation (tx_complete from both)
	//   2. Acceptor (0 contribution) auto-signs and sends commitment_signed
	//   3. Initiator gets FundingTransactionReadyForSigning, signs, sends commitment_signed
	//   4. Acceptor receives commitment_signed, can now send tx_signatures (empty witnesses)
	//   5. Initiator receives tx_signatures, sends tx_signatures back
	//   6. Both broadcast the funding transaction
	let chanmon_cfgs = create_chanmon_cfgs(2);
	let node_cfgs = create_node_cfgs(2, &chanmon_cfgs);
	let dual_fund_config = create_dual_funding_config();
	let node_chanmgrs = create_node_chanmgrs(
		2,
		&node_cfgs,
		&[Some(dual_fund_config.clone()), Some(dual_fund_config.clone())],
	);
	let nodes = create_network(2, &node_cfgs, &node_chanmgrs);

	let node_id_0 = nodes[0].node.get_our_node_id();
	let node_id_1 = nodes[1].node.get_our_node_id();

	let funding_satoshis = 100_000;
	// Create a wallet-signable funding input for the initiator.
	let funding_input = create_wallet_funding_input(&nodes[0], funding_satoshis + 5_000);

	// Step 1: Node 0 creates a v2 channel.
	let _temp_channel_id = nodes[0]
		.node
		.create_v2_channel(
			node_id_1,
			funding_satoshis,
			vec![funding_input],
			42,
			None,
			ConfirmationTarget::NonAnchorChannelFee,
		)
		.unwrap();

	// Step 2: Deliver OpenChannelV2 to node 1.
	let open_msg =
		get_event_msg!(nodes[0], MessageSendEvent::SendOpenChannelV2, node_id_1);
	nodes[1].node.handle_open_channel_v2(node_id_0, &open_msg);

	// Step 3: Node 1 accepts with zero contribution.
	let events = nodes[1].node.get_and_clear_pending_events();
	assert_eq!(events.len(), 1);
	let temp_channel_id = match &events[0] {
		Event::OpenChannelRequest { temporary_channel_id, .. } => *temporary_channel_id,
		_ => panic!("Expected OpenChannelRequest"),
	};
	nodes[1]
		.node
		.accept_inbound_channel(&temp_channel_id, &node_id_0, 43, None)
		.unwrap();

	// Step 4: Deliver AcceptChannelV2 to node 0. This starts interactive tx.
	let accept_msg =
		get_event_msg!(nodes[1], MessageSendEvent::SendAcceptChannelV2, node_id_0);
	nodes[0].node.handle_accept_channel_v2(node_id_1, &accept_msg);

	// Step 5: Drive interactive tx negotiation to completion.
	drive_interactive_tx_negotiation(&nodes[0], &nodes[1]);

	// Step 6: The acceptor (0 contribution) auto-signs during tx_complete and sends
	// commitment_signed. The initiator gets FundingTransactionReadyForSigning.

	// Check for the acceptor's commitment_signed (sent from the tx_complete handler since
	// the acceptor has no inputs to sign).
	let acceptor_msg_events = nodes[1].node.get_and_clear_pending_msg_events();
	let acceptor_commitment_signed = if !acceptor_msg_events.is_empty() {
		match &acceptor_msg_events[0] {
			MessageSendEvent::UpdateHTLCs { ref updates, .. } => {
				Some(updates.commitment_signed[0].clone())
			},
			_ => None,
		}
	} else {
		None
	};

	// Initiator should get FundingTransactionReadyForSigning.
	let event = get_event!(nodes[0], Event::FundingTransactionReadyForSigning);
	let (channel_id, unsigned_transaction) = match event {
		Event::FundingTransactionReadyForSigning {
			channel_id,
			unsigned_transaction,
			..
		} => (channel_id, unsigned_transaction),
		_ => panic!("Expected FundingTransactionReadyForSigning"),
	};

	// Step 7: Sign the transaction using the wallet and provide it back.
	let signed_tx = nodes[0].wallet_source.sign_tx(unsigned_transaction).unwrap();
	nodes[0]
		.node
		.funding_transaction_signed(&channel_id, &node_id_1, signed_tx)
		.unwrap();

	// Step 8: The initiator should send commitment_signed.
	let msg_events = nodes[0].node.get_and_clear_pending_msg_events();
	assert!(!msg_events.is_empty(), "Initiator should send commitment_signed after signing");
	let initiator_commitment_signed = match &msg_events[0] {
		MessageSendEvent::UpdateHTLCs { ref updates, .. } => {
			updates.commitment_signed[0].clone()
		},
		_ => panic!("Expected UpdateHTLCs with commitment_signed, got {:?}", msg_events[0]),
	};

	// Step 9: Deliver the acceptor's commitment_signed to the initiator (if we got one).
	if let Some(ref cs) = acceptor_commitment_signed {
		nodes[0].node.handle_commitment_signed(node_id_1, cs);
	}

	// Deliver the initiator's commitment_signed to the acceptor.
	nodes[1].node.handle_commitment_signed(node_id_0, &initiator_commitment_signed);

	// Step 10: The acceptor can now send tx_signatures (since it received commitment_signed
	// and holder_sends_tx_signatures_first is true for the 0-contribution side).
	let msg_events = nodes[1].node.get_and_clear_pending_msg_events();
	// The acceptor may also send commitment_signed here if it wasn't sent earlier.
	let mut acceptor_tx_signatures = None;
	for event in &msg_events {
		match event {
			MessageSendEvent::UpdateHTLCs { ref updates, .. } => {
				if acceptor_commitment_signed.is_none() {
					nodes[0].node.handle_commitment_signed(node_id_1, &updates.commitment_signed[0]);
				}
			},
			MessageSendEvent::SendTxSignatures { ref msg, .. } => {
				acceptor_tx_signatures = Some(msg.clone());
			},
			_ => {},
		}
	}

	let acceptor_tx_sigs = acceptor_tx_signatures
		.expect("Acceptor should send tx_signatures after receiving commitment_signed");

	// Step 11: Deliver acceptor's tx_signatures to initiator.
	nodes[0].node.handle_tx_signatures(node_id_1, &acceptor_tx_sigs);

	// Step 12: Initiator should send tx_signatures back.
	let msg_events = nodes[0].node.get_and_clear_pending_msg_events();
	assert!(!msg_events.is_empty(), "Initiator should send tx_signatures");
	let initiator_tx_signatures = match &msg_events[0] {
		MessageSendEvent::SendTxSignatures { ref msg, .. } => msg.clone(),
		_ => panic!("Expected SendTxSignatures, got {:?}", msg_events[0]),
	};

	// Deliver to acceptor.
	nodes[1].node.handle_tx_signatures(node_id_0, &initiator_tx_signatures);

	// Step 13: Both sides should have broadcast the funding transaction.
	check_added_monitors(&nodes[0], 1);
	check_added_monitors(&nodes[1], 1);

	let initiator_txn = nodes[0].tx_broadcaster.txn_broadcast();
	assert_eq!(initiator_txn.len(), 1, "Initiator should broadcast funding tx");
	let acceptor_txn = nodes[1].tx_broadcaster.txn_broadcast();
	assert_eq!(acceptor_txn.len(), 1, "Acceptor should broadcast funding tx");

	// Both should broadcast the same transaction.
	assert_eq!(initiator_txn[0].compute_txid(), acceptor_txn[0].compute_txid());

	// Drain ChannelPending events from both nodes.
	let events_0 = nodes[0].node.get_and_clear_pending_events();
	assert!(
		events_0.iter().any(|e| matches!(e, Event::ChannelPending { .. })),
		"Initiator should have ChannelPending event"
	);
	let events_1 = nodes[1].node.get_and_clear_pending_events();
	assert!(
		events_1.iter().any(|e| matches!(e, Event::ChannelPending { .. })),
		"Acceptor should have ChannelPending event"
	);
}
