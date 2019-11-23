use support::{
    decl_module, decl_storage, decl_event, dispatch::Result, ensure, print,
    traits::{
        LockableCurrency, WithdrawReason, WithdrawReasons, LockIdentifier, Get, Currency,
    }
};
use system::ensure_signed;
use codec::{Encode, Decode};
use rstd::prelude::Vec;
use sr_primitives::traits::{Hash, CheckedAdd, SaturatedConversion};

// Option: {title: String, pot: u64, voters: <Vec:T::AccountId>}
// Voter: {accountId, votedVotes:<Vec: u64>, timeLastVoted: timestamp, balance: balances}
// Vote: {id, creator, method, timestamp, expiredate, voters:<Vec:T::AccountId>, options:<Vec: Option>
#[derive(PartialEq, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct Vote<AccountId, BlockNumber> {
    id: u64,
    creator: AccountId,
    when: BlockNumber,
    vote_ends: BlockNumber,
    concluded: bool,
    vote_type: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum Ballot {
    Aye,
    Nay,
}

#[derive(PartialEq, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct LockInfo<Balance, BlockNumber> {
    deposit: Balance,
    duration: BlockNumber,
    until: BlockNumber
}

pub type ReferenceIndex = u64;
pub type BalanceOf<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::Balance;

// import Trait from balances, timestamp, event
pub trait Trait: balances::Trait + system::Trait {
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
    type Currency: LockableCurrency<Self::AccountId, Moment=Self::BlockNumber>;
    type LockPeriod: Get<Self::BlockNumber>;
}

decl_event!(
	pub enum Event<T> where AccountId = <T as system::Trait>::AccountId {
        //created, voted, withdrawn, finalized
        Created(AccountId, u64),
        Voted(AccountId, u64, Ballot),
        Concluded(u64),
        Withdrew(AccountId, ReferenceIndex),
	}
);

decl_storage! {
    // -AllVoteCount: u64 -> increment every time any vote is created
    // -VotesByIndex: map u64 -> Vote<T::AccountId>;
    // -VoteCreator: map u64 => Option<T::AccountId>;
    // VoteByCreator: map T::AccountId => <Vec: u64>;
    trait Store for Module<T: Trait> as GovernanceModule {
        // All votes
        AllVoteCount get(all_vote_count): u64;
        VotesByIndex get(votes): map u64 => Vote<T::AccountId, T::BlockNumber>;

        // Creator
        VoteCreator get(creator_of): map u64 => Option<T::AccountId>;
        CreatedVoteCount get(created_by): map T::AccountId => u64; // increment everytime created

        // VoteByCreatorArray get(created_by): map T::AccountId => <Vec: u64>;
        VoteByCreatorArray get(created_by_and_index): map (T::AccountId, u64) => Vote<T::AccountId, T::BlockNumber>;

        VoteResults: map u64 => Vec<u64>;
        VoteIndexHash get(index_hash): map u64 => T::Hash;

        // VotedAccounts:[aye:[AccountId], nay:[AccountId],....]
        VotedAccounts: map (ReferenceIndex, u8) => Vec<T::AccountId>;

        LockBalance: map (ReferenceIndex, T::AccountId) => LockInfo<BalanceOf<T>, T::BlockNumber>;
        LockCount get(lock_count): u64;
    }
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        fn deposit_event() = default;

        // Creator Modules
        // Create a new vote
        // TODO: Takes expiring time, title as data: Vec, voting_type
        pub fn create_vote(origin, vote_type:u8, exp_length: T::BlockNumber ,data: Vec<u8>) -> Result {
            let sender = ensure_signed(origin)?;
            ensure!(data.len() <= 256, "listing data cannot be more than 256 bytes");

            let new_vote_num = <AllVoteCount>::get().checked_add(1)
                .ok_or("Overflow adding vote count")?;
            let vote_count_by_sender = <CreatedVoteCount<T>>::get(sender.clone()).checked_add(1)
                .ok_or("Overflow adding vote count to the sender")?;
            // let exp_length = 60000.into(); // 30sec for test
            let now = <system::Module<T>>::block_number();
            // check if resolved if now > vote_exp
            let vote_exp = now.checked_add(&exp_length.into()).ok_or("Overflow when setting application expiry.")?;
            let hashed = <T as system::Trait>::Hashing::hash(&data);
            let new_vote = Vote{
                id: new_vote_num,
                vote_type,
                creator: sender.clone(),
                when: now,
                vote_ends: vote_exp,
                concluded: false,
            };

            Self::mint_vote(sender, new_vote, vote_count_by_sender, new_vote_num)?;
            <VoteIndexHash<T>>::insert(new_vote_num, hashed);
            Ok(())
        }

        fn cast_lockvote(origin, reference_index: ReferenceIndex, ballot: Ballot, deposit: BalanceOf<T>, duration: T::BlockNumber) -> Result {
            let sender = ensure_signed(origin)?;
            let vote = Self::votes(&reference_index);
            let now = <system::Module<T>>::block_number();
            let lock_id: LockIdentifier = reference_index.to_be_bytes();
            let current_blocknumber = <system::Module<T>>::block_number();
            // duration should be at least vote_end
            // deposit should be smaller than freebalance
            ensure!(now + duration >= vote.vote_ends, "Lock duration should be or bigger than vote expiry.");
            ensure!(!<LockBalance<T>>::exists((&reference_index, &sender)), "You cannot lockvote twice.");
            ensure!(T::Currency::free_balance(&sender) > deposit, "You cannot lock more than your free balance!");
            ensure!(<VotesByIndex<T>>::exists(&reference_index), "Vote doesn't exists");
            ensure!(vote.creator != sender, "You cannot vote your own vote.");
            ensure!(vote.vote_ends > now, "This vote has already been expired.");
            ensure!(vote.vote_type == 1, "This vote is not LockVote.");
            
            // lock function
            <LockBalance<T>>::mutate((&reference_index, &sender), |lockinfo| {
                lockinfo.deposit += deposit;
                lockinfo.duration = duration;
                lockinfo.until = current_blocknumber + duration;
            });
            T::Currency::set_lock(
                lock_id,
                &sender,
                deposit,
                T::LockPeriod::get(),   // use withdraw function
                WithdrawReasons::except(WithdrawReason::TransactionPayment),
            );
            Self::cast_ballot_f(sender, reference_index, ballot)?; // includes checks
            Ok(())
        }

        // Withdraws locked token
        // Takes reference_index and sender accountId
        // checks:
            // a: if the vote exists and type is 1
            // b: the vote has concluded. Cannot tally if withdrawn before conclusion 
            // b: ensure sender has locked the vote
            // c: ensure the lock period is over
        fn withdraw(origin, reference_index: ReferenceIndex) -> Result {
            let sender = ensure_signed(origin)?;
            let vote = Self::votes(reference_index);
            ensure!(vote.vote_type == 1, "This must be lockvote: vote_type: 1!");
            ensure!(vote.concluded == true, "You have to wait at least until the vote concludes!");
            ensure!(<LockBalance<T>>::exists((&reference_index, &sender)), "You need to participate lockvoting to call this function!");
            let lock_info = <LockBalance<T>>::get((&reference_index, &sender));
            ensure!(lock_info.until < <system::Module<T>>::block_number(), "You need to wait until the lock period is over!");
            T::Currency::remove_lock(
                reference_index.to_be_bytes(),
                &sender
            );
            <LockBalance<T>>::remove((&reference_index, &sender));
            print("Locked token is withdrawn!");
            Self::deposit_event(RawEvent::Withdrew(sender, reference_index));
    
            Ok(())
        }
        // Voter modules
        // cast_ballot checks
            // a. the vote exists
            // b. vote hasnt expired
            // c. the voter hasnt voted yet in the same option. If voted in different option, change the vote.
        fn cast_ballot(origin, reference_index: ReferenceIndex, ballot: Ballot) -> Result {
            let sender = ensure_signed(origin)?;
            let vote = <VotesByIndex<T>>::get(&reference_index);
            let now = <system::Module<T>>::block_number();
            ensure!(<VotesByIndex<T>>::exists(&reference_index), "Vote doesn't exists");
            ensure!(vote.creator != sender, "You cannot vote your own vote.");
            ensure!(vote.vote_ends > now, "This vote has already been expired.");
            ensure!(vote.vote_type == 0, "This vote is LockVote. Use 'cast_lockvote' instead!");
            let mut accounts_aye = <VotedAccounts<T>>::get((reference_index, 0));
            let mut accounts_nay = <VotedAccounts<T>>::get((reference_index, 1));
            // keep track of voter's id in aye or nay vector in Vote
            // Voter can change his vote b/w aye and nay
            // Voter cannot vote twice
            match ballot {
                Ballot::Aye => {
                    ensure!(!accounts_aye.contains(&sender), "You have already voted aye.");
                    // if sender has voted for the other option, remove from the array
                    if accounts_nay.contains(&sender) {
                        let i = accounts_nay.iter().position(|x| x == &sender).unwrap() as usize;
                        accounts_nay.remove(i);
                    } 
                    accounts_aye.push(sender.clone());
                    <VotedAccounts<T>>::insert((reference_index, 0), accounts_aye);
                    print("Ballot casted Aye!");
                }
                Ballot::Nay => {
                    ensure!(!accounts_nay.contains(&sender), "You have already voted nay.");
                    if accounts_aye.contains(&sender) {
                        let i = accounts_aye.iter().position(|x| x == &sender).unwrap() as usize;
                        accounts_aye.remove(i);
                    } 
                    accounts_nay.push(sender.clone());
                    <VotedAccounts<T>>::insert((reference_index, 1), accounts_nay);
                    print("Ballot casted Nay!");
                }
            }
            Self::deposit_event(RawEvent::Voted(sender, reference_index, ballot));
            Ok(())
        }

        // conclude a vote given expired
        // anyone can call this function, and Vote.concluded returns true
        pub fn conclude_vote(_origin, reference_index: u64) -> Result {
            let vote = <VotesByIndex<T>>::get(&reference_index);
            // ensure the vote is concluded before tallying
            ensure!(vote.concluded == false, "This vote has already concluded.");
            let now = <system::Module<T>>::block_number();
            // double check
            ensure!(now > vote.vote_ends, "This vote hasn't been expired yet.");
            Self::tally(reference_index)?;
            // For some reason Storage is not reflected, but works.
            <VotesByIndex<T>>::mutate(&reference_index, |vote| vote.concluded = true);
            <VoteByCreatorArray<T>>::mutate((vote.creator, &reference_index), |vote| vote.concluded = true);
            Self::deposit_event(RawEvent::Concluded(reference_index));
            print("Vote concluded.");
            Ok(())
        }
    }
}

impl<T: Trait> Module<T> {
    // keep track of accounts in array by Aye/Nay in <VotedAccounts<T>>
    // TODO: lockvote_tally should check <LockBalance> for accuracy
    fn cast_ballot_f(sender: T::AccountId, reference_index: ReferenceIndex, ballot: Ballot) -> Result {
        let mut accounts_aye = <VotedAccounts<T>>::get((reference_index, 0));
        let mut accounts_nay = <VotedAccounts<T>>::get((reference_index, 1));
        // keep track of voter's id in aye or nay vector in Vote
        // Voter can change his vote b/w aye and nay
        // Voter cannot vote twice
        match ballot {
            Ballot::Aye => {
                ensure!(!accounts_aye.contains(&sender), "You have already voted aye.");
                // if sender has voted for the other option, remove from the array
                if accounts_nay.contains(&sender) {
                    let i = accounts_nay.iter().position(|x| x == &sender).unwrap() as usize;
                    accounts_nay.remove(i);
                } 
                accounts_aye.push(sender.clone());
                <VotedAccounts<T>>::insert((reference_index, 0), accounts_aye);
                print("Ballot casted Aye!");
            }
            Ballot::Nay => {
                ensure!(!accounts_nay.contains(&sender), "You have already voted nay.");
                if accounts_aye.contains(&sender) {
                    let i = accounts_aye.iter().position(|x| x == &sender).unwrap() as usize;
                    accounts_aye.remove(i);
                } 
                accounts_nay.push(sender.clone());
                <VotedAccounts<T>>::insert((reference_index, 1), accounts_nay);
                print("Ballot casted Nay!");
            }
        }
        Self::deposit_event(RawEvent::Voted(sender, reference_index, ballot));
        Ok(())
    }
    fn mint_vote(sender: T::AccountId, new_vote: Vote<T::AccountId, T::BlockNumber>, vote_count_by_sender: u64, new_vote_num: u64 ) -> Result{
        ensure!(!<VotesByIndex<T>>::exists(&new_vote_num), "Vote already exists");

        <VotesByIndex<T>>::insert(new_vote_num.clone(), &new_vote);
        <VoteCreator<T>>::insert(new_vote_num.clone(), sender.clone());
        <CreatedVoteCount<T>>::insert(sender.clone(), vote_count_by_sender);
        <AllVoteCount>::put(new_vote_num.clone());
        <VoteByCreatorArray<T>>::insert((sender.clone(), new_vote_num), new_vote);

        Self::deposit_event(RawEvent::Created(sender, new_vote_num));
        print("Vote created!");
        Ok(())
    }

    // only called after the vote expired
    fn tally(reference_index: u64) -> Result {
        let vote = Self::votes(reference_index);
        let mut aye_count: u64 = 0;
        let mut nay_count: u64 = 0;
        match vote.vote_type {
            // normal vote tally
            0 => {
                aye_count = <VotedAccounts<T>>::get((reference_index, 0)).len() as u64;
                nay_count = <VotedAccounts<T>>::get((reference_index, 1)).len() as u64;
            }
            // lock vote tally
            // deposit amount * duration
            1 => {
                for account in <VotedAccounts<T>>::get((reference_index, 0)) {
                    let lock_vote = <LockBalance<T>>::get((reference_index, account));
                    let vote_power: u64 = lock_vote.deposit.saturated_into::<u64>() * lock_vote.duration.saturated_into::<u64>();
                    aye_count += vote_power;
                }
                for account in <VotedAccounts<T>>::get((reference_index, 0)) {
                    let lock_vote = <LockBalance<T>>::get((reference_index, account));
                    let vote_power: u64 = lock_vote.deposit.saturated_into::<u64>() * lock_vote.duration.saturated_into::<u64>();
                    nay_count += vote_power;
                }
            }
            _ => ensure!(vote.vote_type <= 1, "This vote_type is not covered."),
        }
        let mut result:Vec<u64> = Vec::new();
        result.push(aye_count);
        result.push(nay_count);
        <VoteResults>::insert(reference_index, result);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
	use super::*;
    use system::ensure_signed;
    use codec::{Encode, Decode};
    use support::{
        decl_module, decl_storage, decl_event, dispatch::Result, ensure, print, impl_outer_origin, assert_ok, assert_noop, parameter_types,
        traits::{
            LockableCurrency, WithdrawReason, WithdrawReasons, LockIdentifier, Get, Currency, Imbalance,
        }
    };
    use rstd::prelude::Vec;
    use sr_primitives::traits::{Hash, CheckedAdd, SaturatedConversion};
    use runtime_io::{TestExternalities};
    use primitives::{H256, Blake2Hasher};
    use sr_primitives::{
        BuildStorage, Perbill, traits::{BlakeTwo256, IdentityLookup},
        testing::{Digest, DigestItem, Header}
    };

    impl_outer_origin! {
        pub enum Origin for Test {}
    }

    #[derive(Clone, Eq, PartialEq)]
    pub struct Test;

    type PositiveImbalance<T> =
	    <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::PositiveImbalance;
    type NegativeImbalance<T> =
        <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;

    type BalanceOf<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::Balance;
    
    impl Trait for Test {
        type Event = ();
        type Currency = balances::Module<Test>;
        type LockPeriod = BlockHashCount;
    }
    parameter_types! {
        pub const BlockHashCount: u64 = 250;
        pub const MaximumBlockWeight: u32 = 1024;
        pub const MaximumBlockLength: u32 = 2 * 1024;
        pub const AvailableBlockRatio: Perbill = Perbill::one();
        pub const MinimumPeriod: u64 = 1;
        pub const ExistentialDeposit: u64 = 0;
		pub const TransferFee: u64 = 0;
        pub const CreationFee: u64 = 0;
    }


    impl system::Trait for Test {
        type Origin = Origin;
        type Index = u64;
        type BlockNumber = u64;
        type Call = ();
        type Hash = H256;
        type Hashing = ::sr_primitives::traits::BlakeTwo256;
        type AccountId = u64;
        type Lookup = IdentityLookup<Self::AccountId>;
        type Header = Header;
        type Event = ();
        type BlockHashCount = BlockHashCount;
        type MaximumBlockWeight = MaximumBlockWeight;
        type AvailableBlockRatio = AvailableBlockRatio;
        type MaximumBlockLength = MaximumBlockLength;
        type Version = ();
    }

    impl balances::Trait for Test {
		type Balance = u64;
		type OnFreeBalanceZero = ();
		type OnNewAccount = ();
		type Event = ();
		type TransferPayment = ();
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type TransferFee = TransferFee;
		type CreationFee = CreationFee;
	}

    // type PositiveImbalance<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::PositiveImbalance;
    // type NegativeImbalance<T> = <<T as Trait>::Currency as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;
    // type Currency = dyn LockableCurrency<Self::AccountId, Moment=Self::BlockNumber>;
    // type LockPeriod = dyn Get<Self::BlockNumber>;

    type Governance = Module<Test>;
    type System = system::Module<Test>;

    // fn new_test_ext() -> runtime_io::TestExternalities<Blake2Hasher> {
    //     system::GenesisConfig::default().build_storage::<Test>().unwrap().into()
    // }

    // #[test]
    // fn my_runtime_test() {
    //     with_externalities(&mut new_test_ext(), || {
    //         assert_ok!(Governance::start_auction());
    //         //10ブロック進める
    //         run_to_block(10);
    //         assert_ok!(Governance::end_auction());
    //     });
    // }
    
    #[test]
    fn it_works() {
        TestExternalities::default().execute_with(|| {
            assert!(true);
        });
    }
    
	// #[test]
	// fn vote_creation() {
    //     with_externalities(&mut new_test_ext(), || {
    //         // assert_ok!(Governance::create_vote(Origin::signed(1), 0, 10, "Vote1".as_bytes(),into()));
    //         assert!(true);
    //     })
    // }
}
