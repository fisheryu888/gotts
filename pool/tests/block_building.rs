// Copyright 2018 The Grin Developers
// Modifications Copyright 2019 The Gotts Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod common;

use self::core::core::hash::Hashed;
use self::core::core::verifier_cache::LruVerifierCache;
use self::core::core::{Block, BlockHeader, OutputEx, Transaction, Weighting};
use self::core::libtx;
use self::core::libtx::build;
use self::core::libtx::ProofBuilder;
use self::core::pow::Difficulty;
use self::keychain::{ExtKeychain, Identifier, Keychain};
use self::pool::types::PoolError;
use self::util::secp::pedersen::Commitment;
use self::util::RwLock;
use crate::common::*;
use gotts_core as core;
use gotts_keychain as keychain;
use gotts_pool as pool;
use gotts_util as util;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn test_transaction_pool_block_building() {
	util::init_test_logger();
	let keychain: ExtKeychain = Keychain::from_random_seed(false).unwrap();
	let builder = ProofBuilder::new(&keychain, &Identifier::zero());

	let db_root = ".gotts_block_building".to_string();
	clean_output_dir(db_root.clone());

	{
		let mut chain = ChainAdapter::init(db_root.clone()).unwrap();

		let verifier_cache = Arc::new(RwLock::new(LruVerifierCache::new()));

		// Initialize the chain/txhashset with an initial block
		// so we have a non-empty UTXO set.
		let add_block =
			|prev_header: BlockHeader, txs: Vec<Transaction>, chain: &mut ChainAdapter| {
				let height = prev_header.height + 1;
				let key_id = ExtKeychain::derive_key_id(1, height as u32, 0, 0, 0);
				let fee = txs.iter().map(|x| x.fee()).sum();
				let reward = libtx::reward::output(
					&keychain,
					&libtx::ProofBuilder::new(&keychain, &Identifier::zero()),
					&key_id,
					fee,
					false,
				)
				.unwrap();
				let mut block = Block::new(&prev_header, txs, Difficulty::min(), reward).unwrap();

				// Set the prev_root to the prev hash for testing purposes (no MMR to obtain a root from).
				block.header.prev_root = prev_header.hash();

				chain.update_db_for_block(&block);
				block
			};

		let block = add_block(BlockHeader::default(), vec![], &mut chain);
		let header = block.header;

		// Now create tx to spend that first coinbase (now matured).
		// Provides us with some useful outputs to test with.
		let initial_tx = test_transaction_spending_coinbase(
			&keychain,
			&header,
			vec![10, 20, 30, 40, 59_000_000_000],
		);

		let mut complete_inputs: HashMap<Commitment, OutputEx> = HashMap::new();
		let key_id1 = ExtKeychain::derive_key_id(1, header.height as u32, 0, 0, 0);
		let (pre_tx, _) = build::partial_transaction(
			vec![build::output(60_000_000_000, Some(0i64), key_id1)],
			&keychain,
			&builder,
		)
		.unwrap();
		complete_inputs.insert(
			pre_tx.body.outputs[0].commit,
			OutputEx {
				output: pre_tx.body.outputs[0],
				height: header.height,
				mmr_index: 1, // wrong index but not used here
			},
		);
		initial_tx
			.validate(
				Weighting::AsTransaction,
				verifier_cache.clone(),
				Some(&complete_inputs),
				1,
			)
			.unwrap();

		// Mine that initial tx so we can spend it with multiple txs
		let block = add_block(header, vec![initial_tx], &mut chain);
		let header = block.header;

		// Initialize a new pool with our chain adapter.
		let pool = RwLock::new(test_setup(Arc::new(chain.clone()), verifier_cache));

		let root_tx_1 = test_transaction(&keychain, vec![10, 20], vec![24]);
		let root_tx_2 = test_transaction(&keychain, vec![30], vec![28]);
		let root_tx_3 = test_transaction(&keychain, vec![40], vec![38]);

		let child_tx_1 = test_transaction(&keychain, vec![24], vec![22]);
		let child_tx_2 = test_transaction(&keychain, vec![38], vec![32]);

		{
			let mut write_pool = pool.write();

			// Add the three root txs to the pool.
			write_pool
				.add_to_pool(test_source(), root_tx_1.clone(), false, &header)
				.unwrap();
			write_pool
				.add_to_pool(test_source(), root_tx_2.clone(), false, &header)
				.unwrap();
			write_pool
				.add_to_pool(test_source(), root_tx_3.clone(), false, &header)
				.unwrap();

			// Now add the two child txs to the pool.
			write_pool
				.add_to_pool(test_source(), child_tx_1.clone(), false, &header)
				.unwrap();
			write_pool
				.add_to_pool(test_source(), child_tx_2.clone(), false, &header)
				.unwrap();

			assert_eq!(write_pool.total_size(), 5);
		}

		let txs = pool.read().prepare_mineable_transactions().unwrap();

		let block = add_block(header, txs, &mut chain);

		// Check the block contains what we expect.
		assert_eq!(block.inputs().len(), 4);
		assert_eq!(block.outputs().len(), 4);
		assert_eq!(block.kernels().len(), 6);

		assert!(block.kernels().contains(&root_tx_1.kernels()[0]));
		assert!(block.kernels().contains(&root_tx_2.kernels()[0]));
		assert!(block.kernels().contains(&root_tx_3.kernels()[0]));
		assert!(block.kernels().contains(&child_tx_1.kernels()[0]));
		assert!(block.kernels().contains(&child_tx_1.kernels()[0]));

		// Now reconcile the transaction pool with the new block
		// and check the resulting contents of the pool are what we expect.
		{
			let mut write_pool = pool.write();
			write_pool.reconcile_block(&block).unwrap();

			assert_eq!(write_pool.total_size(), 0);
		}

		// Now create a bad transaction with inflation
		let header = block.header;
		let bad_tx_1 = test_bad_transaction(&keychain, vec![32], vec![15]);
		{
			let mut write_pool = pool.write();

			// Add this bad tx to the pool.
			assert_eq!(
				write_pool.add_to_pool(test_source(), bad_tx_1.clone(), false, &header),
				Err(PoolError::InvalidTx(
					core::core::transaction::Error::TransactionSumMismatch
				)),
			);
			assert_eq!(write_pool.total_size(), 0);

			let goog_tx_1 = test_transaction(&keychain, vec![32], vec![15]);
			let bad_tx_2 = test_bad_transaction(&keychain, vec![15], vec![9]);
			write_pool
				.add_to_pool(test_source(), goog_tx_1.clone(), false, &header)
				.unwrap();
			assert_eq!(
				write_pool.add_to_pool(test_source(), bad_tx_2.clone(), false, &header),
				Err(PoolError::InvalidTx(
					core::core::transaction::Error::TransactionSumMismatch
				)),
			);
			assert_eq!(write_pool.total_size(), 1);
		}
	}
	// Cleanup db directory
	clean_output_dir(db_root.clone());
}
