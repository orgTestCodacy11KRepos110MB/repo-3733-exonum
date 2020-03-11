// Copyright 2020 The Exonum Team
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

//! High-level tests for the Exonum node.

use exonum::{
    blockchain::{config::GenesisConfigBuilder, Blockchain},
    crypto::KeyPair,
    helpers::Height,
    merkledb::{Database, ObjectHash, TemporaryDB},
    runtime::{ExecutionContext, ExecutionError, InstanceId, SnapshotExt},
};
use exonum_derive::*;
use exonum_rust_runtime::{AfterCommitContext, RustRuntime, Service, ServiceFactory};
use futures::{sync::mpsc, Future, Stream};
use tokio::util::FutureExt;
use tokio_core::reactor::Core;

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use exonum_node::{generate_testnet_config, Node, NodeBuilder, NodeConfig, ShutdownHandle};

#[derive(Debug)]
struct RunHandle {
    blockchain: Blockchain,
    node_thread: thread::JoinHandle<()>,
    shutdown_handle: ShutdownHandle,
}

impl RunHandle {
    fn new(node: Node) -> Self {
        let blockchain = node.blockchain().to_owned();
        let shutdown_handle = node.shutdown_handle();
        Self {
            blockchain,
            shutdown_handle,
            node_thread: thread::spawn(|| node.run().unwrap()),
        }
    }

    fn join(self) {
        self.shutdown_handle.shutdown().wait().unwrap();
        self.node_thread.join().unwrap();
    }
}

#[exonum_interface(auto_ids)]
trait DummyInterface<Ctx> {
    type Output;
    fn timestamp(&self, context: Ctx, _value: u64) -> Self::Output;
}

#[derive(Debug, Clone, ServiceDispatcher, ServiceFactory)]
#[service_dispatcher(implements("DummyInterface"))]
#[service_factory(
    artifact_name = "after-commit",
    artifact_version = "1.0.0",
    proto_sources = "exonum::proto::schema",
    service_constructor = "CommitWatcherService::new_instance"
)]
struct CommitWatcherService(mpsc::UnboundedSender<()>);

impl CommitWatcherService {
    const ID: InstanceId = 2;

    fn new_instance(&self) -> Box<dyn Service> {
        Box::new(self.clone())
    }
}

impl Service for CommitWatcherService {
    fn after_commit(&self, _context: AfterCommitContext<'_>) {
        self.0.unbounded_send(()).ok();
    }
}

impl DummyInterface<ExecutionContext<'_>> for CommitWatcherService {
    type Output = Result<(), ExecutionError>;

    fn timestamp(&self, _context: ExecutionContext<'_>, _value: u64) -> Self::Output {
        Ok(())
    }
}

#[derive(Debug, ServiceDispatcher)]
struct StartCheckerService;

impl Service for StartCheckerService {}

#[derive(Debug, ServiceFactory)]
#[service_factory(
    artifact_name = "configure",
    artifact_version = "1.0.2",
    proto_sources = "exonum::proto::schema",
    service_constructor = "StartCheckerServiceFactory::new_instance"
)]
struct StartCheckerServiceFactory(pub Arc<Mutex<u64>>);

impl StartCheckerServiceFactory {
    fn new_instance(&self) -> Box<dyn Service> {
        *self.0.lock().unwrap() += 1;
        Box::new(StartCheckerService)
    }
}

fn run_nodes(
    count: u16,
    start_port: u16,
    slow_blocks: bool,
) -> (Vec<RunHandle>, Vec<mpsc::UnboundedReceiver<()>>) {
    let mut node_handles = Vec::new();
    let mut commit_rxs = Vec::new();
    for (mut node_cfg, node_keys) in generate_testnet_config(count, start_port) {
        let (commit_tx, commit_rx) = mpsc::unbounded();
        if slow_blocks {
            node_cfg.consensus.first_round_timeout = 20_000;
            node_cfg.consensus.min_propose_timeout = 10_000;
            node_cfg.consensus.max_propose_timeout = 10_000;
        }

        let service = CommitWatcherService(commit_tx);
        let artifact = service.artifact_id();
        let instance = artifact
            .clone()
            .into_default_instance(CommitWatcherService::ID, "commit-watcher");
        let genesis_cfg = GenesisConfigBuilder::with_consensus_config(node_cfg.consensus.clone())
            .with_artifact(artifact)
            .with_instance(instance)
            .build();

        let db = TemporaryDB::new();
        let node = NodeBuilder::new(db, node_cfg, node_keys)
            .with_genesis_config(genesis_cfg)
            .with_runtime_fn(|channel| {
                RustRuntime::builder()
                    .with_factory(service)
                    .build(channel.endpoints_sender())
            })
            .build();

        node_handles.push(RunHandle::new(node));
        commit_rxs.push(commit_rx);
    }

    (node_handles, commit_rxs)
}

#[test]
fn nodes_commit_blocks() {
    let (nodes, commit_rxs) = run_nodes(4, 16_300, false);

    let mut core = Core::new().unwrap();
    let duration = Duration::from_secs(60);
    for rx in commit_rxs {
        let future = rx.into_future().timeout(duration).map_err(drop);
        core.run(future).expect("failed commit");
    }

    for handle in nodes {
        handle.join();
    }
}

#[test]
fn nodes_flush_transactions_to_storage_before_commit() {
    // `slow_blocks: true` argument makes it so that nodes should not create a single block
    // during the test.
    let (nodes, _) = run_nodes(4, 16_400, true);
    let mut core = Core::new().unwrap();
    thread::sleep(Duration::from_secs(5));

    // Send some transactions over `blockchain`s.
    let keys = KeyPair::random();
    let tx_hashes: Vec<_> = (0_u64..10)
        .map(|i| {
            let tx = keys.timestamp(CommitWatcherService::ID, i);
            let tx_hash = tx.object_hash();
            let node_i = i as usize % nodes.len();
            let broadcast = nodes[node_i].blockchain.sender().broadcast_transaction(tx);
            core.run(broadcast).unwrap();
            tx_hash
        })
        .collect();

    // Nodes need order of 100ms to create a column family for the tx pool in the debug mode,
    // so we sleep here to make it happen for all nodes.
    thread::sleep(Duration::from_millis(300));

    // All transactions should be persisted on all nodes now.
    for node in &nodes {
        let snapshot = node.blockchain.snapshot();
        let snapshot = snapshot.for_core();
        assert_eq!(snapshot.height(), Height(0));
        let tx_pool = snapshot.transactions_pool();
        for tx_hash in &tx_hashes {
            assert!(tx_pool.contains(tx_hash));
        }
    }

    for handle in nodes {
        handle.join();
    }
}

#[test]
fn node_restart_regression() {
    let start_node = |node_cfg: NodeConfig, node_keys, db, start_times| {
        let service = StartCheckerServiceFactory(start_times);
        let artifact = service.artifact_id();
        let genesis_config =
            GenesisConfigBuilder::with_consensus_config(node_cfg.consensus.clone())
                .with_artifact(artifact.clone())
                .with_instance(artifact.into_default_instance(4, "startup-checker"))
                .build();

        let node = NodeBuilder::new(db, node_cfg, node_keys)
            .with_genesis_config(genesis_config)
            .with_runtime_fn(|channel| {
                RustRuntime::builder()
                    .with_factory(service)
                    .build(channel.endpoints_sender())
            })
            .build();
        RunHandle::new(node).join();
    };

    let db = Arc::new(TemporaryDB::new()) as Arc<dyn Database>;
    let (node_cfg, node_keys) = generate_testnet_config(1, 3600).pop().unwrap();

    let start_times = Arc::new(Mutex::new(0));
    // First launch
    start_node(
        node_cfg.clone(),
        node_keys.clone(),
        Arc::clone(&db),
        Arc::clone(&start_times),
    );
    // Second launch
    start_node(node_cfg, node_keys, db, Arc::clone(&start_times));

    // The service is created two times on instantiation (for `start_adding_service`
    // and `commit_service` methods), and then once on each new node startup.
    assert_eq!(*start_times.lock().unwrap(), 3);
}
