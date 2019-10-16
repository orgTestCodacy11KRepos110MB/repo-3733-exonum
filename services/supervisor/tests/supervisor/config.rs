// Copyright 2019 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use exonum_merkledb::ObjectHash;
use exonum_testkit::TestKitBuilder;

use exonum::{
    blockchain::InstanceCollection,
    crypto,
    helpers::{Height, ValidatorId},
    runtime::{rust::Transaction, InstanceId, SUPERVISOR_INSTANCE_ID},
};

use crate::{utils::*, IncService as ConfigChangeService};
use exonum_supervisor::{ConfigVote, Error, Supervisor};

#[test]
fn test_multiple_consensus_change_proposes() {
    let mut testkit = testkit_with_supervisor(1);

    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(consensus_config_propose_first_variant(&testkit))
        .extend_consensus_config_propose(consensus_config_propose_second_variant(&testkit))
        .config_propose();

    let signed_proposal =
        sign_config_propose_transaction(&testkit, config_proposal, ValidatorId(0));
    let block = testkit.create_block_with_transaction(signed_proposal);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::MalformedConfigPropose.into()));

    assert_eq!(config_propose_entry(&testkit), None);
}

#[test]
fn test_deadline_config_exceeded() {
    let mut testkit = testkit_with_supervisor(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();
    let consensus_config = testkit.consensus_config();
    let new_consensus_config = consensus_config_propose_first_variant(&testkit);

    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(new_consensus_config.clone())
        .config_propose();
    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");
    testkit.create_blocks_until(CFG_CHANGE_HEIGHT.next());

    assert_eq!(config_propose_entry(&testkit), None);
    assert_eq!(testkit.consensus_config(), consensus_config);
}

#[test]
fn test_sent_new_config_after_expired_one() {
    let mut testkit = testkit_with_supervisor(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let first_consensus_config = consensus_config_propose_first_variant(&testkit);

    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(first_consensus_config.clone())
        .config_propose();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");
    testkit.create_blocks_until(CFG_CHANGE_HEIGHT.next());
    assert_eq!(config_propose_entry(&testkit), None);

    // Send config one more time and vote for it
    let cfg_change_height = Height(5);
    let second_consensus_config = consensus_config_propose_second_variant(&testkit);

    let config_proposal = ConfigProposeBuilder::new(cfg_change_height)
        .extend_consensus_config_propose(second_consensus_config.clone())
        .config_propose();
    let proposal_hash = config_proposal.object_hash();
    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    let signed_txs = build_confirmation_transactions(&testkit, proposal_hash, initiator_id);
    testkit
        .create_block_with_transactions(signed_txs)
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");
    testkit.create_blocks_until(cfg_change_height);

    assert_eq!(config_propose_entry(&testkit), None);
    assert_eq!(testkit.consensus_config(), second_consensus_config);
}

#[test]
fn test_discard_config_with_not_enough_confirms() {
    let mut testkit = testkit_with_supervisor(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    testkit.create_block();
    let base_consensus_config = testkit.consensus_config();

    let cfg_change_height = Height(3);
    let consensus_config = consensus_config_propose_first_variant(&testkit);
    let config_proposal = ConfigProposeBuilder::new(cfg_change_height)
        .extend_consensus_config_propose(consensus_config.clone())
        .config_propose();
    let proposal_hash = config_proposal.object_hash();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    // Sign confirmation transaction by second validator
    let keys = testkit.network().validators()[1].service_keypair();
    let signed_confirm = ConfigVote {
        propose_hash: proposal_hash,
    }
    .sign(SUPERVISOR_INSTANCE_ID, keys.0, &keys.1);
    testkit
        .create_block_with_transaction(signed_confirm)
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");

    testkit.create_blocks_until(cfg_change_height.next());
    assert_eq!(config_propose_entry(&testkit), None);
    assert_eq!(testkit.consensus_config(), base_consensus_config);
}

#[test]
fn test_apply_config_by_min_required_majority() {
    let mut testkit = testkit_with_supervisor(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let cfg_change_height = Height(3);
    let consensus_config = consensus_config_propose_first_variant(&testkit);
    let config_proposal = ConfigProposeBuilder::new(cfg_change_height)
        .extend_consensus_config_propose(consensus_config.clone())
        .config_propose();
    let proposal_hash = config_proposal.object_hash();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    let confirm = ConfigVote {
        propose_hash: proposal_hash,
    };
    // Sign and send confirmation transaction by second validator
    let keys = testkit.network().validators()[1].service_keypair();
    testkit
        .create_block_with_transaction(confirm.clone().sign(
            SUPERVISOR_INSTANCE_ID,
            keys.0,
            &keys.1,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");

    // Sign confirmation transaction by third validator
    let keys = testkit.network().validators()[2].service_keypair();
    testkit
        .create_block_with_transaction(confirm.sign(SUPERVISOR_INSTANCE_ID, keys.0, &keys.1))
        .transactions[0]
        .status()
        .expect("Transaction with confirmation discarded.");

    assert_eq!(config_propose_entry(&testkit), None);
    assert_eq!(testkit.consensus_config(), consensus_config);
}

#[test]
fn test_send_confirmation_by_initiator() {
    let mut testkit = testkit_with_supervisor(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let consensus_config = consensus_config_propose_first_variant(&testkit);
    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(consensus_config.clone())
        .config_propose();
    let proposal_hash = config_proposal.object_hash();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    // Try to send confirmation transaction by the initiator
    let keys = testkit.network().us().service_keypair();
    let signed_confirm = ConfigVote {
        propose_hash: proposal_hash,
    }
    .sign(SUPERVISOR_INSTANCE_ID, keys.0, &keys.1);

    let block = testkit.create_block_with_transaction(signed_confirm);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::AttemptToVoteTwice.into()));
}

#[test]
fn test_propose_config_change_by_incorrect_validator() {
    let mut testkit = testkit_with_supervisor(1);

    let consensus_config = consensus_config_propose_first_variant(&testkit);
    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(consensus_config.clone())
        .config_propose();

    let keys = crypto::gen_keypair();
    let signed_confirm = config_proposal.sign(SUPERVISOR_INSTANCE_ID, keys.0, &keys.1);

    let block = testkit.create_block_with_transaction(signed_confirm);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::UnknownAuthor.into()));
}

#[test]
fn test_confirm_config_by_incorrect_validator() {
    let mut testkit = testkit_with_supervisor(1);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let consensus_config = consensus_config_propose_first_variant(&testkit);
    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(consensus_config.clone())
        .config_propose();
    let proposal_hash = config_proposal.object_hash();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    let keys = crypto::gen_keypair();
    let signed_confirm = ConfigVote {
        propose_hash: proposal_hash,
    }
    .sign(SUPERVISOR_INSTANCE_ID, keys.0, &keys.1);

    let block = testkit.create_block_with_transaction(signed_confirm);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::UnknownAuthor.into()));
}

#[test]
fn test_try_confirm_non_existing_proposal() {
    let mut testkit = testkit_with_supervisor(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let consensus_config = consensus_config_propose_first_variant(&testkit);
    let config_proposal = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_consensus_config_propose(consensus_config.clone())
        .config_propose();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            config_proposal,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    let wrong_hash = crypto::hash(&[0]);;
    let signed_confirm = build_confirmation_transactions(&testkit, wrong_hash, initiator_id);

    let block = testkit.create_block_with_transactions(signed_confirm);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::ConfigProposeNotRegistered.into()));
}

#[test]
fn test_service_config_change() {
    let mut testkit = testkit_with_supervisor_and_service(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let params = "I am a new parameter".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .config_propose();
    let proposal_hash = propose.object_hash();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            propose,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");
    let signed_txs = build_confirmation_transactions(&testkit, proposal_hash, initiator_id);
    testkit
        .create_block_with_transactions(signed_txs)
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");

    testkit.create_blocks_until(CFG_CHANGE_HEIGHT);

    assert_eq!(config_propose_entry(&testkit), None);
    check_service_actual_param(&testkit, Some(params));
}

#[test]
fn test_discard_errored_service_config_change() {
    let mut testkit = testkit_with_supervisor_and_service(4);

    let params = "I am a discarded parameter".to_owned();
    let new_consensus_config = consensus_config_propose_first_variant(&testkit);

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .extend_service_config_propose("error".to_string())
        .extend_consensus_config_propose(new_consensus_config)
        .config_propose();

    let signed_proposal = sign_config_propose_transaction(&testkit, propose, ValidatorId(0));

    let block = testkit.create_block_with_transaction(signed_proposal);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::MalformedConfigPropose.into()));

    assert_eq!(config_propose_entry(&testkit), None);
}

#[test]
fn test_discard_panicked_service_config_change() {
    let mut testkit = testkit_with_supervisor_and_service(4);

    let params = "I am a discarded parameter".to_owned();
    let new_consensus_config = consensus_config_propose_first_variant(&testkit);

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .extend_service_config_propose("panic".to_string())
        .extend_consensus_config_propose(new_consensus_config)
        .config_propose();

    let signed_proposal = sign_config_propose_transaction(&testkit, propose, ValidatorId(0));

    let block = testkit.create_block_with_transaction(signed_proposal);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::MalformedConfigPropose.into()));

    assert_eq!(config_propose_entry(&testkit), None);
}

#[test]
fn test_incorrect_actual_from_field() {
    let mut testkit = testkit_with_supervisor_and_service(1);

    let params = "I am a new parameter".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .config_propose();

    testkit.create_blocks_until(CFG_CHANGE_HEIGHT);

    let signed_proposal = sign_config_propose_transaction(&testkit, propose, ValidatorId(0));

    let block = testkit.create_block_with_transaction(signed_proposal);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::ActualFromIsPast.into()));
}

#[test]
fn test_another_configuration_change_proposal() {
    let mut testkit = testkit_with_supervisor_and_service(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();
    let params = "I am a new parameter".to_owned();

    let cfg_change_height = Height(4);
    let propose = ConfigProposeBuilder::new(cfg_change_height)
        .extend_service_config_propose(params.clone())
        .config_propose();

    let proposal_hash = propose.object_hash();
    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            propose,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    // Try to commit second config change propose.
    let second_propose = ConfigProposeBuilder::new(cfg_change_height)
        .extend_service_config_propose("I am an overridden parameter".to_string())
        .config_propose();

    let signed_proposal = sign_config_propose_transaction(&testkit, second_propose, initiator_id);
    let block = testkit.create_block_with_transaction(signed_proposal);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::ConfigProposeExists.into()));

    let signed_txs = build_confirmation_transactions(&testkit, proposal_hash, initiator_id);
    testkit
        .create_block_with_transactions(signed_txs)
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");
    testkit.create_blocks_until(cfg_change_height);

    assert_eq!(config_propose_entry(&testkit), None);
    check_service_actual_param(&testkit, Some(params));
}

#[test]
fn test_service_config_discard_fake_supervisor() {
    const FAKE_SUPERVISOR_ID: InstanceId = 5;
    let keypair = crypto::gen_keypair();

    let mut testkit = TestKitBuilder::validator()
        .with_validators(1)
        .with_rust_service(InstanceCollection::new(Supervisor).with_instance(
            FAKE_SUPERVISOR_ID,
            "fake-supervisor",
            Vec::default(),
        ))
        .with_rust_service(ConfigChangeService)
        .create();

    let params = "I am a new parameter".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .config_propose();

    let block = testkit.create_block_with_transaction(propose.sign(
        FAKE_SUPERVISOR_ID,
        keypair.0,
        &keypair.1,
    ));
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::UnknownAuthor.into()));
}

#[test]
fn test_test_configuration_and_rollbacks() {
    let mut testkit = testkit_with_supervisor(4);

    testkit.create_blocks_until(CFG_CHANGE_HEIGHT);

    let cfg_change_height = Height(4);
    let old_config = testkit.consensus_config();

    testkit.checkpoint();

    let new_config = consensus_config_propose_first_variant(&testkit);

    let propose = ConfigProposeBuilder::new(cfg_change_height)
        .extend_consensus_config_propose(new_config.clone())
        .config_propose();

    let proposal_hash = propose.object_hash();
    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            propose,
            ValidatorId(0),
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");
    let signed_txs = build_confirmation_transactions(&testkit, proposal_hash, ValidatorId(0));
    testkit
        .create_block_with_transactions(signed_txs)
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");

    testkit.create_blocks_until(cfg_change_height);
    assert_eq!(config_propose_entry(&testkit), None);
    assert_eq!(testkit.consensus_config(), new_config);

    testkit.checkpoint();
    testkit.create_block();
    testkit.rollback();
    assert_eq!(testkit.consensus_config(), new_config);
    assert_eq!(config_propose_entry(&testkit), None);

    testkit.rollback();

    // As rollback is behind the time a proposal entered the blockchain,
    // the proposal is effectively forgotten.
    testkit.create_blocks_until(Height(4));
    assert_eq!(testkit.consensus_config(), old_config);
    assert_eq!(config_propose_entry(&testkit), None);
}

#[test]
fn test_service_config_discard_single_apply_error() {
    let mut testkit = testkit_with_supervisor_and_service(1);

    let params = "apply_error".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .config_propose();
    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            propose,
            ValidatorId(0),
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    testkit.create_blocks_until(CFG_CHANGE_HEIGHT.next());
    assert_eq!(config_propose_entry(&testkit), None);

    check_service_actual_param(&testkit, None);
}

#[test]
fn test_service_config_discard_single_apply_panic() {
    let mut testkit = testkit_with_supervisor_and_service(1);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let params = "apply_panic".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .config_propose();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            propose,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");
    testkit.create_blocks_until(CFG_CHANGE_HEIGHT.next());

    assert_eq!(config_propose_entry(&testkit), None);
    check_service_actual_param(&testkit, None);
}

#[test]
fn test_services_config_apply_multiple_configs() {
    let mut testkit = testkit_with_supervisor_and_2_services(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let params = "I am a new parameter".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .extend_second_service_config_propose(params.clone())
        .config_propose();
    let proposal_hash = propose.object_hash();

    testkit
        .create_block_with_transaction(sign_config_propose_transaction(
            &testkit,
            propose,
            initiator_id,
        ))
        .transactions[0]
        .status()
        .expect("Transaction with change propose discarded.");

    let signed_txs = build_confirmation_transactions(&testkit, proposal_hash, initiator_id);
    testkit
        .create_block_with_transactions(signed_txs)
        .transactions[0]
        .status()
        .expect("Transaction with confirmations discarded.");
    testkit.create_blocks_until(CFG_CHANGE_HEIGHT);

    check_service_actual_param(&testkit, Some(params.clone()));
    check_second_service_actual_param(&testkit, Some(params));
}

#[test]
fn test_services_config_discard_multiple_configs() {
    let mut testkit = testkit_with_supervisor_and_2_services(1);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    let params = "I am a new parameter".to_owned();

    let propose = ConfigProposeBuilder::new(CFG_CHANGE_HEIGHT)
        .extend_service_config_propose(params.clone())
        .extend_second_service_config_propose(params.clone())
        .extend_second_service_config_propose("I am a extra proposal".to_owned())
        .config_propose();

    let signed_proposal = sign_config_propose_transaction(&testkit, propose, initiator_id);

    let block = testkit.create_block_with_transaction(signed_proposal);
    let status = block.transactions[0].status();
    assert_eq!(status, Err(&Error::MalformedConfigPropose.into()));

    assert_eq!(config_propose_entry(&testkit), None);
}

#[test]
fn test_several_service_config_changes() {
    let mut testkit = testkit_with_supervisor_and_service(4);
    let initiator_id = testkit.network().us().validator_id().unwrap();

    for i in 1..5 {
        let cfg_change_height = Height(2 * i);
        let params = format!("Change {}", i);

        let propose = ConfigProposeBuilder::new(cfg_change_height)
            .extend_service_config_propose(params.clone())
            .config_propose();
        let proposal_hash = propose.object_hash();

        testkit
            .create_block_with_transaction(sign_config_propose_transaction(
                &testkit,
                propose,
                initiator_id,
            ))
            .transactions[0]
            .status()
            .expect("Transaction with change propose discarded.");

        let signed_txs = build_confirmation_transactions(&testkit, proposal_hash, initiator_id);
        testkit.create_block_with_transactions(signed_txs)[0]
            .status()
            .unwrap();

        testkit.create_blocks_until(cfg_change_height);
        assert_eq!(config_propose_entry(&testkit), None);
    }

    check_service_actual_param(&testkit, Some("Change 4".to_string()));
}