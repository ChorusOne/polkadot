// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The Candidate Validation subsystem.
//!
//! This handles incoming requests from other subsystems to validate candidates
//! according to a validation function. This delegates validation to an underlying
//! pool of processes used for execution of the Wasm.

use polkadot_subsystem::{
	Subsystem, SubsystemContext, SpawnedSubsystem, SubsystemResult,
	FromOverseer, OverseerSignal,
};
use polkadot_subsystem::messages::{
	AllMessages, CandidateValidationMessage, RuntimeApiMessage, ValidationFailed, RuntimeApiRequest,
	RuntimeApiError,
};
use polkadot_node_primitives::{ValidationResult, ValidationOutputs};
use polkadot_primitives::v1::{
	ValidationCode, OmittedValidationData, PoV, CandidateDescriptor, LocalValidationData,
	GlobalValidationData, OccupiedCoreAssumption, Hash, validation_data_hash,
};
use polkadot_parachain::wasm_executor::{self, ValidationPool, ExecutionMode};
use polkadot_parachain::primitives::{ValidationResult as WasmValidationResult, ValidationParams};

use parity_scale_codec::Encode;
use sp_core::traits::SpawnNamed;

use futures::channel::oneshot;
use futures::prelude::*;

use std::sync::Arc;

/// The candidate validation subsystem.
pub struct CandidateValidationSubsystem<S>(S);

impl<S> CandidateValidationSubsystem<S> {
	/// Create a new `CandidateValidationSubsystem` with the given task spawner.
	pub fn new(spawn: S) -> Self {
		CandidateValidationSubsystem(spawn)
	}
}

impl<S, C> Subsystem<C> for CandidateValidationSubsystem<S> where
	C: SubsystemContext<Message = CandidateValidationMessage>,
	S: SpawnNamed + Clone + 'static,
{
	fn start(self, ctx: C) -> SpawnedSubsystem {
		SpawnedSubsystem {
			name: "candidate-validation-subsystem",
			future: run(ctx, self.0).map(|_| ()).boxed(),
		}
	}
}

async fn run(
	mut ctx: impl SubsystemContext<Message = CandidateValidationMessage>,
	spawn: impl SpawnNamed + Clone + 'static,
)
	-> SubsystemResult<()>
{
	let pool = ValidationPool::new();

	loop {
		match ctx.recv().await? {
			FromOverseer::Signal(OverseerSignal::ActiveLeaves(_)) => {}
			FromOverseer::Signal(OverseerSignal::BlockFinalized(_)) => {}
			FromOverseer::Signal(OverseerSignal::Conclude) => return Ok(()),
			FromOverseer::Communication { msg } => match msg {
				CandidateValidationMessage::ValidateFromChainState(
					descriptor,
					pov,
					response_sender,
				) => {
					let res = spawn_validate_from_chain_state(
						&mut ctx,
						Some(pool.clone()),
						descriptor,
						pov,
						spawn.clone(),
					).await;

					match res {
						Ok(x) => { let _ = response_sender.send(x); }
						Err(e)=> return Err(e),
					}
				}
				CandidateValidationMessage::ValidateFromExhaustive(
					omitted_validation,
					validation_code,
					descriptor,
					pov,
					response_sender,
				) => {
					let res = spawn_validate_exhaustive(
						&mut ctx,
						Some(pool.clone()),
						omitted_validation,
						validation_code,
						descriptor,
						pov,
						spawn.clone(),
					).await;

					match res {
						Ok(x) => if let Err(_e) = response_sender.send(x) {
							log::warn!(
								target: "candidate_validation",
								"Requester of candidate validation dropped",
							)
						},
						Err(e)=> return Err(e),
					}
				}
			}
		}
	}
}

async fn runtime_api_request<T>(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	relay_parent: Hash,
	request: RuntimeApiRequest,
	receiver: oneshot::Receiver<Result<T, RuntimeApiError>>,
) -> SubsystemResult<Result<T, RuntimeApiError>> {
	ctx.send_message(
		AllMessages::RuntimeApi(RuntimeApiMessage::Request(
			relay_parent,
			request,
		))
	).await?;

	receiver.await.map_err(Into::into)
}

#[derive(Debug)]
enum AssumptionCheckOutcome {
	Matches(OmittedValidationData, ValidationCode),
	DoesNotMatch,
	BadRequest,
}

async fn check_assumption_validation_data(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	descriptor: &CandidateDescriptor,
	global_validation_data: &GlobalValidationData,
	assumption: OccupiedCoreAssumption,
) -> SubsystemResult<AssumptionCheckOutcome> {
	let local_validation_data = {
		let (tx, rx) = oneshot::channel();
		let d = runtime_api_request(
			ctx,
			descriptor.relay_parent,
			RuntimeApiRequest::LocalValidationData(
				descriptor.para_id,
				assumption,
				tx,
			),
			rx,
		).await?;

		match d {
			Ok(None) | Err(_) => {
				return Ok(AssumptionCheckOutcome::BadRequest);
			}
			Ok(Some(d)) => d,
		}
	};

	let validation_data_hash = validation_data_hash(
		&global_validation_data,
		&local_validation_data,
	);

	SubsystemResult::Ok(if descriptor.validation_data_hash == validation_data_hash {
		let omitted_validation = OmittedValidationData {
			global_validation: global_validation_data.clone(),
			local_validation: local_validation_data,
		};

		let (code_tx, code_rx) = oneshot::channel();
		let validation_code = runtime_api_request(
			ctx,
			descriptor.relay_parent,
			RuntimeApiRequest::ValidationCode(
				descriptor.para_id,
				OccupiedCoreAssumption::Included,
				code_tx,
			),
			code_rx,
		).await?;

		match validation_code {
			Ok(None) | Err(_) => AssumptionCheckOutcome::BadRequest,
			Ok(Some(v)) => AssumptionCheckOutcome::Matches(omitted_validation, v),
		}
	} else {
		AssumptionCheckOutcome::DoesNotMatch
	})
}

async fn spawn_validate_from_chain_state(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	validation_pool: Option<ValidationPool>,
	descriptor: CandidateDescriptor,
	pov: Arc<PoV>,
	spawn: impl SpawnNamed + 'static,
) -> SubsystemResult<Result<ValidationResult, ValidationFailed>> {
	// The candidate descriptor has a `validation_data_hash` which corresponds to
	// one of up to two possible values that we can derive from the state of the
	// relay-parent. We can fetch these values by getting the `global_validation_data`,
	// and both `local_validation_data` based on the different `OccupiedCoreAssumption`s.
	let global_validation_data = {
		let (tx, rx) = oneshot::channel();
		let res = runtime_api_request(
			ctx,
			descriptor.relay_parent,
			RuntimeApiRequest::GlobalValidationData(tx),
			rx,
		).await?;

		match res {
			Ok(g) => g,
			Err(e) => {
				log::warn!(
					target: "candidate_validation",
					"Error making runtime API request: {:?}",
					e,
				);

				return Ok(Err(ValidationFailed));
			}
		}
	};

	match check_assumption_validation_data(
		ctx,
		&descriptor,
		&global_validation_data,
		OccupiedCoreAssumption::Included,
	).await? {
		AssumptionCheckOutcome::Matches(omitted_validation, validation_code) => {
			return spawn_validate_exhaustive(
				ctx,
				validation_pool,
				omitted_validation,
				validation_code,
				descriptor,
				pov,
				spawn,
			).await;
		}
		AssumptionCheckOutcome::DoesNotMatch => {},
		AssumptionCheckOutcome::BadRequest => return Ok(Err(ValidationFailed)),
	}

	match check_assumption_validation_data(
		ctx,
		&descriptor,
		&global_validation_data,
		OccupiedCoreAssumption::TimedOut,
	).await? {
		AssumptionCheckOutcome::Matches(omitted_validation, validation_code) => {
			return spawn_validate_exhaustive(
				ctx,
				validation_pool,
				omitted_validation,
				validation_code,
				descriptor,
				pov,
				spawn,
			).await;
		}
		AssumptionCheckOutcome::DoesNotMatch => {},
		AssumptionCheckOutcome::BadRequest => return Ok(Err(ValidationFailed)),
	}

	// If neither the assumption of the occupied core having the para included or the assumption
	// of the occupied core timing out are valid, then the validation_data_hash in the descriptor
	// is not based on the relay parent and is thus invalid.
	Ok(Ok(ValidationResult::Invalid))
}

async fn spawn_validate_exhaustive(
	ctx: &mut impl SubsystemContext<Message = CandidateValidationMessage>,
	validation_pool: Option<ValidationPool>,
	omitted_validation: OmittedValidationData,
	validation_code: ValidationCode,
	descriptor: CandidateDescriptor,
	pov: Arc<PoV>,
	spawn: impl SpawnNamed + 'static,
) -> SubsystemResult<Result<ValidationResult, ValidationFailed>> {
	let (tx, rx) = oneshot::channel();
	let fut = async move {
		let res = validate_candidate_exhaustive::<RealValidationBackend, _>(
			validation_pool,
			omitted_validation,
			validation_code,
			descriptor,
			pov,
			spawn,
		);

		let _ = tx.send(res);
	};

	ctx.spawn("blocking-candidate-validation-task", fut.boxed()).await?;
	rx.await.map_err(Into::into)
}

/// Does basic checks of a candidate. Provide the encoded PoV-block. Returns `true` if basic checks
/// are passed, false otherwise.
fn passes_basic_checks(
	candidate: &CandidateDescriptor,
	max_block_data_size: Option<u64>,
	pov: &PoV,
) -> bool {
	let encoded_pov = pov.encode();
	let hash = pov.hash();

	if let Some(max_size) = max_block_data_size {
		if encoded_pov.len() as u64 > max_size {
			return false;
		}
	}

	if hash != candidate.pov_hash {
		return false;
	}

	if let Err(()) = candidate.check_collator_signature() {
		return false;
	}

	true
}

/// Check the result of Wasm execution against the constraints given by the relay-chain.
///
/// Returns `true` if checks pass, false otherwise.
fn check_wasm_result_against_constraints(
	global_validation_data: &GlobalValidationData,
	_local_validation_data: &LocalValidationData,
	result: &WasmValidationResult,
) -> bool {
	if result.head_data.0.len() > global_validation_data.max_head_data_size as _ {
		return false
	}

	if let Some(ref code) = result.new_validation_code {
		if code.0.len() > global_validation_data.max_code_size as _ {
			return false
		}
	}

	true
}

trait ValidationBackend {
	type Arg;

	fn validate<S: SpawnNamed + 'static>(
		arg: Self::Arg,
		validation_code: &ValidationCode,
		params: ValidationParams,
		spawn: S,
	) -> Result<WasmValidationResult, wasm_executor::Error>;
}

struct RealValidationBackend;

impl ValidationBackend for RealValidationBackend {
	type Arg = Option<ValidationPool>;

	fn validate<S: SpawnNamed + 'static>(
		pool: Option<ValidationPool>,
		validation_code: &ValidationCode,
		params: ValidationParams,
		spawn: S,
	) -> Result<WasmValidationResult, wasm_executor::Error> {
		let execution_mode = pool.as_ref()
			.map(ExecutionMode::Remote)
			.unwrap_or(ExecutionMode::Local);

		wasm_executor::validate_candidate(
			&validation_code.0,
			params,
			execution_mode,
			spawn,
		)
	}
}

/// Validates the candidate from exhaustive parameters.
///
/// Sends the result of validation on the channel once complete.
fn validate_candidate_exhaustive<B: ValidationBackend, S: SpawnNamed + 'static>(
	backend_arg: B::Arg,
	omitted_validation: OmittedValidationData,
	validation_code: ValidationCode,
	descriptor: CandidateDescriptor,
	pov: Arc<PoV>,
	spawn: S,
) -> Result<ValidationResult, ValidationFailed> {
	if !passes_basic_checks(&descriptor, None, &*pov) {
		return Ok(ValidationResult::Invalid);
	}

	let OmittedValidationData { global_validation, local_validation } = omitted_validation;

	let params = ValidationParams {
		parent_head: local_validation.parent_head.clone(),
		block_data: pov.block_data.clone(),
		max_code_size: global_validation.max_code_size,
		max_head_data_size: global_validation.max_head_data_size,
		relay_chain_height: global_validation.block_number,
		code_upgrade_allowed: local_validation.code_upgrade_allowed,
	};

	match B::validate(backend_arg, &validation_code, params, spawn) {
		Err(wasm_executor::Error::BadReturn) => Ok(ValidationResult::Invalid),
		Err(_) => Err(ValidationFailed),
		Ok(res) => {
			let passes_post_checks = check_wasm_result_against_constraints(
				&global_validation,
				&local_validation,
				&res,
			);

			Ok(if passes_post_checks {
				ValidationResult::Valid(ValidationOutputs {
					head_data: res.head_data,
					global_validation_data: global_validation,
					local_validation_data: local_validation,
					upward_messages: res.upward_messages,
					fees: 0,
					new_validation_code: res.new_validation_code,
				})
			} else {
				ValidationResult::Invalid
			})
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use polkadot_subsystem::test_helpers;
	use polkadot_primitives::v1::{HeadData, BlockData};
	use sp_core::testing::TaskExecutor;
	use futures::executor;
	use assert_matches::assert_matches;
	use sp_keyring::Sr25519Keyring;

	struct MockValidationBackend;

	struct MockValidationArg {
		result: Result<WasmValidationResult, wasm_executor::Error>,
	}

	impl ValidationBackend for MockValidationBackend {
		type Arg = MockValidationArg;

		fn validate<S: SpawnNamed + 'static>(
			arg: Self::Arg,
			_validation_code: &ValidationCode,
			_params: ValidationParams,
			_spawn: S,
		) -> Result<WasmValidationResult, wasm_executor::Error> {
			arg.result
		}
	}

	fn collator_sign(descriptor: &mut CandidateDescriptor, collator: Sr25519Keyring) {
		descriptor.collator = collator.public().into();
		let payload = polkadot_primitives::v1::collator_signature_payload(
			&descriptor.relay_parent,
			&descriptor.para_id,
			&descriptor.validation_data_hash,
			&descriptor.pov_hash,
		);

		descriptor.signature = collator.sign(&payload[..]).into();
		assert!(descriptor.check_collator_signature().is_ok());
	}

	#[test]
	fn correctly_checks_included_assumption() {
		let local_validation_data = LocalValidationData::default();
		let global_validation_data = GlobalValidationData::default();
		let validation_code: ValidationCode = vec![1, 2, 3].into();

		let validation_data_hash = validation_data_hash(&global_validation_data, &local_validation_data);
		let relay_parent = [2; 32].into();
		let para_id = 5.into();

		let mut candidate = CandidateDescriptor::default();
		candidate.relay_parent = relay_parent;
		candidate.validation_data_hash = validation_data_hash;
		candidate.para_id = para_id;

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

		let (check_fut, check_result) = check_assumption_validation_data(
			&mut ctx,
			&candidate,
			&global_validation_data,
			OccupiedCoreAssumption::Included,
		).remote_handle();

		let global_validation_data = global_validation_data.clone();
		let test_fut = async move {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::LocalValidationData(p, OccupiedCoreAssumption::Included, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(Some(local_validation_data.clone())));
				}
			);

			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::Included, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(Some(validation_code.clone())));
				}
			);

			assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::Matches(o, v) => {
				assert_eq!(o, OmittedValidationData {
					local_validation: local_validation_data,
					global_validation: global_validation_data,
				});

				assert_eq!(v, validation_code);
			});
		};

		let test_fut = future::join(test_fut, check_fut);
		executor::block_on(test_fut);
	}

	#[test]
	fn correctly_checks_timed_out_assumption() {
		let local_validation_data = LocalValidationData::default();
		let global_validation_data = GlobalValidationData::default();
		let validation_code: ValidationCode = vec![1, 2, 3].into();

		let validation_data_hash = validation_data_hash(&global_validation_data, &local_validation_data);
		let relay_parent = [2; 32].into();
		let para_id = 5.into();

		let mut candidate = CandidateDescriptor::default();
		candidate.relay_parent = relay_parent;
		candidate.validation_data_hash = validation_data_hash;
		candidate.para_id = para_id;

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

		let (check_fut, check_result) = check_assumption_validation_data(
			&mut ctx,
			&candidate,
			&global_validation_data,
			OccupiedCoreAssumption::TimedOut,
		).remote_handle();

		let global_validation_data = global_validation_data.clone();
		let test_fut = async move {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::LocalValidationData(p, OccupiedCoreAssumption::TimedOut, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(Some(local_validation_data.clone())));
				}
			);

			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::Included, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(Some(validation_code.clone())));
				}
			);

			assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::Matches(o, v) => {
				assert_eq!(o, OmittedValidationData {
					local_validation: local_validation_data,
					global_validation: global_validation_data,
				});

				assert_eq!(v, validation_code);
			});
		};

		let test_fut = future::join(test_fut, check_fut);
		executor::block_on(test_fut);
	}

	#[test]
	fn check_is_bad_request_if_no_validation_data() {
		let local_validation_data = LocalValidationData::default();
		let global_validation_data = GlobalValidationData::default();

		let validation_data_hash = validation_data_hash(&global_validation_data, &local_validation_data);
		let relay_parent = [2; 32].into();
		let para_id = 5.into();

		let mut candidate = CandidateDescriptor::default();
		candidate.relay_parent = relay_parent;
		candidate.validation_data_hash = validation_data_hash;
		candidate.para_id = para_id;

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

		let (check_fut, check_result) = check_assumption_validation_data(
			&mut ctx,
			&candidate,
			&global_validation_data,
			OccupiedCoreAssumption::Included,
		).remote_handle();

		let test_fut = async move {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::LocalValidationData(p, OccupiedCoreAssumption::Included, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(None));
				}
			);

			assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::BadRequest);
		};

		let test_fut = future::join(test_fut, check_fut);
		executor::block_on(test_fut);
	}

	#[test]
	fn check_is_bad_request_if_no_validation_code() {
		let local_validation_data = LocalValidationData::default();
		let global_validation_data = GlobalValidationData::default();

		let validation_data_hash = validation_data_hash(&global_validation_data, &local_validation_data);
		let relay_parent = [2; 32].into();
		let para_id = 5.into();

		let mut candidate = CandidateDescriptor::default();
		candidate.relay_parent = relay_parent;
		candidate.validation_data_hash = validation_data_hash;
		candidate.para_id = para_id;

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

		let (check_fut, check_result) = check_assumption_validation_data(
			&mut ctx,
			&candidate,
			&global_validation_data,
			OccupiedCoreAssumption::TimedOut,
		).remote_handle();

		let test_fut = async move {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::LocalValidationData(p, OccupiedCoreAssumption::TimedOut, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(Some(local_validation_data.clone())));
				}
			);

			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::ValidationCode(p, OccupiedCoreAssumption::Included, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(None));
				}
			);

			assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::BadRequest);
		};

		let test_fut = future::join(test_fut, check_fut);
		executor::block_on(test_fut);
	}

	#[test]
	fn check_does_not_match() {
		let local_validation_data = LocalValidationData::default();
		let global_validation_data = GlobalValidationData::default();

		let relay_parent = [2; 32].into();
		let para_id = 5.into();

		let mut candidate = CandidateDescriptor::default();
		candidate.relay_parent = relay_parent;
		candidate.validation_data_hash = [3; 32].into();
		candidate.para_id = para_id;

		let pool = TaskExecutor::new();
		let (mut ctx, mut ctx_handle) = test_helpers::make_subsystem_context(pool.clone());

		let (check_fut, check_result) = check_assumption_validation_data(
			&mut ctx,
			&candidate,
			&global_validation_data,
			OccupiedCoreAssumption::Included,
		).remote_handle();

		let test_fut = async move {
			assert_matches!(
				ctx_handle.recv().await,
				AllMessages::RuntimeApi(RuntimeApiMessage::Request(
					rp,
					RuntimeApiRequest::LocalValidationData(p, OccupiedCoreAssumption::Included, tx)
				)) => {
					assert_eq!(rp, relay_parent);
					assert_eq!(p, para_id);

					let _ = tx.send(Ok(Some(local_validation_data.clone())));
				}
			);

			assert_matches!(check_result.await.unwrap(), AssumptionCheckOutcome::DoesNotMatch);
		};

		let test_fut = future::join(test_fut, check_fut);
		executor::block_on(test_fut);
	}

	#[test]
	fn candidate_validation_ok_is_ok() {
		let mut omitted_validation = OmittedValidationData {
			local_validation: Default::default(),
			global_validation: Default::default(),
		};

		omitted_validation.global_validation.max_head_data_size = 1024;
		omitted_validation.global_validation.max_code_size = 1024;

		let pov = PoV { block_data: BlockData(vec![1; 32]) };

		let mut descriptor = CandidateDescriptor::default();
		descriptor.pov_hash = pov.hash();
		collator_sign(&mut descriptor, Sr25519Keyring::Alice);

		assert!(passes_basic_checks(&descriptor, Some(1024), &pov));

		let validation_result = WasmValidationResult {
			head_data: HeadData(vec![1, 1, 1]),
			new_validation_code: Some(vec![2, 2, 2].into()),
			upward_messages: Vec::new(),
			processed_downward_messages: 0,
		};

		assert!(check_wasm_result_against_constraints(
			&omitted_validation.global_validation,
			&omitted_validation.local_validation,
			&validation_result,
		));

		let v = validate_candidate_exhaustive::<MockValidationBackend, _>(
			MockValidationArg { result: Ok(validation_result) },
			omitted_validation.clone(),
			vec![1, 2, 3].into(),
			descriptor,
			Arc::new(pov),
			TaskExecutor::new(),
		).unwrap();

		assert_matches!(v, ValidationResult::Valid(outputs) => {
			assert_eq!(outputs.head_data, HeadData(vec![1, 1, 1]));
			assert_eq!(outputs.global_validation_data, omitted_validation.global_validation);
			assert_eq!(outputs.local_validation_data, omitted_validation.local_validation);
			assert_eq!(outputs.upward_messages, Vec::new());
			assert_eq!(outputs.fees, 0);
			assert_eq!(outputs.new_validation_code, Some(vec![2, 2, 2].into()));
		});
	}

	#[test]
	fn candidate_validation_bad_return_is_invalid() {
		let mut omitted_validation = OmittedValidationData {
			local_validation: Default::default(),
			global_validation: Default::default(),
		};

		omitted_validation.global_validation.max_head_data_size = 1024;
		omitted_validation.global_validation.max_code_size = 1024;

		let pov = PoV { block_data: BlockData(vec![1; 32]) };

		let mut descriptor = CandidateDescriptor::default();
		descriptor.pov_hash = pov.hash();
		collator_sign(&mut descriptor, Sr25519Keyring::Alice);

		assert!(passes_basic_checks(&descriptor, Some(1024), &pov));

		let validation_result = WasmValidationResult {
			head_data: HeadData(vec![1, 1, 1]),
			new_validation_code: Some(vec![2, 2, 2].into()),
			upward_messages: Vec::new(),
			processed_downward_messages: 0,
		};

		assert!(check_wasm_result_against_constraints(
			&omitted_validation.global_validation,
			&omitted_validation.local_validation,
			&validation_result,
		));

		let v = validate_candidate_exhaustive::<MockValidationBackend, _>(
			MockValidationArg { result: Err(wasm_executor::Error::BadReturn) },
			omitted_validation.clone(),
			vec![1, 2, 3].into(),
			descriptor,
			Arc::new(pov),
			TaskExecutor::new(),
		).unwrap();

		assert_matches!(v, ValidationResult::Invalid);
	}


	#[test]
	fn candidate_validation_timeout_is_internal_error() {
		let mut omitted_validation = OmittedValidationData {
			local_validation: Default::default(),
			global_validation: Default::default(),
		};

		omitted_validation.global_validation.max_head_data_size = 1024;
		omitted_validation.global_validation.max_code_size = 1024;

		let pov = PoV { block_data: BlockData(vec![1; 32]) };

		let mut descriptor = CandidateDescriptor::default();
		descriptor.pov_hash = pov.hash();
		collator_sign(&mut descriptor, Sr25519Keyring::Alice);

		assert!(passes_basic_checks(&descriptor, Some(1024), &pov));

		let validation_result = WasmValidationResult {
			head_data: HeadData(vec![1, 1, 1]),
			new_validation_code: Some(vec![2, 2, 2].into()),
			upward_messages: Vec::new(),
			processed_downward_messages: 0,
		};

		assert!(check_wasm_result_against_constraints(
			&omitted_validation.global_validation,
			&omitted_validation.local_validation,
			&validation_result,
		));

		let v = validate_candidate_exhaustive::<MockValidationBackend, _>(
			MockValidationArg { result: Err(wasm_executor::Error::Timeout) },
			omitted_validation.clone(),
			vec![1, 2, 3].into(),
			descriptor,
			Arc::new(pov),
			TaskExecutor::new(),
		);

		assert_matches!(v, Err(ValidationFailed));
	}
}
