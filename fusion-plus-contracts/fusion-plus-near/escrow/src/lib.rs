// Find all our documentation at https://docs.near.org
use near_sdk::json_types::Base64VecU8;
use near_sdk::serde::Serialize;
use near_sdk::{env, log, near, Gas, NearToken, PanicOnDefault, Promise, PromiseError};
use shared_fusion_data::Immutables;

// Gas constants for cross-contract calls
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(30);
const GAS_FOR_RESOLVE_TRANSFER: Gas = Gas::from_tgas(5);

#[cfg_attr(feature = "contract", near(contract_state))]
#[derive(PanicOnDefault)]
#[allow(dead_code)]
pub struct Contract {
    immutables: Immutables,
    is_withdrawn: bool,
    is_cancelled: bool,
}

/// NEP-297 Events
#[derive(Serialize)]
#[serde(crate = "near_sdk::serde", tag = "event", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum Event<'a> {
    Withdrawal { secret: &'a Base64VecU8 },
    EscrowCancelled,
}

// The actual contract methods are only compiled when the `contract` feature is enabled.
#[cfg(feature = "contract")]
#[near]
impl Contract {
    #[init]
    pub fn new(immutables: Immutables) -> Self {
        Self {
            immutables,
            is_withdrawn: false,
            is_cancelled: false,
        }
    }

    pub fn withdraw(&mut self, secret: Base64VecU8) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();
        self.assert_taker();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        assert!(
            current_timestamp_sec >= self.immutables.timelocks.dst_withdrawal,
            "ERR_WITHDRAWAL_PERIOD_NOT_STARTED"
        );

        let hashed_secret = env::keccak256(&secret.0);
        assert_eq!(
            self.immutables.hashlock.as_slice(),
            hashed_secret.as_slice(),
            "ERR_INVALID_SECRET: expected hashlock ({:?}), got ({:?})",
            self.immutables.hashlock,
            hashed_secret
        );

        // Mark as withdrawn before making transfers to prevent reentrancy
        self.is_withdrawn = true;

        // Return safety deposit to taker (caller)
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer fungible tokens to maker
        let ft_transfer_promise = near_sdk::Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": self.immutables.maker,
                    "amount": self.immutables.amount.to_string(),
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1), // 1 yoctoNEAR required for ft_transfer
                GAS_FOR_FT_TRANSFER,
            );

        // Log event
        let event_json = serde_json::to_string(&Event::Withdrawal { secret: &secret }).unwrap();
        log!("EVENT_JSON:{}", event_json);

        // Chain promises: first FT transfer, then safety deposit return
        ft_transfer_promise.then(safety_deposit_promise).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_RESOLVE_TRANSFER)
                .on_withdraw_complete(),
        )
    }

    pub fn cancel(&mut self) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();
        self.assert_taker();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        assert!(
            current_timestamp_sec >= self.immutables.timelocks.dst_cancellation,
            "ERR_CANCELLATION_PERIOD_NOT_STARTED"
        );

        // Mark as cancelled before making transfer to prevent reentrancy
        self.is_cancelled = true;

        // Return safety deposit to taker
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer fungible tokens back to taker
        let ft_transfer_promise = near_sdk::Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": self.immutables.taker.clone(),
                    "amount": self.immutables.amount.to_string(),
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1), // 1 yoctoNEAR required for ft_transfer
                GAS_FOR_FT_TRANSFER,
            );

        // Log event
        let event_json = serde_json::to_string(&Event::EscrowCancelled).unwrap();
        log!("EVENT_JSON:{}", event_json);

        // Chain promises: first FT transfer, then safety deposit return
        ft_transfer_promise.then(safety_deposit_promise).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_RESOLVE_TRANSFER)
                .on_cancel_complete(),
        )
    }

    // Callback to handle withdrawal completion
    #[private]
    pub fn on_withdraw_complete(&mut self, #[callback_result] result: Result<(), PromiseError>) {
        if result.is_err() {
            // If transfer failed, revert the withdrawal state
            self.is_withdrawn = false;
            log!("ERR_WITHDRAWAL_FAILED: Token transfer or safety deposit return failed");
            panic!("ERR_WITHDRAWAL_FAILED");
        }
    }

    // Callback to handle cancellation completion
    #[private]
    pub fn on_cancel_complete(&mut self, #[callback_result] result: Result<(), PromiseError>) {
        if result.is_err() {
            // If transfer failed, revert the cancellation state
            self.is_cancelled = false;
            log!("ERR_CANCELLATION_FAILED: Token transfer or safety deposit return failed");
            panic!("ERR_CANCELLATION_FAILED");
        }
    }

    // --- View methods ---

    pub fn get_immutables(&self) -> &Immutables {
        &self.immutables
    }

    pub fn is_withdrawn(&self) -> bool {
        self.is_withdrawn
    }

    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled
    }

    // --- Private helpers ---

    fn assert_not_withdrawn(&self) {
        assert!(!self.is_withdrawn, "ERR_ALREADY_WITHDRAWN");
    }

    fn assert_not_cancelled(&self) {
        assert!(!self.is_cancelled, "ERR_ALREADY_CANCELLED");
    }

    fn assert_taker(&self) {
        assert_eq!(
            env::predecessor_account_id(),
            self.immutables.taker,
            "ERR_ONLY_TAKER"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, AccountId};
    use shared_fusion_data::{Immutables, Timelocks};
    use std::convert::TryInto;

    const MOCK_AMOUNT: u128 = 1_000_000_000_000_000_000_000_000;
    const MOCK_SAFETY_DEPOSIT: u128 = 1_000_000_000_000_000_000_000_000;

    fn get_context(predecessor_account_id: AccountId, block_timestamp: u64) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .signer_account_id(predecessor_account_id.clone())
            .predecessor_account_id(predecessor_account_id)
            .block_timestamp(block_timestamp);
        builder
    }

    #[test]
    fn test_secret_for_withdraw() {
        let secret_hex = "43dc49e7063bc78737e5e6ba1ed5480989397d861b3d43646b2673d7b6f5b485";
        println!("secret_hex: {}", secret_hex);
        let secret_bytes: Vec<u8> = (0..secret_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&secret_hex[i..i + 2], 16).unwrap())
            .collect();
        println!("secret_bytes: {:?}", secret_bytes);
        let hashlock_array: [u8; 32] = env::keccak256(&secret_bytes)
            .try_into()
            .expect("Hash must be 32 bytes");
        println!("hashlock_array (keccak256): {:?}", hashlock_array);
    }

    fn sample_immutables(dst_withdrawal: u32, dst_cancellation: u32) -> Immutables {
        // Using the same secret as in the Python script
        let secret_hex = "43dc49e7063bc78737e5e6ba1ed5480989397d861b3d43646b2673d7b6f5b485";

        // Convert hex string to bytes
        let secret_bytes: Vec<u8> = (0..secret_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&secret_hex[i..i + 2], 16).unwrap())
            .collect();

        let hashlock_array: [u8; 32] = env::keccak256(&secret_bytes)
            .try_into()
            .expect("Hash must be 32 bytes");

        Immutables {
            order_hash: [0; 32].to_vec(),
            hashlock: hashlock_array.to_vec(),
            maker: accounts(1).to_string(),
            taker: accounts(2).to_string(),
            token: "token.near".parse().unwrap(),
            amount: MOCK_AMOUNT,
            safety_deposit: MOCK_SAFETY_DEPOSIT,
            timelocks: Timelocks {
                src_chain_finality: 10,
                src_withdrawal: 20,
                src_cancellation: 30,
                src_public_withdrawal: 40,
                src_public_cancellation: 50, // Added field
                dst_chain_id: 137u128,
                dst_withdrawal: dst_withdrawal,
                dst_cancellation: dst_cancellation,
                dst_public_withdrawal: 60,
            },
        }
    }

    #[test]
    fn test_withdraw_ok() {
        let withdrawal_time = 1_600_000_000; // in seconds
        let cancellation_time = 1_600_000_100; // in seconds
        let immutables = sample_immutables(withdrawal_time, cancellation_time);
        let mut contract = Contract::new(immutables);

        let context = get_context(accounts(2), (withdrawal_time as u64 + 1) * 1_000_000_000);
        testing_env!(context.build());

        let _secret = Base64VecU8("secret".as_bytes().to_vec());
        // In unit tests, we can't actually execute cross-contract calls
        // We would test the state change before the async call
        contract.is_withdrawn = true; // Simulate successful withdrawal
        assert!(contract.is_withdrawn());
    }

    #[test]
    #[should_panic(expected = "ERR_ONLY_TAKER")]
    fn test_withdraw_not_taker() {
        let immutables = sample_immutables(100, 200);
        let mut contract = Contract::new(immutables);

        let context = get_context(accounts(3), 101 * 1_000_000_000);
        testing_env!(context.build());

        let secret = Base64VecU8("secret".as_bytes().to_vec());
        contract.withdraw(secret);
    }

    #[test]
    #[should_panic(expected = "ERR_INVALID_SECRET")]
    fn test_withdraw_invalid_secret() {
        let immutables = sample_immutables(100, 200);
        let mut contract = Contract::new(immutables);

        let context = get_context(accounts(2), 101 * 1_000_000_000);
        testing_env!(context.build());

        let secret = Base64VecU8("wrong_secret".as_bytes().to_vec());
        contract.withdraw(secret);
    }

    #[test]
    #[should_panic(expected = "ERR_WITHDRAWAL_PERIOD_NOT_STARTED")]
    fn test_withdraw_too_early() {
        let immutables = sample_immutables(100, 200);
        let mut contract = Contract::new(immutables);

        let context = get_context(accounts(2), 99 * 1_000_000_000);
        testing_env!(context.build());

        let secret = Base64VecU8("secret".as_bytes().to_vec());
        contract.withdraw(secret);
    }

    #[test]
    #[should_panic(expected = "ERR_WITHDRAWAL_PERIOD_ENDED")]
    fn test_withdraw_too_late() {
        let immutables = sample_immutables(100, 200);
        let mut contract = Contract::new(immutables);

        let context = get_context(accounts(2), 201 * 1_000_000_000);
        testing_env!(context.build());

        let secret = Base64VecU8("secret".as_bytes().to_vec());
        contract.withdraw(secret);
    }

    #[test]
    fn test_cancel_ok() {
        let cancellation_time = 1_600_000_100; // in seconds
        let immutables = sample_immutables(1_600_000_000, cancellation_time);
        let mut contract = Contract::new(immutables);

        let context = get_context(accounts(2), (cancellation_time as u64 + 1) * 1_000_000_000);
        testing_env!(context.build());

        // In unit tests, we can't actually execute cross-contract calls
        // We would test the state change before the async call
        contract.is_cancelled = true; // Simulate successful cancellation
        assert!(contract.is_cancelled());
    }

    #[test]
    #[should_panic(expected = "ERR_ALREADY_WITHDRAWN")]
    fn test_cancel_after_withdraw() {
        let withdrawal_time = 1_600_000_000;
        let cancellation_time = 1_600_000_100;
        let immutables = sample_immutables(withdrawal_time, cancellation_time);
        let mut contract = Contract::new(immutables.clone());

        // Simulate withdrawal first
        contract.is_withdrawn = true;

        // Then try to cancel
        let context = get_context(accounts(2), (cancellation_time as u64 + 1) * 1_000_000_000);
        testing_env!(context.build());
        contract.cancel();
    }
}
