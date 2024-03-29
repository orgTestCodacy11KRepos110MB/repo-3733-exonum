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

use exonum::{
    crypto::{Hash, PublicKey},
    helpers::{Height, ValidateInput},
    runtime::{
        migrations::MigrationType, CommonError, ExecutionContext, ExecutionError, ExecutionFail,
        InstanceId, InstanceSpec, InstanceState, InstanceStatus, RuntimeFeature,
    },
};
use exonum_derive::{exonum_interface, interface_method};
use exonum_merkledb::ObjectHash;

use std::collections::HashSet;

use super::{
    configure::ConfigureMut, migration_state::MigrationState, ArtifactError, AsyncEventState,
    CommonError as SupervisorCommonError, ConfigChange, ConfigProposalWithHash, ConfigPropose,
    ConfigVote, ConfigurationError, DeployRequest, DeployResult, FreezeService, MigrationError,
    MigrationRequest, MigrationResult, ResumeService, SchemaImpl, ServiceError, StartService,
    StopService, Supervisor, UnloadArtifact,
};
use exonum::runtime::ArtifactStatus;

/// Supervisor service transactions.
#[allow(clippy::empty_line_after_outer_attr)] // false positive
#[exonum_interface]
pub trait SupervisorInterface<Ctx> {
    /// Output generated by the stub.
    type Output;

    /// Requests artifact deploy.
    ///
    /// This request should be initiated by the validator (and depending on the `Supervisor`
    /// mode several other actions can be required, e.g. sending the same request by majority
    /// of other validators as well).
    /// After that, the supervisor will try to deploy the artifact, and once this procedure
    /// is completed, it will send `report_deploy_result` transaction.
    #[interface_method(id = 0)]
    fn request_artifact_deploy(&self, context: Ctx, artifact: DeployRequest) -> Self::Output;

    /// Confirms that the artifact deployment was completed by the validator.
    ///
    /// The artifact is registered in the dispatcher once all validators send successful confirmation.
    /// This transaction is sent automatically by a validator node when the local deployment process
    /// completes.
    #[interface_method(id = 1)]
    fn report_deploy_result(&self, context: Ctx, artifact: DeployResult) -> Self::Output;

    /// Proposes config change
    ///
    /// This request should be sent by one of validators as the proposition to change
    /// current configuration to new one. All another validators are able to vote for this
    /// configuration by sending `confirm_config_change` transaction.
    /// The configuration application rules depend on the `Supervisor` mode, e.g. confirmations
    /// are not required for the `Simple` mode, and for `Decentralized` mode (2/3+1) confirmations
    /// are required.
    ///
    /// **Note:** only one proposal at time is possible.
    #[interface_method(id = 2)]
    fn propose_config_change(&self, context: Ctx, propose: ConfigPropose) -> Self::Output;

    /// Confirms config change
    ///
    /// This confirm should be sent by validators to vote for proposed configuration.
    /// Vote of the author of the `propose_config_change` transaction is taken into
    /// account automatically.
    /// The configuration application rules depend on the `Supervisor` mode.
    #[interface_method(id = 3)]
    fn confirm_config_change(&self, context: Ctx, vote: ConfigVote) -> Self::Output;

    /// Requests the data migration.
    ///
    /// This request should be initiated by the validator (and depending on the `Supervisor`
    /// mode several other actions can be required, e.g. sending the same request by majority
    /// of other validators as well).
    /// After that, the core will try to perform the requested migration, and once the migration
    /// is finished, supervisor will send `report_deploy_result` transaction.
    #[interface_method(id = 4)]
    fn request_migration(&self, context: Ctx, request: MigrationRequest) -> Self::Output;

    /// Confirms that migration was completed by the validator.
    ///
    /// The migration is applied in the core once all validators send successful confirmation.
    /// This transaction is sent automatically by a validator node when the local migration process
    /// completes.
    #[interface_method(id = 5)]
    fn report_migration_result(&self, context: Ctx, result: MigrationResult) -> Self::Output;
}

impl ConfigChange {
    fn register_instance(
        &self,
        modified_instances: &mut HashSet<InstanceId>,
    ) -> Result<(), ExecutionError> {
        let maybe_instance_id = match self {
            Self::StopService(service) => Some(service.instance_id),
            Self::FreezeService(service) => Some(service.instance_id),
            Self::ResumeService(service) => Some(service.instance_id),
            Self::Service(service) => Some(service.instance_id),
            _ => None,
        };
        if let Some(instance_id) = maybe_instance_id {
            if !modified_instances.insert(instance_id) {
                let msg = format!(
                    "Discarded several actions concerning service with ID {}",
                    instance_id
                );
                return Err(ConfigurationError::malformed_propose(msg));
            }
        }
        Ok(())
    }
}

impl StartService {
    fn validate(&self, context: &ExecutionContext<'_>) -> Result<(), ExecutionError> {
        InstanceSpec::is_valid_name(&self.name).map_err(|e| {
            let msg = format!("Service name `{}` is invalid: {}", self.name, e);
            ServiceError::InvalidInstanceName.with_description(msg)
        })?;

        // Check that artifact is deployed and active.
        let dispatcher_data = context.data().for_dispatcher();
        let artifact_state = dispatcher_data
            .get_artifact(&self.artifact)
            .ok_or_else(|| {
                let msg = format!(
                    "Discarded start of service `{}` from the unknown artifact `{}`.",
                    self.name, self.artifact,
                );
                ArtifactError::UnknownArtifact.with_description(msg)
            })?;
        if artifact_state.status != ArtifactStatus::Active {
            let msg = format!(
                "Discarded start of service `{}` from the non-active artifact `{}`.",
                self.name, self.artifact,
            );
            return Err(ArtifactError::UnknownArtifact.with_description(msg));
        }

        // Check that there is no instance with the same name.
        if dispatcher_data.get_instance(self.name.as_str()).is_some() {
            return Err(ServiceError::InstanceExists.with_description(format!(
                "Discarded an attempt to start of the already started instance {}.",
                self.name
            )));
        }

        Ok(())
    }
}

impl StopService {
    fn validate(&self, context: &ExecutionContext<'_>) -> Result<(), ExecutionError> {
        validate_status(
            context,
            self.instance_id,
            "stop",
            InstanceStatus::can_be_stopped,
        )
        .map(drop)
    }
}

impl FreezeService {
    fn validate(&self, context: &ExecutionContext<'_>) -> Result<InstanceState, ExecutionError> {
        validate_status(
            context,
            self.instance_id,
            "freeze",
            InstanceStatus::can_be_frozen,
        )
    }
}

impl ResumeService {
    fn validate(&self, context: &ExecutionContext<'_>) -> Result<(), ExecutionError> {
        let instance = get_instance(context, self.instance_id)?;
        let status = instance.status.as_ref();

        let can_be_resumed = status.map_or(false, InstanceStatus::can_be_resumed);
        if !can_be_resumed {
            let status = status.map_or_else(|| "none".to_owned(), ToString::to_string);
            let msg = format!(
                "Discarded an attempt to resume service `{}` with inappropriate status ({})",
                instance.spec.name, status
            );
            return Err(ConfigurationError::malformed_propose(msg));
        }

        if instance.associated_artifact().is_none() {
            let msg = format!(
                "Service `{}` has data version ({}) differing from its artifact version (`{}`) \
                 and thus cannot be resumed",
                instance.spec.name,
                instance.data_version(),
                instance.spec.artifact
            );
            return Err(ConfigurationError::malformed_propose(msg));
        }

        Ok(())
    }
}

impl UnloadArtifact {
    fn validate(&self, context: &ExecutionContext<'_>) -> Result<(), ExecutionError> {
        context
            .data()
            .for_dispatcher()
            .check_unloading_artifact(&self.artifact_id)
            .map_err(|e| ConfigurationError::malformed_propose(e.description()))
    }
}

/// Checks if method was called by transaction, and transaction author is a validator.
fn get_validator(context: &ExecutionContext<'_>) -> Result<PublicKey, ExecutionError> {
    let author = context
        .caller()
        .author()
        .ok_or(CommonError::UnauthorizedCaller)?;

    // Verify that transaction author is validator.
    context
        .data()
        .for_core()
        .validator_id(author)
        .ok_or(CommonError::UnauthorizedCaller)?;

    Ok(author)
}

/// Returns the information about a service instance by its identifier.
fn get_instance(
    context: &ExecutionContext<'_>,
    instance_id: InstanceId,
) -> Result<InstanceState, ExecutionError> {
    context
        .data()
        .for_dispatcher()
        .get_instance(instance_id)
        .ok_or_else(|| {
            let msg = format!(
                "Instance with ID {} is absent from the blockchain",
                instance_id
            );
            ConfigurationError::malformed_propose(msg)
        })
}

/// Checks that the current service status allows a specified transition.
fn validate_status(
    context: &ExecutionContext<'_>,
    instance_id: InstanceId,
    action: &str,
    check_fn: fn(&InstanceStatus) -> bool,
) -> Result<InstanceState, ExecutionError> {
    let instance = get_instance(context, instance_id)?;
    let status = instance.status.as_ref();
    let is_valid_transition = status.map_or(false, check_fn);

    if is_valid_transition {
        Ok(instance)
    } else {
        let status = status.map_or_else(|| "none".to_owned(), ToString::to_string);
        let msg = format!(
            "Discarded an attempt to {} service `{}` with inappropriate status ({})",
            action, instance.spec.name, status
        );
        Err(ConfigurationError::malformed_propose(msg))
    }
}

/// Returns the information about a service instance by its name.
pub fn get_instance_by_name(
    context: &ExecutionContext<'_>,
    service: &str,
) -> Result<InstanceState, ExecutionError> {
    context
        .data()
        .for_dispatcher()
        .get_instance(service)
        .ok_or_else(|| {
            let msg = format!("Instance with name `{}` is absent from blockchain", service);
            ConfigurationError::malformed_propose(msg)
        })
}

impl SupervisorInterface<ExecutionContext<'_>> for Supervisor {
    type Output = Result<(), ExecutionError>;

    fn propose_config_change(
        &self,
        mut context: ExecutionContext<'_>,
        mut propose: ConfigPropose,
    ) -> Self::Output {
        let author = get_validator(&context)?;
        let current_height = context.data().for_core().height();

        // If `actual_from` field is not set, set it to the next height.
        if propose.actual_from == Height(0) {
            propose.actual_from = current_height.next();
        } else if current_height >= propose.actual_from {
            // Otherwise verify that the `actual_from` height is in the future.
            let msg = format!(
                "Actual height for config proposal ({}) is in the past (current height: {}).",
                propose.actual_from, current_height
            );
            return Err(SupervisorCommonError::ActualFromIsPast.with_description(msg));
        }

        let mut schema = SchemaImpl::new(context.service_data());

        // Verify that there are no pending config changes.
        if let Some(proposal) = schema.public.pending_proposal.get() {
            // We have a proposal, check that it's actual.
            if current_height < proposal.config_propose.actual_from {
                return Err(ConfigurationError::ConfigProposeExists.into());
            }
            // Proposal is outdated but was not removed (e.g. because of the panic
            // during config applying), clean it.
            schema.public.pending_proposal.remove();
        }
        drop(schema);

        // Verify changes in the proposal.
        Self::verify_config_changes(&mut context, &propose.changes)?;
        let mut schema = SchemaImpl::new(context.service_data());

        // After all the checks verify that configuration number is expected one.
        let expected_config_number = schema.get_configuration_number();
        if propose.configuration_number != expected_config_number {
            let msg = format!(
                "Number for config proposal ({}) differs from the expected one ({})",
                propose.configuration_number, expected_config_number
            );
            return Err(ConfigurationError::IncorrectConfigurationNumber.with_description(msg));
        }
        schema.increase_configuration_number();

        let propose_hash = propose.object_hash();
        schema.config_confirms.confirm(&propose_hash, author);

        let config_entry = ConfigProposalWithHash {
            config_propose: propose,
            propose_hash,
        };
        schema.public.pending_proposal.set(config_entry);

        Ok(())
    }

    fn confirm_config_change(
        &self,
        context: ExecutionContext<'_>,
        vote: ConfigVote,
    ) -> Self::Output {
        let author = get_validator(&context)?;

        let core_schema = context.data().for_core();
        let mut schema = SchemaImpl::new(context.service_data());
        let entry = schema
            .public
            .pending_proposal
            .get()
            .ok_or(ConfigurationError::ConfigProposeNotRegistered)?;

        // Verify that this config proposal is registered.
        if entry.propose_hash != vote.propose_hash {
            let msg = format!(
                "Mismatch between the hash of the saved proposal ({}) and the hash \
                 referenced in the vote ({})",
                entry.propose_hash, vote.propose_hash
            );
            return Err(ConfigurationError::ConfigProposeNotRegistered.with_description(msg));
        }

        // Verify that we didn't reach the deadline height.
        let config_propose = entry.config_propose;
        let current_height = core_schema.height();
        if config_propose.actual_from <= current_height {
            let msg = format!(
                "Deadline height ({}) exceeded for the config proposal ({}); \
                 voting for it is impossible",
                config_propose.actual_from, current_height
            );
            return Err(SupervisorCommonError::DeadlineExceeded.with_description(msg));
        }

        let already_confirmed = schema
            .config_confirms
            .confirmed_by(&entry.propose_hash, &author);
        if already_confirmed {
            return Err(ConfigurationError::AttemptToVoteTwice.into());
        }

        schema.config_confirms.confirm(&vote.propose_hash, author);
        log::trace!(
            "Propose config {:?} has been confirmed by {:?}",
            vote.propose_hash,
            author
        );

        Ok(())
    }

    fn request_artifact_deploy(
        &self,
        context: ExecutionContext<'_>,
        deploy: DeployRequest,
    ) -> Self::Output {
        // Verify that transaction author is validator.
        let author = get_validator(&context)?;

        deploy.artifact.validate().map_err(|e| {
            let msg = format!(
                "Artifact identifier `{}` is invalid: {}",
                deploy.artifact, e
            );
            ArtifactError::InvalidArtifactId.with_description(msg)
        })?;

        // Check that we didn't reach the deadline height.
        let core_schema = context.data().for_core();
        let current_height = core_schema.height();
        if deploy.deadline_height < current_height {
            return Err(SupervisorCommonError::ActualFromIsPast.into());
        }
        let mut schema = SchemaImpl::new(context.service_data());

        // Verify that the artifact is not deployed yet.
        let is_deployed = context
            .data()
            .for_dispatcher()
            .get_artifact(&deploy.artifact)
            .is_some();
        if is_deployed {
            let msg = format!("Artifact `{}` is already deployed", deploy.artifact);
            return Err(ArtifactError::AlreadyDeployed.with_description(msg));
        }

        // If deployment is already registered, check whether the request is new.
        if schema.pending_deployments.contains(&deploy.artifact) {
            let new_confirmation = !schema.deploy_requests.confirmed_by(&deploy, &author);
            return if new_confirmation {
                // It's OK, just an additional confirmation.
                schema.deploy_requests.confirm(&deploy, author);
                Ok(())
            } else {
                // Author already confirmed deployment of this artifact, so it's a duplicate.
                let msg = format!(
                    "Deploy of artifact `{}` is already confirmed by validator {}",
                    deploy.artifact, author
                );
                Err(ArtifactError::DeployRequestAlreadyRegistered.with_description(msg))
            };
        }

        schema.deploy_requests.confirm(&deploy, author);
        let supervisor_mode = schema.supervisor_config().mode;
        let validator_count = core_schema.consensus_config().validator_keys.len();
        if supervisor_mode.deploy_approved(&deploy, &schema.deploy_requests, validator_count) {
            schema.deploy_states.put(&deploy, AsyncEventState::Pending);
            log::trace!("Deploy artifact request accepted {:?}", deploy.artifact);
            let artifact = deploy.artifact.clone();
            schema.pending_deployments.put(&artifact, deploy);
        }
        Ok(())
    }

    fn report_deploy_result(
        &self,
        context: ExecutionContext<'_>,
        deploy_result: DeployResult,
    ) -> Self::Output {
        // Verify that transaction author is validator.
        let author = get_validator(&context)?;
        let core_schema = context.data().for_core();
        let current_height = core_schema.height();
        let schema = SchemaImpl::new(context.service_data());

        // Check if deployment already failed.
        if schema
            .deploy_states
            .get(&deploy_result.request)
            .map_or(false, |state| state.is_failed())
        {
            // This deployment is already resulted in failure, no further
            // processing needed.
            return Ok(());
        }

        // Verify that this deployment is registered.
        let deploy_request = schema
            .pending_deployments
            .get(&deploy_result.request.artifact)
            .ok_or_else(|| {
                let msg = format!(
                    "Deploy of artifact `{}` is not registered; reporting its result is impossible",
                    deploy_result.request.artifact
                );
                ArtifactError::DeployRequestNotRegistered.with_description(msg)
            })?;

        // Check that pending deployment is the same as in confirmation.
        if deploy_request != deploy_result.request {
            let msg = format!(
                "Mismatch between the recorded deploy request for artifact `{}` and the request \
                 mentioned in the deploy report",
                deploy_result.request.artifact
            );
            return Err(ArtifactError::DeployRequestNotRegistered.with_description(msg));
        }

        // Verify that we didn't reach deadline height.
        if deploy_request.deadline_height < current_height {
            let msg = format!(
                "Deadline height ({}) exceeded for the deploy request ({}); \
                 reporting deploy result is impossible",
                deploy_request.deadline_height, current_height
            );
            return Err(SupervisorCommonError::DeadlineExceeded.with_description(msg));
        }

        drop(schema);
        match deploy_result.result.0 {
            Ok(()) => Self::confirm_deploy(context, deploy_request, author)?,
            Err(error) => Self::fail_deploy(&context, &deploy_request, error),
        }
        Ok(())
    }

    fn request_migration(
        &self,
        mut context: ExecutionContext<'_>,
        request: MigrationRequest,
    ) -> Self::Output {
        // Verify that transaction author is validator.
        let author = get_validator(&context)?;

        // Check that target instance exists.
        let instance = get_instance_by_name(&context, &request.service)?;
        let core_schema = context.data().for_core();
        let validator_count = core_schema.consensus_config().validator_keys.len();

        // Check that we didn't reach the deadline height.
        let current_height = core_schema.height();
        if request.deadline_height < current_height {
            let msg = format!(
                "Deadline height ({}) for the migration request is in the past (current height: {})",
                request.deadline_height, current_height
            );
            return Err(SupervisorCommonError::ActualFromIsPast.with_description(msg));
        }

        let mut schema = SchemaImpl::new(context.service_data());
        schema.migration_requests.confirm(&request, author);
        let supervisor_mode = schema.supervisor_config().mode;
        let migration_approved = supervisor_mode.migration_approved(
            &request,
            &schema.migration_requests,
            validator_count,
        );

        if migration_approved {
            log::trace!(
                "Migration request for instance {} accepted",
                request.service
            );
            // Store initial state of the request.
            let mut state =
                MigrationState::new(AsyncEventState::Pending, instance.data_version().clone());
            schema.migration_states.put(&request, state.clone());
            // Store the migration as pending. It will be removed in `before_transactions` hook
            // once the migration will be completed (either successfully or unsuccessfully).
            schema.pending_migrations.insert(request.clone());

            // Finally, request core to start the migration.
            // If migration initialization will fail now, it won't be a transaction execution error,
            // since migration failure is one of possible outcomes of migration process. Instead of
            // returning an error, we will just mark this migration as failed.
            drop(schema);
            let supervisor_extensions = context.supervisor_extensions();
            let result = supervisor_extensions
                .initiate_migration(request.new_artifact.clone(), &request.service);

            // Check whether migration started successfully.
            let migration_type = match result {
                Ok(ty) => ty,
                Err(error) => {
                    // Migration failed even before start, softly mark it as failed.
                    let initiate_rollback = false;
                    return Self::fail_migration(context, &request, error, initiate_rollback);
                }
            };

            if let MigrationType::FastForward = migration_type {
                // Migration is fast-forward, complete it immediately.
                // No agreement needed, since nodes which will behave differently will obtain
                // different blockchain state hash and will be excluded from consensus.
                log::trace!("Applied fast-forward migration with request {:?}", request);
                let new_version = request.new_artifact.version.clone();

                let mut schema = SchemaImpl::new(context.service_data());
                // Update the state of a migration.
                state.update(AsyncEventState::Succeed, new_version);
                schema.migration_states.put(&request, state);
                // Remove the migration from the list of pending.
                schema.pending_migrations.remove(&request);
            }
        }
        Ok(())
    }

    fn report_migration_result(
        &self,
        context: ExecutionContext<'_>,
        result: MigrationResult,
    ) -> Self::Output {
        // Verifies that transaction author is validator.
        let author = get_validator(&context)?;

        let core_schema = context.data().for_core();
        let current_height = core_schema.height();
        let schema = SchemaImpl::new(context.service_data());

        // Verify that this migration is registered.
        let state = schema
            .migration_states
            .get(&result.request)
            .ok_or_else(|| {
                let msg = format!(
                    "Migration request {:?} is not registered; impossible to process its result",
                    result.request
                );
                MigrationError::MigrationRequestNotRegistered.with_description(msg)
            })?;

        // Check if migration already failed.
        if state.is_failed() {
            // This migration is already resulted in failure, no further
            // processing needed.
            return Ok(());
        }

        // Verify that we didn't reach deadline height.
        if result.request.deadline_height < current_height {
            let msg = format!(
                "Deadline height ({}) exceeded for the migration request ({}); \
                 reporting its result is impossible",
                result.request.deadline_height, current_height
            );
            return Err(SupervisorCommonError::DeadlineExceeded.with_description(msg));
        }

        drop(schema);

        match result.status.0 {
            Ok(hash) => Self::confirm_migration(context, &result.request, hash, author),
            Err(error) => {
                // Since the migration process error is represented as a string rather than
                // `ExecutionError`, we use our service error code, but set the description
                // to the actual error.
                let fail_cause =
                    ExecutionError::service(MigrationError::MigrationFailed as u8, error);
                let initiate_rollback = true;
                Self::fail_migration(context, &result.request, fail_cause, initiate_rollback)
            }
        }
    }
}

impl Supervisor {
    /// Verifies that each change introduced within config proposal is valid.
    fn verify_config_changes(
        context: &mut ExecutionContext<'_>,
        changes: &[ConfigChange],
    ) -> Result<(), ExecutionError> {
        // To prevent multiple consensus change proposition in one request
        let mut consensus_propose_added = false;
        // To prevent multiple service change proposition in one request
        let mut modified_instances = HashSet::new();
        // To prevent multiple services start in one request.
        let mut services_to_start = HashSet::new();
        // To prevent starting services with an unloaded artifact.
        let mut artifacts_for_started_services = HashSet::new();
        let mut unloaded_artifacts = HashSet::new();

        // Perform config verification.
        for change in changes {
            change.register_instance(&mut modified_instances)?;
            match change {
                ConfigChange::Consensus(config) => {
                    if consensus_propose_added {
                        let msg = "Discarded multiple consensus change proposals in one request";
                        return Err(ConfigurationError::malformed_propose(msg));
                    }
                    consensus_propose_added = true;
                    config
                        .validate()
                        .map_err(ConfigurationError::malformed_propose)?;
                }

                ConfigChange::Service(config) => {
                    context.verify_config(config.instance_id, config.params.clone())?;
                }

                ConfigChange::StartService(start_service) => {
                    if !services_to_start.insert(&start_service.name) {
                        let msg = format!(
                            "Discarded multiple starts of service `{}`",
                            start_service.name
                        );
                        return Err(ConfigurationError::malformed_propose(msg));
                    }
                    artifacts_for_started_services.insert(&start_service.artifact);
                    start_service.validate(context)?;
                }

                ConfigChange::StopService(stop_service) => {
                    stop_service.validate(context)?;
                }
                ConfigChange::ResumeService(resume_service) => {
                    resume_service.validate(context)?;
                }

                ConfigChange::FreezeService(freeze_service) => {
                    let instance_state = freeze_service.validate(context)?;
                    let runtime_id = instance_state.spec.artifact.runtime_id;
                    if !context
                        .supervisor_extensions()
                        .check_feature(runtime_id, &RuntimeFeature::FreezingServices)
                    {
                        let msg = format!(
                            "Cannot freeze service `{}`: runtime with ID {}, with which \
                             its artifact `{}` is associated, does not support service freezing",
                            instance_state.spec.as_descriptor(),
                            runtime_id,
                            instance_state.spec.artifact,
                        );
                        return Err(ConfigurationError::malformed_propose(msg));
                    }
                }

                ConfigChange::UnloadArtifact(unload_artifact) => {
                    if !unloaded_artifacts.insert(&unload_artifact.artifact_id) {
                        let msg = format!(
                            "Discarded multiple unloads of artifact `{}`",
                            unload_artifact.artifact_id
                        );
                        return Err(ConfigurationError::malformed_propose(msg));
                    }
                    unload_artifact.validate(context)?;
                }
            }
        }

        let mut intersection = unloaded_artifacts.intersection(&artifacts_for_started_services);
        if let Some(&artifact) = intersection.next() {
            let msg = format!(
                "Discarded proposal which both starts a service from artifact `{}` and unloads it",
                artifact
            );
            return Err(ConfigurationError::malformed_propose(msg));
        }

        Ok(())
    }

    /// Confirms a deploy by the given author's public key and checks
    /// if all the confirmations are collected. If so, starts the artifact registration.
    #[allow(clippy::unnecessary_wraps)]
    fn confirm_deploy(
        mut context: ExecutionContext<'_>,
        deploy_request: DeployRequest,
        author: PublicKey,
    ) -> Result<(), ExecutionError> {
        let core_schema = context.data().for_core();
        let mut schema = SchemaImpl::new(context.service_data());
        schema.deploy_confirmations.confirm(&deploy_request, author);

        // Check if we have enough confirmations for the deployment.
        let config = core_schema.consensus_config();
        let validator_keys = config.validator_keys.iter().map(|keys| keys.service_key);

        if schema
            .deploy_confirmations
            .intersect_with_validators(&deploy_request, validator_keys)
        {
            log::trace!(
                "Registering deployed artifact in dispatcher {:?}",
                deploy_request.artifact
            );

            // Remove artifact from pending deployments.
            schema
                .deploy_states
                .put(&deploy_request, AsyncEventState::Succeed);
            drop(schema);
            // We have enough confirmations to register the deployed artifact in the dispatcher;
            // if this action fails, this transaction will be canceled.
            context
                .supervisor_extensions()
                .start_artifact_registration(&deploy_request.artifact, deploy_request.spec);
        }
        Ok(())
    }

    /// Marks deployment as failed, discarding the further deployment steps.
    fn fail_deploy(
        context: &ExecutionContext<'_>,
        deploy_request: &DeployRequest,
        error: ExecutionError,
    ) {
        log::warn!(
            "Deploying artifact for request {:?} failed. Reason: {}",
            deploy_request,
            error
        );

        let height = context.data().for_core().height();
        let mut schema = SchemaImpl::new(context.service_data());

        // Mark deploy as failed.
        schema
            .deploy_states
            .put(deploy_request, AsyncEventState::Failed { height, error });

        // Remove artifact from pending deployments: since we require
        // a confirmation from every node, failure for one node means failure
        // for the whole network.
        schema.pending_deployments.remove(&deploy_request.artifact);
    }

    /// Confirms a local migration success by the given author's public key and checks
    /// if all the confirmations are collected. If so, commits the migration.
    /// If migration state hash differs from the expected one, migration fails though,
    /// and `fail_migration` method is invoked.
    fn confirm_migration(
        mut context: ExecutionContext<'_>,
        request: &MigrationRequest,
        state_hash: Hash,
        author: PublicKey,
    ) -> Result<(), ExecutionError> {
        let core_schema = context.data().for_core();
        let mut schema = SchemaImpl::new(context.service_data());
        let mut state = schema.migration_state_unchecked(request);

        // Verify that state hash does match expected one.
        if let Err(error) = state.add_state_hash(state_hash) {
            // Hashes do not match, rollback the migration.
            drop(schema); // Required for the context reborrow in `fail_migration`.
            let initiate_rollback = true;
            return Self::fail_migration(context, request, error, initiate_rollback);
        }

        // Hash is OK, process further.

        // Update state and add a confirmation.
        schema.migration_states.put(request, state.clone());
        schema.migration_confirmations.confirm(request, author);

        // Check if we have enough confirmations to finish the migration.
        let consensus_config = core_schema.consensus_config();
        let validator_keys = consensus_config
            .validator_keys
            .iter()
            .map(|keys| keys.service_key);

        if schema
            .migration_confirmations
            .intersect_with_validators(request, validator_keys)
        {
            log::trace!(
                "Confirming commit of migration request {:?}. Result state hash: {:?}",
                request,
                state_hash
            );

            // Schedule migration for a flush.
            // Migration will be flushed and marked as succeed in `before_transactions`
            // hook of the next block.
            schema.migration_states.put(request, state);
            schema.pending_migrations.remove(request);
            schema.migrations_to_flush.insert(request.clone());

            drop(schema);

            // Commit the migration.
            let supervisor_extensions = context.supervisor_extensions();
            supervisor_extensions.commit_migration(&request.service, state_hash)?;
        }
        Ok(())
    }

    /// Marks migration as failed, discarding the further migration steps.
    /// If `initiate_rollback` argument is `true`, ongoing migration will
    /// be rolled back after the invocation of this method.
    /// This argument is required, since migration can fail on the init step.
    fn fail_migration(
        mut context: ExecutionContext<'_>,
        request: &MigrationRequest,
        error: ExecutionError,
        initiate_rollback: bool,
    ) -> Result<(), ExecutionError> {
        if initiate_rollback {
            log::warn!(
                "Migration for a request {:?} failed. Reason: {}. \
                 This migration is going to be rolled back.",
                request,
                error
            );
        } else {
            log::warn!(
                "Migration for a request {:?} failed to start. Reason: {}.",
                request,
                error
            );
        }

        let height = context.data().for_core().height();
        let mut schema = SchemaImpl::new(context.service_data());

        // Mark deploy as failed.
        let mut state = schema.migration_state_unchecked(request);

        state.fail(AsyncEventState::Failed { height, error });
        schema.migration_states.put(request, state);

        // Migration is not pending anymore, remove it.
        schema.pending_migrations.remove(request);

        // Rollback the migration.
        drop(schema);
        if initiate_rollback {
            context
                .supervisor_extensions()
                .rollback_migration(&request.service)?;
        }

        Ok(())
    }
}
