// This file is part of Substrate.

// Copyright (C) 2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Pallet State Trie Migration
//!
//! Reads and writes all keys and values in the entire state in a systematic way. This is useful for
//! upgrading a chain to [`sp-core::StateVersion::V1`], where all keys need to be touched.
//!
//! ## Migration Types
//!
//! This pallet provides 3 ways to do this, each of which is suited for a particular use-case, and
//! can be enabled independently.
//!
//! ### Auto migration
//!
//! This system will try and migrate all keys by continuously using `on_initialize`. It is only
//! sensible for a relay chain or a solo chain, where going slightly over weight is not a problem.
//! It can be configured so that the migration takes at most `n` items and tries to not go over `x`
//! bytes, but the latter is not guaranteed.
//!
//! For example, if a chain contains keys of 1 byte size, the `on_initialize` could read up to `x -
//! 1` bytes from `n` different keys, while the next key is suddenly `:code:`, and there is no way
//! to bail out of this.
//!
//! ### Signed migration
//!
//! as a backup, the migration process can be set in motion via signed transactions that basically
//! say in advance how many items and how many bytes they will consume, and pay for it as well. This
//! can be a good safe alternative, if the former two systems are not desirable.
//!
//! The (minor) caveat of this approach is that we cannot know in advance how many bytes reading a
//! certain number of keys will incur. To overcome this, the runtime needs to configure this pallet
//! with a `SignedDepositPerItem`. This is the per-item deposit that the origin of the signed
//! migration transactions need to have in their account (on top of the normal fee) and if the size
//! witness data that they claim is incorrect, this deposit is slashed.
//!
//! ---
//!
//! Initially, this pallet does not contain any auto migration. They must be manually enabled by the
//! `ControlOrigin`.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

const LOG_TARGET: &'static str = "runtime::state-trie-migration";

#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: crate::LOG_TARGET,
			concat!("[{:?}] 🤖 ", $patter), frame_system::Pallet::<T>::block_number() $(, $values)*
		)
	};
}

#[frame_support::pallet]
pub mod pallet {
	use frame_support::{
		dispatch::{DispatchErrorWithPostInfo, PostDispatchInfo},
		ensure,
		pallet_prelude::*,
		traits::{Currency, Get},
	};
	use frame_system::{self, ensure_signed, pallet_prelude::*};
	use sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX;
	use sp_runtime::{
		self,
		traits::{Saturating, Zero},
	};
	use sp_std::prelude::*;

	pub(crate) type BalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

	/// The weight information of this pallet.
	pub trait WeightInfo {
		fn process_top_key(x: u32) -> Weight;
		fn continue_migrate() -> Weight;
		fn continue_migrate_wrong_witness() -> Weight;
		fn migrate_custom_top_fail() -> Weight;
		fn migrate_custom_top_success() -> Weight;
	}

	impl WeightInfo for () {
		fn process_top_key(_: u32) -> Weight {
			1000000
		}
		fn continue_migrate() -> Weight {
			1000000
		}
		fn continue_migrate_wrong_witness() -> Weight {
			1000000
		}
		fn migrate_custom_top_fail() -> Weight {
			1000000
		}
		fn migrate_custom_top_success() -> Weight {
			1000000
		}
	}

	/// A migration task stored in state.
	///
	/// It tracks the last top and child keys read.
	#[derive(Clone, Encode, Decode, scale_info::TypeInfo, PartialEq, Eq)]
	#[codec(mel_bound(T: Config))]
	#[scale_info(skip_type_params(T))]
	pub struct MigrationTask<T: Config> {
		/// The top key that we currently have to iterate.
		///
		/// If it does not exist, it means that the migration is done and no further keys exist.
		pub(crate) current_top: Option<Vec<u8>>,
		/// The last child key that we have processed.
		///
		/// This is a child key under the current `self.last_top`.
		///
		/// If this is set, no further top keys are processed until the child key migration is
		/// complete.
		pub(crate) current_child: Option<Vec<u8>>,

		/// A marker to indicate if the previous tick was a child tree migration or not.
		pub(crate) prev_tick_child: bool,

		/// dynamic counter for the number of items that we have processed in this execution from
		/// the top trie.
		///
		/// It is not written to storage.
		#[codec(skip)]
		pub(crate) dyn_top_items: u32,
		/// dynamic counter for the number of items that we have processed in this execution from
		/// any child trie.
		///
		/// It is not written to storage.
		#[codec(skip)]
		pub(crate) dyn_child_items: u32,

		/// dynamic counter for for the byte size of items that we have processed in this
		/// execution.
		///
		/// It is not written to storage.
		#[codec(skip)]
		pub(crate) dyn_size: u32,

		/// The total size of the migration, over all executions.
		///
		/// This only kept around for bookkeeping and debugging.
		pub(crate) size: u32,
		/// The total count of top keys in the migration, over all executions.
		///
		/// This only kept around for bookkeeping and debugging.
		pub(crate) top_items: u32,
		/// The total count of child keys in the migration, over all executions.
		///
		/// This only kept around for bookkeeping and debugging.
		pub(crate) child_items: u32,

		#[codec(skip)]
		pub(crate) _ph: sp_std::marker::PhantomData<T>,
	}

	impl<T: Config> sp_std::fmt::Debug for MigrationTask<T> {
		fn fmt(&self, f: &mut sp_std::fmt::Formatter<'_>) -> sp_std::fmt::Result {
			f.debug_struct("MigrationTask")
				.field(
					"top",
					&self.current_top.as_ref().map(|d| sp_core::hexdisplay::HexDisplay::from(d)),
				)
				.field(
					"child",
					&self.current_child.as_ref().map(|d| sp_core::hexdisplay::HexDisplay::from(d)),
				)
				.field("prev_tick_child", &self.prev_tick_child)
				.field("dyn_top_items", &self.dyn_top_items)
				.field("dyn_child_items", &self.dyn_child_items)
				.field("dyn_size", &self.dyn_size)
				.field("size", &self.size)
				.field("top_items", &self.top_items)
				.field("child_items", &self.child_items)
				.finish()
		}
	}

	impl<T: Config> Default for MigrationTask<T> {
		fn default() -> Self {
			Self {
				current_top: Some(Default::default()),
				current_child: Default::default(),
				dyn_child_items: Default::default(),
				dyn_top_items: Default::default(),
				dyn_size: Default::default(),
				prev_tick_child: Default::default(),
				_ph: Default::default(),
				size: Default::default(),
				top_items: Default::default(),
				child_items: Default::default(),
			}
		}
	}

	impl<T: Config> MigrationTask<T> {
		/// Return true if the task is finished.
		#[cfg(test)]
		pub(crate) fn finished(&self) -> bool {
			self.current_top.is_none() && self.current_child.is_none()
		}

		/// Check if there's any work left, or if we have exhausted the limits already.
		fn exhausted(&self, limits: MigrationLimits) -> bool {
			self.current_top.is_none() ||
				self.dyn_total_items() >= limits.item ||
				self.dyn_size >= limits.size
		}

		/// get the total number of keys affected by the current task.
		pub(crate) fn dyn_total_items(&self) -> u32 {
			self.dyn_child_items.saturating_add(self.dyn_top_items)
		}

		/// Migrate keys until either of the given limits are exhausted, or if no more top keys
		/// exist.
		///
		/// Note that this can return after the **first** migration tick that causes exhaustion,
		/// specifically in the case of the `size` constrain. The reason for this is that before
		/// reading a key, we simply cannot know how many bytes it is. In other words, this should
		/// not be used in any environment where resources are strictly bounded (e.g. a parachain),
		/// but it is acceptable otherwise (relay chain, offchain workers).
		pub(crate) fn migrate_until_exhaustion(&mut self, limits: MigrationLimits) {
			log!(debug, "running migrations on top of {:?} until {:?}", self, limits);

			if limits.item.is_zero() || limits.size.is_zero() {
				// handle this minor edge case, else we would call `migrate_tick` at least once.
				log!(warn, "limits are zero. stopping");
				return
			}

			loop {
				self.migrate_tick();
				if self.exhausted(limits) {
					break
				}
			}

			// accumulate dynamic data into the storage items.
			self.size = self.size.saturating_add(self.dyn_size);
			self.child_items = self.child_items.saturating_add(self.dyn_child_items);
			self.top_items = self.top_items.saturating_add(self.dyn_top_items);
			log!(debug, "finished with {:?}", self);
		}

		/// Migrate AT MOST ONE KEY. This can be either a top or a child key.
		///
		/// This function is the core of this entire pallet.
		fn migrate_tick(&mut self) {
			match (self.current_top.as_ref(), self.current_child.as_ref()) {
				(Some(_), Some(_)) => {
					// we're in the middle of doing work on a child tree.
					self.migrate_child();
				},
				(Some(ref top_key), None) => {
					// we have a top key and no child key. 3 possibilities exist:
					// 1. we continue the top key migrations.
					// 2. this is the root of a child key, and we start processing child keys (and
					// should call `migrate_child`).
					// 3. this is the root of a child key, and we are finishing all child-keys (and
					// should call `migrate_top`).

					// NOTE: this block is written intentionally to verbosely for easy of
					// verification.
					match (
						top_key.starts_with(DEFAULT_CHILD_STORAGE_KEY_PREFIX),
						self.prev_tick_child,
					) {
						(false, false) => {
							// continue the top key migration
							self.migrate_top();
						},
						(true, false) => {
							// start going into a child key. In the first iteration, we always
							let maybe_first_child_key = {
								// just in case there's some data in `&[]`, read it. Since we can't
								// check this without reading the actual key, and given that this
								// function should always read at most one key, we return after
								// this. The rest of the migration should happen in the next tick.
								let child_top_key = Pallet::<T>::child_io_key_or_halt(top_key);
								let _ = sp_io::default_child_storage::get(child_top_key, &vec![]);
								sp_io::default_child_storage::next_key(child_top_key, &vec![])
							};
							if let Some(first_child_key) = maybe_first_child_key {
								self.current_child = Some(first_child_key);
								self.prev_tick_child = true;
							} else {
								// we have already done a (pretty useless) child key migration, just
								// set the flag. Since we don't set the `self.current_child`, next
								// tick will move forward to the next top key.
								self.prev_tick_child = true;
							}
						},
						(true, true) => {
							// we're done with migrating a child-root.
							self.prev_tick_child = false;
							self.migrate_top();
						},
						(false, true) => {
							// should never happen.
							log!(error, "LOGIC ERROR: unreachable code [0].");
							Pallet::<T>::halt();
						},
					};
				},
				(None, Some(_)) => {
					log!(error, "LOGIC ERROR: unreachable code [1].");
					Pallet::<T>::halt()
				},
				(None, None) => {
					// nada
				},
			}
		}

		/// Migrate the current child key, setting it to its new value, if one exists.
		///
		/// It updates the dynamic counters.
		fn migrate_child(&mut self) {
			let last_child = self.current_child.as_ref().expect("value checked to be `Some`; qed");
			let last_top = self.current_top.clone().expect("value checked to be `Some`; qed");

			let child_root = Pallet::<T>::child_io_key_or_halt(&last_top);
			let added_size =
				if let Some(data) = sp_io::default_child_storage::get(child_root, &last_child) {
					self.dyn_size = self.dyn_size.saturating_add(data.len() as u32);
					sp_io::default_child_storage::set(child_root, last_child, &data);
					data.len() as u32
				} else {
					Zero::zero()
				};

			self.dyn_child_items.saturating_inc();
			let next_key = sp_io::default_child_storage::next_key(child_root, last_child);
			self.current_child = next_key;
			log!(trace, "migrated a child key with size: {:?}, next task: {:?}", added_size, self,);
		}

		/// Migrate the current top key, setting it to its new value, if one exists.
		///
		/// It updates the dynamic counters.
		fn migrate_top(&mut self) {
			let last_top = self.current_top.as_ref().expect("value checked to be `Some`; qed");
			let added_size = if let Some(data) = sp_io::storage::get(&last_top) {
				self.dyn_size = self.dyn_size.saturating_add(data.len() as u32);
				sp_io::storage::set(last_top, &data);
				data.len() as u32
			} else {
				Zero::zero()
			};

			self.dyn_top_items.saturating_inc();
			let next_key = sp_io::storage::next_key(last_top);
			self.current_top = next_key;
			log!(trace, "migrated a top key with size {}, next_task = {:?}", added_size, self);
		}
	}

	/// The limits of a migration.
	#[derive(Clone, Copy, Encode, Decode, scale_info::TypeInfo, Default, Debug, PartialEq, Eq)]
	pub struct MigrationLimits {
		/// The byte size limit.
		pub size: u32,
		/// The number of keys limit.
		pub item: u32,
	}

	/// How a migration was computed.
	#[derive(Clone, Copy, Encode, Decode, scale_info::TypeInfo, Debug, PartialEq, Eq)]
	pub enum MigrationCompute {
		/// A signed origin triggered the migration.
		Signed,
		/// An automatic task triggered the migration.
		Auto,
	}

	/// Inner events of this pallet.
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Given number of `(top, child)` keys were migrated respectively, with the given
		/// `compute`.
		Migrated { top: u32, child: u32, compute: MigrationCompute },
		/// Some account got slashed by the given amount.
		Slashed { who: T::AccountId, amount: BalanceOf<T> },
	}

	/// The outer Pallet struct.
	#[pallet::pallet]
	#[pallet::generate_store(pub(crate) trait Store)]
	pub struct Pallet<T>(_);

	/// Configurations of this pallet.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Origin that can control the configurations of this pallet.
		type ControlOrigin: frame_support::traits::EnsureOrigin<Self::Origin>;

		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The currency provider type.
		type Currency: Currency<Self::AccountId>;

		/// The amount of deposit collected per item in advance, for signed migrations.
		///
		/// This should reflect the average storage value size in the worse case.
		type SignedDepositPerItem: Get<BalanceOf<Self>>;

		/// The base value of [`SignedDepositPerItem`].
		///
		/// Final deposit is `items * SignedDepositPerItem + SignedDepositBase`.
		type SignedDepositBase: Get<BalanceOf<Self>>;

		/// The maximum limits that the signed migration could use.
		type SignedMigrationMaxLimits: Get<MigrationLimits>;

		/// The weight information of this pallet.
		type WeightInfo: WeightInfo;
	}

	/// Migration progress.
	///
	/// This stores the snapshot of the last migrated keys. It can be set into motion and move
	/// forward by any of the means provided by this pallet.
	#[pallet::storage]
	#[pallet::getter(fn migration_process)]
	pub type MigrationProcess<T> = StorageValue<_, MigrationTask<T>, ValueQuery>;

	/// The limits that are imposed on automatic migrations.
	///
	/// If set to None, then no automatic migration happens.
	#[pallet::storage]
	#[pallet::getter(fn auto_limits)]
	pub type AutoLimits<T> = StorageValue<_, Option<MigrationLimits>, ValueQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// control the automatic migration.
		///
		/// The dispatch origin of this call must be [`Config::ControlOrigin`].
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn control_auto_migration(
			origin: OriginFor<T>,
			maybe_config: Option<MigrationLimits>,
		) -> DispatchResultWithPostInfo {
			T::ControlOrigin::ensure_origin(origin)?;
			AutoLimits::<T>::put(maybe_config);
			Ok(().into())
		}

		/// Continue the migration for the given `limits`.
		///
		/// The dispatch origin of this call can be any signed account.
		///
		/// This transaction has NO MONETARY INCENTIVES. calling it will not reward anyone. Albeit,
		/// Upon successful execution, the transaction fee is returned.
		///
		/// The (potentially over-estimated) of the byte length of all the data read must be
		/// provided for up-front fee-payment and weighing. In essence, the caller is guaranteeing
		/// that executing the current `MigrationTask` with the given `limits` will not exceed
		/// `real_size_upper` bytes of read data.
		///
		/// The `witness_task` is merely a helper to prevent the caller from being slashed or
		/// generally trigger a migration that they do not intend. This parameter is just a message
		/// from caller, saying that they believed `witness_task` was the last state of the
		/// migration, and they only wish for their transaction to do anything, if this assumption
		/// holds. In case `witness_task` does not match, the transaction fails.
		///
		/// Based on the documentation of [`MigrationTask::migrate_until_exhaustion`], the
		/// recommended way of doing this is to pass a `limit` that only bounds `count`, as the
		/// `size` limit can always be overwritten.
		#[pallet::weight(
			// the migration process
			Pallet::<T>::dynamic_weight(limits.item, * real_size_upper)
			// rest of the operations, like deposit etc.
			+ T::WeightInfo::continue_migrate()
		)]
		pub fn continue_migrate(
			origin: OriginFor<T>,
			limits: MigrationLimits,
			real_size_upper: u32,
			witness_task: MigrationTask<T>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			let max_limits = T::SignedMigrationMaxLimits::get();
			ensure!(
				limits.size <= max_limits.size && limits.item <= max_limits.item,
				"max signed limits not respected"
			);

			// ensure they can pay more than the fee.
			let deposit = T::SignedDepositPerItem::get().saturating_mul(limits.item.into());
			ensure!(T::Currency::can_slash(&who, deposit), "not enough funds");

			let mut task = Self::migration_process();
			ensure!(
				task == witness_task,
				DispatchErrorWithPostInfo {
					error: "wrong witness".into(),
					post_info: PostDispatchInfo {
						actual_weight: Some(T::WeightInfo::continue_migrate_wrong_witness()),
						pays_fee: Pays::Yes
					}
				}
			);
			task.migrate_until_exhaustion(limits);

			// ensure that the migration witness data was correct.
			if real_size_upper < task.dyn_size {
				// let the imbalance burn.
				let (_imbalance, _remainder) = T::Currency::slash(&who, deposit);
				debug_assert!(_remainder.is_zero());
				return Err("wrong witness data".into())
			}

			Self::deposit_event(Event::<T>::Migrated {
				top: task.dyn_top_items,
				child: task.dyn_child_items,
				compute: MigrationCompute::Signed,
			});

			let actual_weight = Some(
				Pallet::<T>::dynamic_weight(limits.item, task.dyn_size) +
					T::WeightInfo::continue_migrate(),
			);
			MigrationProcess::<T>::put(task);
			let pays = Pays::No;

			Ok((actual_weight, pays).into())
		}

		/// Migrate the list of top keys by iterating each of them one by one.
		///
		/// This does not affect the global migration process tracker ([`MigrationProcess`]), and
		/// should only be used in case any keys are leftover due to a bug.
		#[pallet::weight(
			T::WeightInfo::migrate_custom_top_success()
				.max(T::WeightInfo::migrate_custom_top_fail())
			.saturating_add(
				Pallet::<T>::dynamic_weight(keys.len() as u32, *witness_size)
			)
		)]
		pub fn migrate_custom_top(
			origin: OriginFor<T>,
			keys: Vec<Vec<u8>>,
			witness_size: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			// ensure they can pay more than the fee.
			let deposit = T::SignedDepositBase::get().saturating_add(
				T::SignedDepositPerItem::get().saturating_mul((keys.len() as u32).into()),
			);
			ensure!(T::Currency::can_slash(&who, deposit), "not enough funds");

			let mut dyn_size = 0u32;
			for key in &keys {
				if let Some(data) = sp_io::storage::get(&key) {
					dyn_size = dyn_size.saturating_add(data.len() as u32);
					sp_io::storage::set(key, &data);
				}
			}

			if dyn_size > witness_size {
				let (_imbalance, _remainder) = T::Currency::slash(&who, deposit);
				debug_assert!(_remainder.is_zero());
				return Err("wrong witness data".into())
			}

			Self::deposit_event(Event::<T>::Migrated {
				top: keys.len() as u32,
				child: 0,
				compute: MigrationCompute::Signed,
			});
			Ok(().into())
		}

		/// Migrate the list of child keys by iterating each of them one by one.
		///
		/// All of the given child keys must be present under one `top_key`.
		///
		/// This does not affect the global migration process tracker ([`MigrationProcess`]), and
		/// should only be used in case any keys are leftover due to a bug.
		#[pallet::weight(
			T::WeightInfo::migrate_custom_top_success()
				.max(T::WeightInfo::migrate_custom_top_fail())
			.saturating_add(
				Pallet::<T>::dynamic_weight(child_keys.len() as u32, *total_size)
			)
		)]
		pub fn migrate_custom_child(
			origin: OriginFor<T>,
			top_key: Vec<u8>,
			child_keys: Vec<Vec<u8>>,
			total_size: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			// ensure they can pay more than the fee.
			let deposit = T::SignedDepositBase::get().saturating_add(
				T::SignedDepositPerItem::get().saturating_mul((child_keys.len() as u32).into()),
			);
			ensure!(T::Currency::can_slash(&who, deposit), "not enough funds");

			let mut dyn_size = 0u32;
			for child_key in &child_keys {
				if let Some(data) = sp_io::default_child_storage::get(
					Self::child_io_key(&top_key).ok_or("bad child key")?,
					&child_key,
				) {
					dyn_size = dyn_size.saturating_add(data.len() as u32);
					sp_io::default_child_storage::set(
						Self::child_io_key(&top_key).ok_or("bad child key")?,
						&child_key,
						&data,
					);
				}
			}

			if dyn_size != total_size {
				let (_imbalance, _remainder) = T::Currency::slash(&who, deposit);
				debug_assert!(_remainder.is_zero());
				Self::deposit_event(Event::<T>::Slashed { who, amount: deposit });
				Err(DispatchErrorWithPostInfo {
					error: "bad witness".into(),
					post_info: PostDispatchInfo {
						actual_weight: Some(T::WeightInfo::migrate_custom_top_fail()),
						pays_fee: Pays::Yes,
					},
				})
			} else {
				Self::deposit_event(Event::<T>::Migrated {
					top: 0,
					child: child_keys.len() as u32,
					compute: MigrationCompute::Signed,
				});
				Ok(PostDispatchInfo {
					actual_weight: Some(T::WeightInfo::migrate_custom_top_success()),
					pays_fee: Pays::Yes,
				})
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_: BlockNumberFor<T>) -> Weight {
			if let Some(limits) = Self::auto_limits() {
				let mut task = Self::migration_process();
				task.migrate_until_exhaustion(limits);
				let weight = Self::dynamic_weight(task.dyn_total_items(), task.dyn_size);

				log!(
					info,
					"migrated {} top keys, {} child keys, and a total of {} bytes.",
					task.dyn_top_items,
					task.dyn_child_items,
					task.dyn_size,
				);
				Self::deposit_event(Event::<T>::Migrated {
					top: task.dyn_top_items,
					child: task.dyn_child_items,
					compute: MigrationCompute::Auto,
				});
				MigrationProcess::<T>::put(task);

				weight
			} else {
				T::DbWeight::get().reads(1)
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// The real weight of a migration of the given number of `items` with total `size`.
		fn dynamic_weight(items: u32, size: u32) -> frame_support::pallet_prelude::Weight {
			let items = items as Weight;
			items
				.saturating_mul(<T as frame_system::Config>::DbWeight::get().reads_writes(1, 1))
				// we assume that the read/write per-byte weight is the same for child and top tree.
				.saturating_add(T::WeightInfo::process_top_key(size))
		}

		/// Put a stop to all ongoing migrations.
		fn halt() {
			AutoLimits::<T>::kill();
		}

		/// Convert a child root key, aka. "Child-bearing top key" into the proper format.
		fn child_io_key(root: &Vec<u8>) -> Option<&[u8]> {
			use sp_core::storage::{ChildType, PrefixedStorageKey};
			match ChildType::from_prefixed_key(PrefixedStorageKey::new_ref(root)) {
				Some((ChildType::ParentKeyId, root)) => Some(root),
				_ => None,
			}
		}

		/// Same as [`child_io_key`], and it halts the auto/unsigned migrations if a bad child root
		/// is used.
		///
		/// This should be used when we are sure that `root` is a correct default child root.
		fn child_io_key_or_halt(root: &Vec<u8>) -> &[u8] {
			let key = Self::child_io_key(root);
			if key.is_none() {
				Self::halt();
			}
			key.unwrap_or_default()
		}
	}
}

#[cfg(feature = "runtime-benchmarks")]
mod benchmarks {
	use super::{pallet::Pallet as StateTrieMigration, *};
	use frame_support::traits::Currency;

	// The size of the key seemingly makes no difference in the read/write time, so we make it
	// constant.
	const KEY: &'static [u8] = b"key";

	frame_benchmarking::benchmarks! {
		continue_migrate {
			// note that this benchmark should migrate nothing, as we only want the overhead weight
			// of the bookkeeping, and the migration cost itself is noted via the `dynamic_weight`
			// function.
			let null = MigrationLimits::default();
			let caller = frame_benchmarking::whitelisted_caller();
		}: _(frame_system::RawOrigin::Signed(caller), null, 0, StateTrieMigration::<T>::migration_process())
		verify {
			assert_eq!(StateTrieMigration::<T>::migration_process(), Default::default())
		}

		continue_migrate_wrong_witness {
			let null = MigrationLimits::default();
			let caller = frame_benchmarking::whitelisted_caller();
			let bad_witness = MigrationTask { current_top: Some(vec![1u8]), ..Default::default() };
		}: {
			assert!(
				StateTrieMigration::<T>::continue_migrate(
					frame_system::RawOrigin::Signed(caller).into(),
					null,
					0,
					bad_witness,
				)
				.is_err()
			)
		}
		verify {
			assert_eq!(StateTrieMigration::<T>::migration_process(), Default::default())
		}

		migrate_custom_top_success {
			let null = MigrationLimits::default();
			let caller = frame_benchmarking::whitelisted_caller();
			let stash = T::Currency::minimum_balance() * BalanceOf::<T>::from(10u32);
			T::Currency::make_free_balance_be(&caller, stash);
		}: migrate_custom_top(frame_system::RawOrigin::Signed(caller.clone()), Default::default(), 0)
		verify {
			assert_eq!(StateTrieMigration::<T>::migration_process(), Default::default());
			assert_eq!(T::Currency::free_balance(&caller), stash)
		}

		migrate_custom_top_fail {
			let null = MigrationLimits::default();
			let caller = frame_benchmarking::whitelisted_caller();
			let stash = T::Currency::minimum_balance() * BalanceOf::<T>::from(10u32);
			T::Currency::make_free_balance_be(&caller, stash);
		}: {
			assert!(
				dbg!(StateTrieMigration::<T>::migrate_custom_top(
					frame_system::RawOrigin::Signed(caller.clone()).into(),
					Default::default(),
					1,
				)).is_err()
			)
		}
		verify {
			assert_eq!(StateTrieMigration::<T>::migration_process(), Default::default());
			// must have gotten slashed
			assert!(T::Currency::free_balance(&caller) < stash)
		}

		process_top_key {
			let v in 1 .. (4 * 1024 * 1024);

			let value = sp_std::vec![1u8; v as usize];
			sp_io::storage::set(KEY, &value);
		}: {
			let data = sp_io::storage::get(KEY).unwrap();
			sp_io::storage::set(KEY, &data);
			let _next = sp_io::storage::next_key(KEY);
			assert_eq!(data, value);
		}

		impl_benchmark_test_suite!(
			StateTrieMigration,
			crate::mock::new_test_ext(sp_runtime::StateVersion::V0, true),
			crate::mock::Test
		);
	}
}

#[cfg(test)]
mod mock {
	use super::*;
	use crate as pallet_state_trie_migration;
	use frame_support::{parameter_types, traits::Hooks};
	use frame_system::EnsureRoot;
	use sp_core::{storage::StateVersion, H256};
	use sp_runtime::traits::{BlakeTwo256, Header as _, IdentityLookup};

	type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
	type Block = frame_system::mocking::MockBlock<Test>;

	// Configure a mock runtime to test the pallet.
	frame_support::construct_runtime!(
		pub enum Test where
			Block = Block,
			NodeBlock = Block,
			UncheckedExtrinsic = UncheckedExtrinsic,
		{
			System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
			Balances: pallet_balances::{Pallet, Call, Config<T>, Storage, Event<T>},
			StateTrieMigration: pallet_state_trie_migration::{Pallet, Call, Storage, Event<T>},
		}
	);

	parameter_types! {
		pub const BlockHashCount: u32 = 250;
		pub const SS58Prefix: u8 = 42;
	}

	impl frame_system::Config for Test {
		type BaseCallFilter = frame_support::traits::Everything;
		type BlockWeights = ();
		type BlockLength = ();
		type Origin = Origin;
		type Call = Call;
		type Index = u64;
		type BlockNumber = u32;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = sp_runtime::generic::Header<Self::BlockNumber, BlakeTwo256>;
		type Event = Event;
		type BlockHashCount = BlockHashCount;
		type DbWeight = ();
		type Version = ();
		type PalletInfo = PalletInfo;
		type AccountData = pallet_balances::AccountData<u64>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = SS58Prefix;
		type OnSetCode = ();
		type MaxConsumers = frame_support::traits::ConstU32<16>;
	}

	parameter_types! {
		pub const ExistentialDeposit: u64 = 1;
		pub const OffchainRepeat: u32 = 1;
		pub const SignedDepositPerItem: u64 = 1;
		pub const SignedDepositBase: u64 = 5;
		pub const SignedMigrationMaxLimits: MigrationLimits = MigrationLimits { size: 1024, item: 5 };
	}

	impl pallet_balances::Config for Test {
		type Balance = u64;
		type Event = Event;
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = ();
		type MaxReserves = ();
		type ReserveIdentifier = [u8; 8];
		type WeightInfo = ();
	}

	impl pallet_state_trie_migration::Config for Test {
		type Event = Event;
		type ControlOrigin = EnsureRoot<u64>;
		type Currency = Balances;
		type SignedDepositPerItem = SignedDepositPerItem;
		type SignedDepositBase = SignedDepositBase;
		type SignedMigrationMaxLimits = SignedMigrationMaxLimits;
		type WeightInfo = ();
	}

	pub fn new_test_ext(version: StateVersion, with_pallets: bool) -> sp_io::TestExternalities {
		use sp_core::storage::ChildInfo;

		let minimum_size = sp_core::storage::TRIE_VALUE_NODE_THRESHOLD as usize + 1;
		let mut custom_storage = sp_core::storage::Storage {
			top: vec![
				(b"key1".to_vec(), vec![1u8; minimum_size + 1]), // 6b657931
				(b"key2".to_vec(), vec![1u8; minimum_size + 2]), // 6b657931
				(b"key3".to_vec(), vec![1u8; minimum_size + 3]), // 6b657931
				(b"key4".to_vec(), vec![1u8; minimum_size + 4]), // 6b657931
				(b"key5".to_vec(), vec![1u8; minimum_size + 5]), // 6b657932
				(b"key6".to_vec(), vec![1u8; minimum_size + 6]), // 6b657934
				(b"key7".to_vec(), vec![1u8; minimum_size + 7]), // 6b657934
				(b"key8".to_vec(), vec![1u8; minimum_size + 8]), // 6b657934
				(b"key9".to_vec(), vec![1u8; minimum_size + 9]), // 6b657934
				(b"CODE".to_vec(), vec![1u8; minimum_size + 100]), // 434f4445
			]
			.into_iter()
			.collect(),
			children_default: vec![
				(
					b"chk1".to_vec(), // 63686b31
					sp_core::storage::StorageChild {
						data: vec![
							(b"key1".to_vec(), vec![1u8; 55]),
							(b"key2".to_vec(), vec![2u8; 66]),
						]
						.into_iter()
						.collect(),
						child_info: ChildInfo::new_default(b"chk1"),
					},
				),
				(
					b"chk2".to_vec(),
					sp_core::storage::StorageChild {
						data: vec![
							(b"key1".to_vec(), vec![1u8; 54]),
							(b"key2".to_vec(), vec![2u8; 64]),
						]
						.into_iter()
						.collect(),
						child_info: ChildInfo::new_default(b"chk2"),
					},
				),
			]
			.into_iter()
			.collect(),
		};

		if with_pallets {
			frame_system::GenesisConfig::default()
				.assimilate_storage::<Test>(&mut custom_storage)
				.unwrap();
			pallet_balances::GenesisConfig::<Test> { balances: vec![(1, 1000)] }
				.assimilate_storage(&mut custom_storage)
				.unwrap();
		}

		sp_tracing::try_init_simple();
		(custom_storage, version).into()
	}

	pub fn run_to_block(n: u32) -> H256 {
		let mut root = Default::default();
		log::trace!(target: LOG_TARGET, "running from {:?} to {:?}", System::block_number(), n);
		while System::block_number() < n {
			System::set_block_number(System::block_number() + 1);
			System::on_initialize(System::block_number());

			StateTrieMigration::on_initialize(System::block_number());

			root = System::finalize().state_root().clone();
			System::on_finalize(System::block_number());
		}
		root
	}
}

#[cfg(test)]
mod test {
	use super::{mock::*, *};
	use sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX;
	use sp_runtime::{traits::Bounded, StateVersion};

	#[test]
	fn fails_if_no_migration() {
		let mut ext = new_test_ext(StateVersion::V0, false);
		let root1 = ext.execute_with(|| run_to_block(30));

		let mut ext2 = new_test_ext(StateVersion::V1, false);
		let root2 = ext2.execute_with(|| run_to_block(30));

		// these two roots should not be the same.
		assert_ne!(root1, root2);
	}

	#[test]
	fn detects_first_child_key() {
		use frame_support::storage::child;
		let limit = MigrationLimits { item: 1, size: 1000 };
		let mut ext = new_test_ext(StateVersion::V0, false);

		let root_upgraded = ext.execute_with(|| {
			child::put(&child::ChildInfo::new_default(b"chk1"), &[], &vec![66u8; 77]);

			AutoLimits::<Test>::put(Some(limit));
			let root = run_to_block(30);

			// eventually everything is over.
			assert!(matches!(
				StateTrieMigration::migration_process(),
				MigrationTask { current_child: None, current_top: None, .. }
			));
			root
		});

		let mut ext2 = new_test_ext(StateVersion::V1, false);
		let root = ext2.execute_with(|| {
			child::put(&child::ChildInfo::new_default(b"chk1"), &[], &vec![66u8; 77]);
			AutoLimits::<Test>::put(Some(limit));
			run_to_block(30)
		});

		assert_eq!(root, root_upgraded);
	}

	#[test]
	fn auto_migrate_works() {
		let run_with_limits = |limit, from, until| {
			let mut ext = new_test_ext(StateVersion::V0, false);
			let root_upgraded = ext.execute_with(|| {
				assert_eq!(AutoLimits::<Test>::get(), None);
				assert_eq!(MigrationProcess::<Test>::get(), Default::default());

				// nothing happens if we don't set the limits.
				let _ = run_to_block(from);
				assert_eq!(MigrationProcess::<Test>::get(), Default::default());

				// this should allow 1 item per block to be migrated.
				AutoLimits::<Test>::put(Some(limit));

				let root = run_to_block(until);

				// eventually everything is over.
				assert!(matches!(
					StateTrieMigration::migration_process(),
					MigrationTask { current_child: None, current_top: None, .. }
				));
				root
			});

			let mut ext2 = new_test_ext(StateVersion::V1, false);
			let root = ext2.execute_with(|| {
				// update ex2 to contain the new items
				let _ = run_to_block(from);
				AutoLimits::<Test>::put(Some(limit));
				run_to_block(until)
			});
			assert_eq!(root, root_upgraded);
		};

		// single item
		run_with_limits(MigrationLimits { item: 1, size: 1000 }, 10, 100);
		// multi-item
		run_with_limits(MigrationLimits { item: 5, size: 1000 }, 10, 100);
		// multi-item, based on size. Note that largest value is 100 bytes.
		run_with_limits(MigrationLimits { item: 1000, size: 128 }, 10, 100);
		// unbounded
		run_with_limits(
			MigrationLimits { item: Bounded::max_value(), size: Bounded::max_value() },
			10,
			100,
		);
	}

	#[test]
	fn signed_migrate_works() {
		new_test_ext(StateVersion::V0, true).execute_with(|| {
			assert_eq!(MigrationProcess::<Test>::get(), Default::default());

			// can't submit if limit is too high.
			frame_support::assert_err!(
				StateTrieMigration::continue_migrate(
					Origin::signed(1),
					MigrationLimits { item: 5, size: sp_runtime::traits::Bounded::max_value() },
					Bounded::max_value(),
					MigrationProcess::<Test>::get()
				),
				"max signed limits not respected"
			);

			// can't submit if poor.
			frame_support::assert_err!(
				StateTrieMigration::continue_migrate(
					Origin::signed(2),
					MigrationLimits { item: 5, size: 100 },
					100,
					MigrationProcess::<Test>::get()
				),
				"not enough funds"
			);

			// can't submit with bad witness.
			frame_support::assert_err_ignore_postinfo!(
				StateTrieMigration::continue_migrate(
					Origin::signed(1),
					MigrationLimits { item: 5, size: 100 },
					100,
					MigrationTask { current_top: Some(vec![1u8]), ..Default::default() }
				),
				"wrong witness"
			);

			// migrate all keys in a series of submissions
			while !MigrationProcess::<Test>::get().finished() {
				// first we compute the task to get the accurate consumption.
				let mut task = StateTrieMigration::migration_process();
				task.migrate_until_exhaustion(SignedMigrationMaxLimits::get());

				frame_support::assert_ok!(StateTrieMigration::continue_migrate(
					Origin::signed(1),
					SignedMigrationMaxLimits::get(),
					task.dyn_size,
					MigrationProcess::<Test>::get()
				));

				// no funds should remain reserved.
				assert_eq!(Balances::reserved_balance(&1), 0);

				// and the task should be updated
				assert!(matches!(
					StateTrieMigration::migration_process(),
					MigrationTask { size: x, .. } if x > 0,
				));
			}
		});
	}

	#[test]
	fn custom_migrate_top_works() {
		new_test_ext(StateVersion::V0, true).execute_with(|| {
			frame_support::assert_ok!(StateTrieMigration::migrate_custom_top(
				Origin::signed(1),
				vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()],
				3 + sp_core::storage::TRIE_VALUE_NODE_THRESHOLD * 3 + 1 + 2 + 3,
			));

			// no funds should remain reserved.
			assert_eq!(Balances::reserved_balance(&1), 0);
			assert_eq!(Balances::free_balance(&1), 1000);
		});

		new_test_ext(StateVersion::V0, true).execute_with(|| {
			assert_eq!(Balances::free_balance(&1), 1000);

			// note that we don't expect this to be a noop -- we do slash.
			frame_support::assert_err!(
				StateTrieMigration::migrate_custom_top(
					Origin::signed(1),
					vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()],
					69, // wrong witness
				),
				"wrong witness data"
			);

			// no funds should remain reserved.
			assert_eq!(Balances::reserved_balance(&1), 0);
			assert_eq!(
				Balances::free_balance(&1),
				1000 - (3 * SignedDepositPerItem::get() + SignedDepositBase::get())
			);
		});
	}

	#[test]
	fn custom_migrate_child_works() {
		let childify = |s: &'static str| {
			let mut string = DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec();
			string.extend_from_slice(s.as_ref());
			string
		};

		new_test_ext(StateVersion::V0, true).execute_with(|| {
			frame_support::assert_ok!(StateTrieMigration::migrate_custom_child(
				Origin::signed(1),
				childify("chk1"),
				vec![b"key1".to_vec(), b"key2".to_vec()],
				55 + 66,
			));

			// no funds should remain reserved.
			assert_eq!(Balances::reserved_balance(&1), 0);
			assert_eq!(Balances::free_balance(&1), 1000);
		});

		new_test_ext(StateVersion::V0, true).execute_with(|| {
			assert_eq!(Balances::free_balance(&1), 1000);

			// note that we don't expect this to be a noop -- we do slash.
			assert!(StateTrieMigration::migrate_custom_child(
				Origin::signed(1),
				childify("chk1"),
				vec![b"key1".to_vec(), b"key2".to_vec()],
				999999, // wrong witness
			)
			.is_err());

			// no funds should remain reserved.
			assert_eq!(Balances::reserved_balance(&1), 0);
			assert_eq!(
				Balances::free_balance(&1),
				1000 - (2 * SignedDepositPerItem::get() + SignedDepositBase::get())
			);
		});
	}
}

#[cfg(all(test, feature = "remote-tests"))]
mod remote_tests {
	use super::{mock::*, *};
	use codec::Encode;
	use remote_externalities::{Mode, OfflineConfig, OnlineConfig};
	use sp_runtime::traits::{Bounded, HashFor};
	use std::sync::Arc;

	// we only use the hash type from this, so using the mock should be fine.
	type Block = sp_runtime::testing::Block<Extrinsic>;

	#[tokio::test]
	async fn on_initialize_migration() {
		sp_tracing::try_init_simple();
		let run_with_limits = |limits| async move {
			let mut ext = remote_externalities::Builder::<Block>::new()
				.mode(Mode::OfflineOrElseOnline(
					OfflineConfig {
						state_snapshot: "/home/kianenigma/remote-builds/state".to_owned().into(),
					},
					OnlineConfig {
						transport: std::env!("WS_API").to_owned().into(),
						state_snapshot: Some(
							"/home/kianenigma/remote-builds/state".to_owned().into(),
						),
						..Default::default()
					},
				))
				.state_version(sp_core::storage::StateVersion::V0)
				.build()
				.await
				.unwrap();

			let mut now = ext.execute_with(|| {
				AutoLimits::<Test>::put(Some(limits));
				// requires the block number type in our tests to be same as with mainnet, u32.
				frame_system::Pallet::<Test>::block_number()
			});

			let mut duration = 0;
			// set the version to 1, as if the upgrade happened.
			ext.state_version = sp_core::storage::StateVersion::V1;

			let (top_left, child_left) =
				ext.as_backend().essence().check_migration_state().unwrap();
			assert!(top_left > 0);

			log::info!(
				target: LOG_TARGET,
				"initial check: top_left: {}, child_left: {}",
				top_left,
				child_left,
			);

			loop {
				let last_state_root = ext.backend.root().clone();
				let (finished, proof) = ext.execute_and_prove(|| {
					run_to_block(now + 1);
					if StateTrieMigration::migration_process().finished() {
						return true
					}
					duration += 1;
					now += 1;
					false
				});

				let compact_proof =
					proof.clone().into_compact_proof::<HashFor<Block>>(last_state_root).unwrap();
				log::info!(
					target: LOG_TARGET,
					"proceeded to #{}, original proof: {}, compact proof size: {}, compact zstd compressed: {}",
					now,
					proof.encoded_size(),
					compact_proof.encoded_size(),
					zstd::stream::encode_all(&compact_proof.encode()[..], 0).unwrap().len(),
				);
				ext.commit_all().unwrap();

				if finished {
					break
				}
			}

			ext.execute_with(|| {
				log::info!(
					target: LOG_TARGET,
					"finished on_initialize migration in {} block, final state of the task: {:?}",
					duration,
					StateTrieMigration::migration_process(),
				)
			});

			let (top_left, child_left) =
				ext.as_backend().essence().check_migration_state().unwrap();
			assert_eq!(top_left, 0);
			assert_eq!(child_left, 0);
		};

		// item being the bottleneck
		run_with_limits(MigrationLimits { item: 8 * 1024, size: 128 * 1024 * 1024 }).await;
		// size being the bottleneck
		run_with_limits(MigrationLimits { item: Bounded::max_value(), size: 64 * 1024 }).await;
	}
}
