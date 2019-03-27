// Copyright 2018-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <http://www.gnu.org/licenses/>.

//! # Contract Module
//! 
//! The contract module provides functionality for the runtime to deploy and execute WebAssembly smart-contracts.
//! The supported dispatchable functions are documented as part of the [`Call`](./enum.Call.html) enum.
//! 
//! ## Overview
//! 
//! This module extends accounts (see `Balances` module) to have smart-contract functionality.
//! These "smart-contract accounts" have the ability to create smart-contracts and make calls to other contract 
//! and non-contract accounts.
//! 
//! The smart-contract code is stored once in a `code_cache`, and later retrievable via its `code_hash`.
//! This means that multiple smart-contracts can be instantiated from the same `code_cache`, without replicating 
//! the code each time.
//! 
//! When a smart-contract is called, its associated code is retrieved via the code hash and gets executed.
//! This call can alter the storage entries of the smart-contract account, create new smart-contracts,
//! or call other smart-contracts.
//! 
//! Finally, when the `Balances` module determines an account is dead (i.e. account balance fell below the
//! existential deposit), it reaps the account. This will delete the associated code and storage of the 
//! smart-contract account.
//! 
//! ### Gas
//! 
//! Senders must specify a gas limit with every call, as all instructions invoked by the smart-contract require gas.
//! Unused gas is refunded after the call, regardless of the execution outcome.
//! 
//! If the gas limit is reached, then all calls and state changes (including balance transfers) are only
//! reverted at the current call's contract level. For example, if contract A calls B and B runs out of gas mid-call,
//! then all of B's calls are reverted. Assuming correct error handling by contract A, A's other calls and state 
//! changes still persist. 
//! 
//! ### Notable Scenarios
//! 
//! Contract call failures are not always cascading. When failures occur in a sub-call, they do not "bubble up",
//! and the call will only revert at the specific contract level. For example, if contract A calls contract B, and B
//! fails, A can decide how to handle that failure, either proceeding or reverting A's changes.
//! 
//! ## Interface
//! 
//! ### Dispatchable functions
//! 
//! * `put_code` - Stores the given binary Wasm code into the chains storage and returns its `codehash`.
//! 
//! * `create` - Deploys a new contract from the given `codehash`, optionally transferring some balance. 
//! This creates a new smart contract account and calls its contract deploy handler to initialize the contract.
//! 
//! * `call` - Makes a call to an account, optionally transferring some balance.
//!
//! ### Public functions
//! 
//! See the [module](./struct.Module.html) for details on publicly available functions.
//! 
//! ## Usage
//! 
//! The contract module is a work in progress. The following examples show how this contract module can be 
//! used to create and call contracts.
//! 
//! * [`pDSL`](https://github.com/Robbepop/pdsl) is a domain specific language which enables writing 
//! WebAssembly based smart contracts in the Rust programming language. This is a work in progress.
//! 
//! ## Related Modules
//! * [`Balances`](https://crates.parity.io/srml_balances/index.html)
//! 

#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
mod gas;

mod account_db;
mod exec;
mod wasm;

#[cfg(test)]
mod tests;

use crate::exec::ExecutionContext;
use crate::account_db::{AccountDb, DirectAccountDb};

#[cfg(feature = "std")]
use serde_derive::{Serialize, Deserialize};
use substrate_primitives::crypto::UncheckedFrom;
use rstd::prelude::*;
use rstd::marker::PhantomData;
use parity_codec::{Codec, Encode, Decode};
use runtime_primitives::traits::{Hash, As, SimpleArithmetic,Bounded, StaticLookup};
use srml_support::dispatch::{Result, Dispatchable};
use srml_support::{Parameter, StorageMap, StorageValue, decl_module, decl_event, decl_storage, storage::child};
use srml_support::traits::{OnFreeBalanceZero, OnUnbalanced};
use system::{ensure_signed, RawOrigin};
use timestamp;

pub type CodeHash<T> = <T as system::Trait>::Hash;
pub type TrieId = Vec<u8>;

/// A function that generates an `AccountId` for a contract upon instantiation.
pub trait ContractAddressFor<CodeHash, AccountId> {
	fn contract_address_for(code_hash: &CodeHash, data: &[u8], origin: &AccountId) -> AccountId;
}

/// A function that returns the fee for dispatching a `Call`.
pub trait ComputeDispatchFee<Call, Balance> {
	fn compute_dispatch_fee(call: &Call) -> Balance;
}

#[derive(Encode,Decode,Clone,Debug)]
/// Information for managing an acocunt and its sub trie abstraction.
/// This is the required info to cache for an account
pub struct AccountInfo {
	/// unique ID for the subtree encoded as a byte
	pub trie_id: TrieId,
	/// the size of stored value in octet
	pub current_mem_stored: u64,
}

/// Get a trie id (trie id must be unique and collision resistant depending upon its context)
/// Note that it is different than encode because trie id should have collision resistance
/// property (being a proper uniqueid).
pub trait TrieIdGenerator<AccountId> {
	/// get a trie id for an account, using reference to parent account trie id to ensure
	/// uniqueness of trie id 
	/// The implementation must ensure every new trie id is unique: two consecutive call with the
	/// same parameter needs to return different trie id values.
	fn trie_id(account_id: &AccountId) -> TrieId;
}

/// Get trie id from `account_id`
pub struct TrieIdFromParentCounter<T: Trait>(PhantomData<T>);

/// This generator use inner counter for account id and apply hash over `AccountId +
/// accountid_counter`
impl<T: Trait> TrieIdGenerator<T::AccountId> for TrieIdFromParentCounter<T>
where
	T::AccountId: AsRef<[u8]>
{
	fn trie_id(account_id: &T::AccountId) -> TrieId {
		// note that skipping a value due to error is not an issue here.
		// we only need uniqueness, not sequence.
		let new_seed = <AccountCounter<T>>::mutate(|v| v.wrapping_add(1));

		let mut buf = Vec::new();
		buf.extend_from_slice(account_id.as_ref());
		buf.extend_from_slice(&new_seed.to_le_bytes()[..]);
		T::Hashing::hash(&buf[..]).as_ref().into()
	}
}

pub trait Trait: balances::Trait + timestamp::Trait {
	/// The outer call dispatch type.
	type Call: Parameter + Dispatchable<Origin=<Self as system::Trait>::Origin>;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	// As<u32> is needed for wasm-utils
	type Gas: Parameter + Default + Codec + SimpleArithmetic + Bounded + Copy + As<Self::Balance> + As<u64> + As<u32>;

	/// A function type to get the contract address given the creator.
	type DetermineContractAddress: ContractAddressFor<CodeHash<Self>, Self::AccountId>;

	/// A function type that computes the fee for dispatching the given `Call`.
	///
	/// It is recommended (though not required) for this function to return a fee that would be taken
	/// by executive module for regular dispatch.
	type ComputeDispatchFee: ComputeDispatchFee<Self::Call, <Self as balances::Trait>::Balance>;
	/// trieid id generator
	type TrieIdGenerator: TrieIdGenerator<Self::AccountId>;

	/// Handler for the unbalanced reduction when making a gas payment.
	type GasPayment: OnUnbalanced<balances::NegativeImbalance<Self>>;
}

/// Simple contract address determintator.
///
/// Address calculated from the code (of the constructor), input data to the constructor
/// and account id which requested the account creation.
///
/// Formula: `blake2_256(blake2_256(code) + blake2_256(data) + origin)`
pub struct SimpleAddressDeterminator<T: Trait>(PhantomData<T>);
impl<T: Trait> ContractAddressFor<CodeHash<T>, T::AccountId> for SimpleAddressDeterminator<T>
where
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>
{
	fn contract_address_for(code_hash: &CodeHash<T>, data: &[u8], origin: &T::AccountId) -> T::AccountId {
		let data_hash = T::Hashing::hash(data);

		let mut buf = Vec::new();
		buf.extend_from_slice(code_hash.as_ref());
		buf.extend_from_slice(data_hash.as_ref());
		buf.extend_from_slice(origin.as_ref());

		UncheckedFrom::unchecked_from(T::Hashing::hash(&buf[..]))
	}
}

/// The default dispatch fee computor computes the fee in the same way that
/// implementation of `MakePayment` for balances module does.
pub struct DefaultDispatchFeeComputor<T: Trait>(PhantomData<T>);
impl<T: Trait> ComputeDispatchFee<T::Call, T::Balance> for DefaultDispatchFeeComputor<T> {
	fn compute_dispatch_fee(call: &T::Call) -> T::Balance {
		let encoded_len = call.using_encoded(|encoded| encoded.len());
		let base_fee = <balances::Module<T>>::transaction_base_fee();
		let byte_fee = <balances::Module<T>>::transaction_byte_fee();
		base_fee + byte_fee * <T::Balance as As<u64>>::sa(encoded_len as u64)
	}
}

decl_module! {
	/// Contracts module.
	pub struct Module<T: Trait> for enum Call where origin: <T as system::Trait>::Origin {
		fn deposit_event<T>() = default;

		/// Updates the schedule for metering contracts.
		///
		/// The schedule must have a greater version than the stored schedule.
		fn update_schedule(schedule: Schedule<T::Gas>) -> Result {
			if <Module<T>>::current_schedule().version >= schedule.version {
				return Err("new schedule must have a greater version than current");
			}

			Self::deposit_event(RawEvent::ScheduleUpdated(schedule.version));
			<CurrentSchedule<T>>::put(schedule);

			Ok(())
		}

		/// Stores the given binary Wasm code into the chains storage and returns its `codehash`. 
		/// You can instantiate contracts only with stored code.
		fn put_code(
			origin,
			#[compact] gas_limit: T::Gas,
			code: Vec<u8>
		) -> Result {
			let origin = ensure_signed(origin)?;
			let schedule = <Module<T>>::current_schedule();

			let (mut gas_meter, imbalance) = gas::buy_gas::<T>(&origin, gas_limit)?;

			let result = wasm::save_code::<T>(code, &mut gas_meter, &schedule);
			if let Ok(code_hash) = result {
				Self::deposit_event(RawEvent::CodeStored(code_hash));
			}

			gas::refund_unused_gas::<T>(&origin, gas_meter, imbalance);

			result.map(|_| ())
		}

		/// Makes a call to an account, optionally transferring some balance.
		///
		/// * If the account is a smart-contract account, the associated code will be 
		/// executed and any balance will be transferred.
		/// * If the account is a regular account, any fees will be transferred.
		/// * If no account exists (and the call value fulfills the `existential_deposit`), 
		/// a regular account will be created and any fees will be transferred.
		fn call(
			origin,
			dest: <T::Lookup as StaticLookup>::Source,
			#[compact] value: T::Balance,
			#[compact] gas_limit: T::Gas,
			data: Vec<u8>
		) -> Result {
			let origin = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(dest)?;

			// Pay for the gas upfront.
			//
			// NOTE: it is very important to avoid any state changes before
			// paying for the gas.
			let (mut gas_meter, imbalance) = gas::buy_gas::<T>(&origin, gas_limit)?;

			let cfg = Config::preload();
			let vm = crate::wasm::WasmVm::new(&cfg.schedule);
			let loader = crate::wasm::WasmLoader::new(&cfg.schedule);
			let mut ctx = ExecutionContext::top_level(origin.clone(), &cfg, &vm, &loader);

			let result = ctx.call(dest, value, &mut gas_meter, &data, exec::EmptyOutputBuf::new());

			if let Ok(_) = result {
				// Commit all changes that made it thus far into the persistant storage.
				DirectAccountDb.commit(ctx.overlay.into_change_set());

				// Then deposit all events produced.
				ctx.events.into_iter().for_each(Self::deposit_event);
			}

			// Refund cost of the unused gas.
			//
			// NOTE: this should go after the commit to the storage, since the storage changes
			// can alter the balance of the caller.
			gas::refund_unused_gas::<T>(&origin, gas_meter, imbalance);

			// Dispatch every recorded call with an appropriate origin.
			ctx.calls.into_iter().for_each(|(who, call)| {
				let result = call.dispatch(RawOrigin::Signed(who.clone()).into());
				Self::deposit_event(RawEvent::Dispatched(who, result.is_ok()));
			});

			result.map(|_| ())
		}

		/// Creates a new contract from the `codehash` generated by PUT_CODE, optionally transferring some balance.
		///
		/// Creation is executed as follows:
		///
		/// - the destination address is computed based on the sender and hash of the code.
		/// - the smart-contract account is created at the computed address.
		/// - the `ctor_code` is executed in the context of the newly created account. Buffer returned
		///   after the execution is saved as the `code` of the account. That code will be invoked
		///   upon any call received by this account.
		/// - the contract is initialized
		fn create(
			origin,
			#[compact] endowment: T::Balance,
			#[compact] gas_limit: T::Gas,
			code_hash: CodeHash<T>,
			data: Vec<u8>
		) -> Result {
			let origin = ensure_signed(origin)?;

			// Commit the gas upfront.
			//
			// NOTE: it is very important to avoid any state changes before
			// paying for the gas.
			let (mut gas_meter, imbalance) = gas::buy_gas::<T>(&origin, gas_limit)?;

			let cfg = Config::preload();
			let vm = crate::wasm::WasmVm::new(&cfg.schedule);
			let loader = crate::wasm::WasmLoader::new(&cfg.schedule);
			let mut ctx = ExecutionContext::top_level(origin.clone(), &cfg, &vm, &loader);
			let result = ctx.instantiate(endowment, &mut gas_meter, &code_hash, &data);

			if let Ok(_) = result {
				// Commit all changes that made it thus far into the persistant storage.
				DirectAccountDb.commit(ctx.overlay.into_change_set());

				// Then deposit all events produced.
				ctx.events.into_iter().for_each(Self::deposit_event);
			}

			// Refund cost of the unused gas.
			//
			// NOTE: this should go after the commit to the storage, since the storage changes
			// can alter the balance of the caller.
			gas::refund_unused_gas::<T>(&origin, gas_meter, imbalance);

			// Dispatch every recorded call with an appropriate origin.
			ctx.calls.into_iter().for_each(|(who, call)| {
				let result = call.dispatch(RawOrigin::Signed(who.clone()).into());
				Self::deposit_event(RawEvent::Dispatched(who, result.is_ok()));
			});

			result.map(|_| ())
		}

		fn on_finalise() {
			<GasSpent<T>>::kill();
		}
	}
}

decl_event! {
	pub enum Event<T>
	where
		<T as balances::Trait>::Balance,
		<T as system::Trait>::AccountId,
		<T as system::Trait>::Hash
	{
		/// Transfer happened `from` -> `to` with given `value` as part of a `call` or `create`.
		Transfer(AccountId, AccountId, Balance),

		/// Contract deployed by address at the specified address.
		Instantiated(AccountId, AccountId),

		/// Code with the specified hash has been stored.
		CodeStored(Hash),

		/// Triggered when the current schedule is updated.
		ScheduleUpdated(u32),

		/// A call was dispatched from the given account. The bool signals whether it was
		/// successful execution or not.
		Dispatched(AccountId, bool),
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as Contract {
		/// The fee required to create a contract instance. At least as big as staking's ReclaimRebate.
		ContractFee get(contract_fee) config(): T::Balance = T::Balance::sa(21);
		/// The base fee charged for a call into a contract.
		CallBaseFee get(call_base_fee) config(): T::Gas = T::Gas::sa(135);
		/// The base fee charged for creating a contract.
		CreateBaseFee get(create_base_fee) config(): T::Gas = T::Gas::sa(175);
		/// The price of one unit of gas.
		GasPrice get(gas_price) config(): T::Balance = T::Balance::sa(1);
		/// The maximum nesting level of a call/create stack.
		MaxDepth get(max_depth) config(): u32 = 100;
		/// The maximum amount of gas that could be expended per block.
		BlockGasLimit get(block_gas_limit) config(): T::Gas = T::Gas::sa(1_000_000);
		/// Gas spent so far in this block.
		GasSpent get(gas_spent): T::Gas;
		/// Current cost schedule for contracts.
		CurrentSchedule get(current_schedule) config(): Schedule<T::Gas> = Schedule::default();
		/// The code associated with a given account.
		pub CodeHashOf: map T::AccountId => Option<CodeHash<T>>;
		/// A mapping from an original code hash to the original code, untouched by instrumentation.
		pub PristineCode: map CodeHash<T> => Option<Vec<u8>>;
		/// A mapping between an original code hash and instrumented wasm code, ready for the execution.
		pub CodeStorage: map CodeHash<T> => Option<wasm::PrefabWasmModule>;
		/// The subtrie counter
		pub AccountCounter: u64 = 0;
		/// The code associated with a given account.
		pub AccountInfoOf: map T::AccountId => Option<AccountInfo>;
	}
}

impl<T: Trait> OnFreeBalanceZero<T::AccountId> for Module<T> {
	fn on_free_balance_zero(who: &T::AccountId) {
		<CodeHashOf<T>>::remove(who);
		<DirectAccountDb as AccountDb<T>>::get_account_info(&DirectAccountDb, who).map(|subtrie| {
			child::kill_storage(&subtrie.trie_id);
		});
	}
}

/// In-memory cache of configuration values.
///
/// We assume that these values can't be changed in the
/// course of transaction execution.
pub struct Config<T: Trait> {
	pub schedule: Schedule<T::Gas>,
	pub existential_deposit: T::Balance,
	pub max_depth: u32,
	pub contract_account_instantiate_fee: T::Balance,
	pub account_create_fee: T::Balance,
	pub transfer_fee: T::Balance,
	pub call_base_fee: T::Gas,
	pub instantiate_base_fee: T::Gas,
}

impl<T: Trait> Config<T> {
	fn preload() -> Config<T> {
		Config {
			schedule: <Module<T>>::current_schedule(),
			existential_deposit: <balances::Module<T>>::existential_deposit(),
			max_depth: <Module<T>>::max_depth(),
			contract_account_instantiate_fee: <Module<T>>::contract_fee(),
			account_create_fee: <balances::Module<T>>::creation_fee(),
			transfer_fee: <balances::Module<T>>::transfer_fee(),
			call_base_fee: <Module<T>>::call_base_fee(),
			instantiate_base_fee: <Module<T>>::create_base_fee(),
		}
	}
}

/// Definition of the cost schedule and other parameterizations for wasm vm.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize, Debug))]
#[derive(Clone, Encode, Decode, PartialEq, Eq)]
pub struct Schedule<Gas> {
	/// Version of the schedule.
	pub version: u32,

	/// Cost of putting a byte of code into the storage.
	pub put_code_per_byte_cost: Gas,

	/// Gas cost of a growing memory by single page.
	pub grow_mem_cost: Gas,

	/// Gas cost of a regular operation.
	pub regular_op_cost: Gas,

	/// Gas cost per one byte returned.
	pub return_data_per_byte_cost: Gas,

	/// Gas cost per one byte read from the sandbox memory.
	pub sandbox_data_read_cost: Gas,

	/// Gas cost per one byte written to the sandbox memory.
	pub sandbox_data_write_cost: Gas,

	/// How tall the stack is allowed to grow?
	///
	/// See https://wiki.parity.io/WebAssembly-StackHeight to find out
	/// how the stack frame cost is calculated.
	pub max_stack_height: u32,

	/// What is the maximal memory pages amount is allowed to have for
	/// a contract.
	pub max_memory_pages: u32,
}

impl<Gas: As<u64>> Default for Schedule<Gas> {
	fn default() -> Schedule<Gas> {
		Schedule {
			version: 0,
			put_code_per_byte_cost: Gas::sa(1),
			grow_mem_cost: Gas::sa(1),
			regular_op_cost: Gas::sa(1),
			return_data_per_byte_cost: Gas::sa(1),
			sandbox_data_read_cost: Gas::sa(1),
			sandbox_data_write_cost: Gas::sa(1),
			max_stack_height: 64 * 1024,
			max_memory_pages: 16,
		}
	}
}
