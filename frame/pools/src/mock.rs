use super::*;
use crate::{self as pools};
use frame_support::{assert_ok, parameter_types};
use frame_system::RawOrigin;

pub type AccountId = u32;
pub type Balance = u128;

/// Pool 0's primary account id (i.e. its stash and controller account with bonded funds).
pub const PRIMARY_ACCOUNT: u32 = 2536596763;
/// Pool 0's reward destination.
pub const REWARDS_ACCOUNT: u32 = 736857005;

parameter_types! {
	pub static CurrentEra: EraIndex = 0;
	static BondedBalanceMap: std::collections::HashMap<AccountId, Balance> = Default::default();
	static UnbondingBalanceMap: std::collections::HashMap<AccountId, Balance> = Default::default();
	pub static CanBondExtra: bool = true;
	pub static CanBond: bool = true;
	pub static CanNominate: bool = true;
}

pub struct StakingMock;
impl StakingMock {
	pub(crate) fn set_bonded_balance(who: AccountId, bonded: Balance) {
		BONDED_BALANCE_MAP.with(|m| m.borrow_mut().insert(who, bonded));
	}
}

impl sp_staking::StakingInterface for StakingMock {
	type Balance = Balance;
	type AccountId = AccountId;
	type LookupSource = Self::AccountId;

	fn minimum_bond() -> Self::Balance {
		10
	}

	fn current_era() -> EraIndex {
		CurrentEra::get()
	}

	fn bonding_duration() -> EraIndex {
		3
	}

	fn bonded_balance(who: &Self::AccountId) -> Self::Balance {
		BondedBalanceMap::get().get(who).map(|v| *v).unwrap_or_default()
	}

	fn can_bond_extra(_: &Self::AccountId, _: Self::Balance) -> bool {
		CanBondExtra::get()
	}

	fn bond_extra(who: &Self::AccountId, extra: Self::Balance) -> DispatchResult {
		BONDED_BALANCE_MAP.with(|m| *m.borrow_mut().get_mut(who).unwrap() += extra);
		Ok(())
	}

	fn unbond(who: &Self::AccountId, amount: Self::Balance) -> DispatchResult {
		BONDED_BALANCE_MAP.with(|m| *m.borrow_mut().get_mut(who).unwrap() -= amount);
		UNBONDING_BALANCE_MAP
			.with(|m| *m.borrow_mut().entry(*who).or_insert(Self::Balance::zero()) += amount);
		Ok(())
	}

	fn withdraw_unbonded(who: &Self::AccountId) -> DispatchResult {
		let maybe_new_free = UNBONDING_BALANCE_MAP.with(|m| m.borrow_mut().remove(who));
		if let Some(new_free) = maybe_new_free {
			assert_ok!(Balances::mutate_account(who, |a| a.free += new_free));
		}
		Ok(())
	}

	fn can_bond(_: &Self::AccountId, _: &Self::AccountId, _: &Self::AccountId) -> bool {
		CanBond::get()
	}

	fn bond(
		stash: Self::AccountId,
		_: Self::AccountId,
		amount: Self::Balance,
		_: Self::AccountId,
	) -> DispatchResult {
		StakingMock::set_bonded_balance(stash, amount);
		Ok(())
	}

	fn can_nominate(_: &Self::AccountId, _: &Vec<Self::LookupSource>) -> bool {
		CanNominate::get()
	}

	fn nominate(_: Self::AccountId, _: Vec<Self::LookupSource>) -> DispatchResult {
		Ok(())
	}
}

impl frame_system::Config for Runtime {
	type SS58Prefix = ();
	type BaseCallFilter = frame_support::traits::Everything;
	type Origin = Origin;
	type Index = u64;
	type BlockNumber = u64;
	type Call = Call;
	type Hash = sp_core::H256;
	type Hashing = sp_runtime::traits::BlakeTwo256;
	type AccountId = AccountId;
	type Lookup = sp_runtime::traits::IdentityLookup<Self::AccountId>;
	type Header = sp_runtime::testing::Header;
	type Event = Event;
	type BlockHashCount = ();
	type DbWeight = ();
	type BlockLength = ();
	type BlockWeights = ();
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = pallet_balances::AccountData<Balance>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type SystemWeightInfo = ();
	type OnSetCode = ();
	type MaxConsumers = frame_support::traits::ConstU32<16>;
}

parameter_types! {
	pub static ExistentialDeposit: Balance = 5;
}

impl pallet_balances::Config for Runtime {
	type MaxLocks = frame_support::traits::ConstU32<1024>;
	type MaxReserves = ();
	type ReserveIdentifier = [u8; 8];
	type Balance = Balance;
	type Event = Event;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
}

pub struct BalanceToU256;
impl Convert<Balance, U256> for BalanceToU256 {
	fn convert(n: Balance) -> U256 {
		n.into()
	}
}

pub struct U256ToBalance;
impl Convert<U256, Balance> for U256ToBalance {
	fn convert(n: U256) -> Balance {
		n.try_into().unwrap()
	}
}

parameter_types! {
	pub static MaxUnbonding: u32 = 5;
}

impl pools::Config for Runtime {
	type Event = Event;
	type Currency = Balances;
	type BalanceToU256 = BalanceToU256;
	type U256ToBalance = U256ToBalance;
	type StakingInterface = StakingMock;
	type MaxUnbonding = MaxUnbonding;
}

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Runtime>;
type Block = frame_system::mocking::MockBlock<Runtime>;
frame_support::construct_runtime!(
	pub enum Runtime where
		Block = Block,
		NodeBlock = Block,
		UncheckedExtrinsic = UncheckedExtrinsic,
	{
		System: frame_system::{Pallet, Call, Storage, Event<T>, Config},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		Pools: pools::{Pallet, Call, Storage, Event<T>},
	}
);

#[derive(Default)]
pub struct ExtBuilder {
	delegators: Vec<(AccountId, Balance)>,
}

impl ExtBuilder {
	// Add delegators to pool 0.
	pub(crate) fn add_delegators(mut self, delegators: Vec<(AccountId, Balance)>) -> Self {
		self.delegators = delegators;
		self
	}

	pub(crate) fn build(self) -> sp_io::TestExternalities {
		sp_tracing::try_init_simple();
		let storage = frame_system::GenesisConfig::default().build_storage::<Runtime>().unwrap();

		let mut ext = sp_io::TestExternalities::from(storage);

		ext.execute_with(|| {
			// make a pool
			let amount_to_bond = <Runtime as pools::Config>::StakingInterface::minimum_bond();
			Balances::make_free_balance_be(&10, amount_to_bond * 2);
			assert_ok!(Pools::create(RawOrigin::Signed(10).into(), 0, vec![100], amount_to_bond));

			for (account_id, bonded) in self.delegators {
				Balances::make_free_balance_be(&account_id, bonded * 2);

				assert_ok!(Pools::join(RawOrigin::Signed(account_id).into(), bonded, 0));
			}
		});

		ext
	}

	pub fn build_and_execute(self, test: impl FnOnce() -> ()) {
		self.build().execute_with(|| {
			test();
			// post-checks can be added here
		})
	}
}

#[cfg(test)]
mod test {
	use super::*;
	#[test]
	fn u256_to_balance_convert_works() {
		assert_eq!(U256ToBalance::convert(0u32.into()), Zero::zero());
		assert_eq!(U256ToBalance::convert(Balance::max_value().into()), Balance::max_value())
	}

	#[test]
	#[should_panic]
	fn u256_to_balance_convert_panics_correctly() {
		U256ToBalance::convert(U256::from(Balance::max_value()).saturating_add(1u32.into()));
	}

	#[test]
	fn balance_to_u256_convert_works() {
		assert_eq!(BalanceToU256::convert(0u32.into()), U256::zero());
		assert_eq!(BalanceToU256::convert(Balance::max_value()), Balance::max_value().into())
	}
}
