// NEAR Source Escrow contract for cross-chain atomic swap (NEAR → EVM)
use near_sdk::json_types::Base64VecU8;
use near_sdk::serde::Serialize;
use near_sdk::{env, log, near, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError};
use shared_fusion_data::Immutables;

// Gas constants for cross-contract calls
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(30);
const GAS_FOR_RESOLVE_TRANSFER: Gas = Gas::from_tgas(5);

#[cfg_attr(feature = "contract", near(contract_state))]
#[derive(PanicOnDefault)]
pub struct Contract {
    immutables: Immutables,
    is_withdrawn: bool,
    is_cancelled: bool,
    is_funded: bool,
    deployed_at: u32, // Timestamp when escrow was deployed
}

/// NEP-297 Events
#[derive(Serialize)]
#[serde(crate = "near_sdk::serde", tag = "event", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum Event<'a> {
    SrcEscrowCreated {
        escrow: &'a AccountId,
        hashlock: &'a Vec<u8>,
        maker: &'a String,
        taker: &'a String,
        amount: u128,
    },
    Withdrawal {
        secret: &'a Base64VecU8,
    },
    EscrowCancelled,
    EscrowFunded {
        sender: &'a AccountId,
        amount: u128,
    },
}

// The actual contract methods are only compiled when the `contract` feature is enabled.
#[cfg(feature = "contract")]
#[near]
impl Contract {
    #[init]
    pub fn new(immutables: Immutables) -> Self {
        let deployed_at = (env::block_timestamp() / 1_000_000_000) as u32;

        // Log the escrow creation event
        let event = Event::SrcEscrowCreated {
            escrow: &env::current_account_id(),
            hashlock: &immutables.hashlock,
            maker: &immutables.maker,
            taker: &immutables.taker,
            amount: immutables.amount,
        };
        let event_json = serde_json::to_string(&event).unwrap();
        log!("EVENT_JSON:{}", event_json);

        Self {
            immutables,
            is_withdrawn: false,
            is_cancelled: false,
            is_funded: false,
            deployed_at,
        }
    }

    /// Called by the resolver to receive tokens using ft_transfer_call
    /// This implements NEP-141 ft_on_transfer
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: String) -> String {
        // Verify that the transfer is from the expected maker
        assert_eq!(
            sender_id.to_string(),
            self.immutables.maker,
            "ERR_INVALID_SENDER: expected maker"
        );

        // Verify the amount matches
        let amount_u128: u128 = amount.parse().expect("ERR_INVALID_AMOUNT");
        assert_eq!(amount_u128, self.immutables.amount, "ERR_INCORRECT_AMOUNT");

        // Verify token contract
        assert_eq!(
            env::predecessor_account_id().to_string(),
            self.immutables.token,
            "ERR_INVALID_TOKEN"
        );

        // Ensure we haven't already been funded
        assert!(!self.is_funded, "ERR_ALREADY_FUNDED");
        self.is_funded = true;

        // Emit funding event
        let fund_event = Event::EscrowFunded {
            sender: &sender_id,
            amount: amount_u128,
        };
        let fund_json = serde_json::to_string(&fund_event).unwrap();
        log!("EVENT_JSON:{}", fund_json);

        // Return "0" to indicate we're keeping all tokens
        "0".to_string()
    }

    /// Withdraw funds by providing the secret (called by taker/resolver)
    pub fn withdraw(&mut self, secret: Base64VecU8) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();
        self.assert_taker();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        let withdrawal_start = self.deployed_at + self.immutables.timelocks.src_withdrawal;
        // let cancellation_start = self.deployed_at + self.immutables.timelocks.src_cancellation;

        assert!(
            current_timestamp_sec >= withdrawal_start,
            "ERR_WITHDRAWAL_PERIOD_NOT_STARTED"
        );

        // Verify the secret
        let hashed_secret = env::keccak256(&secret.0);
        assert_eq!(
            self.immutables.hashlock.as_slice(),
            hashed_secret.as_slice(),
            "ERR_INVALID_SECRET"
        );

        // Mark as withdrawn before making transfers to prevent reentrancy
        self.is_withdrawn = true;

        // Return safety deposit to taker (caller)
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer fungible tokens to taker
        let ft_transfer_promise = Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": self.immutables.taker,
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

    /// Withdraw to a specific target address (called by taker/resolver)
    pub fn withdraw_to(&mut self, secret: Base64VecU8, target: AccountId) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();
        self.assert_taker();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        let withdrawal_start = self.deployed_at + self.immutables.timelocks.src_withdrawal;
        let cancellation_start = self.deployed_at + self.immutables.timelocks.src_cancellation;

        assert!(
            current_timestamp_sec >= withdrawal_start,
            "ERR_WITHDRAWAL_PERIOD_NOT_STARTED"
        );
        assert!(
            current_timestamp_sec < cancellation_start,
            "ERR_WITHDRAWAL_PERIOD_ENDED"
        );

        // Verify the secret
        let hashed_secret = env::keccak256(&secret.0);
        assert_eq!(
            self.immutables.hashlock.as_slice(),
            hashed_secret.as_slice(),
            "ERR_INVALID_SECRET"
        );

        // Mark as withdrawn before making transfers to prevent reentrancy
        self.is_withdrawn = true;

        // Return safety deposit to caller
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer fungible tokens to target
        let ft_transfer_promise = Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": target.to_string(),
                    "amount": self.immutables.amount.to_string(),
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_FOR_FT_TRANSFER,
            );

        // Log event
        let event_json = serde_json::to_string(&Event::Withdrawal { secret: &secret }).unwrap();
        log!("EVENT_JSON:{}", event_json);

        // Chain promises
        ft_transfer_promise.then(safety_deposit_promise).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_RESOLVE_TRANSFER)
                .on_withdraw_complete(),
        )
    }

    /// Public withdrawal after the public withdrawal period starts
    pub fn public_withdraw(&mut self, secret: Base64VecU8) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        let public_withdrawal_start =
            self.deployed_at + self.immutables.timelocks.src_public_withdrawal;
        let cancellation_start = self.deployed_at + self.immutables.timelocks.src_cancellation;

        assert!(
            current_timestamp_sec >= public_withdrawal_start,
            "ERR_PUBLIC_WITHDRAWAL_NOT_STARTED"
        );
        assert!(
            current_timestamp_sec < cancellation_start,
            "ERR_WITHDRAWAL_PERIOD_ENDED"
        );

        // Verify the secret
        let hashed_secret = env::keccak256(&secret.0);
        assert_eq!(
            self.immutables.hashlock.as_slice(),
            hashed_secret.as_slice(),
            "ERR_INVALID_SECRET"
        );

        // Mark as withdrawn
        self.is_withdrawn = true;

        // In public withdrawal, safety deposit goes to the withdrawer (could be anyone)
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer tokens to the taker
        let ft_transfer_promise = Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": self.immutables.taker,
                    "amount": self.immutables.amount.to_string(),
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_FOR_FT_TRANSFER,
            );

        // Log event
        let event_json = serde_json::to_string(&Event::Withdrawal { secret: &secret }).unwrap();
        log!("EVENT_JSON:{}", event_json);

        // Chain promises
        ft_transfer_promise.then(safety_deposit_promise).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_RESOLVE_TRANSFER)
                .on_withdraw_complete(),
        )
    }

    /// Cancel escrow and return funds to maker (called by taker)
    pub fn cancel(&mut self) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();
        self.assert_taker();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        let cancellation_start = self.deployed_at + self.immutables.timelocks.src_cancellation;

        assert!(
            current_timestamp_sec >= cancellation_start,
            "ERR_CANCELLATION_PERIOD_NOT_STARTED"
        );

        // Mark as cancelled
        self.is_cancelled = true;

        // Return safety deposit to taker
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer fungible tokens back to maker
        let ft_transfer_promise = Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": self.immutables.maker,
                    "amount": self.immutables.amount.to_string(),
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_FOR_FT_TRANSFER,
            );

        // Log event
        let event_json = serde_json::to_string(&Event::EscrowCancelled).unwrap();
        log!("EVENT_JSON:{}", event_json);

        // Chain promises
        ft_transfer_promise.then(safety_deposit_promise).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_RESOLVE_TRANSFER)
                .on_cancel_complete(),
        )
    }

    /// Public cancel after the public cancellation period
    pub fn public_cancel(&mut self) -> Promise {
        self.assert_not_withdrawn();
        self.assert_not_cancelled();

        let current_timestamp_sec = (env::block_timestamp() / 1_000_000_000) as u32;
        let public_cancellation_start =
            self.deployed_at + self.immutables.timelocks.src_public_cancellation;

        assert!(
            current_timestamp_sec >= public_cancellation_start,
            "ERR_PUBLIC_CANCELLATION_NOT_STARTED"
        );

        // Mark as cancelled
        self.is_cancelled = true;

        // In public cancellation, safety deposit goes to the caller (anyone)
        let safety_deposit_promise = Promise::new(env::predecessor_account_id())
            .transfer(NearToken::from_yoctonear(self.immutables.safety_deposit));

        // Transfer tokens back to maker
        let ft_transfer_promise = Promise::new(self.immutables.token.parse().unwrap())
            .function_call(
                "ft_transfer".to_string(),
                near_sdk::serde_json::json!({
                    "receiver_id": self.immutables.maker,
                    "amount": self.immutables.amount.to_string(),
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_FOR_FT_TRANSFER,
            );

        // Log event
        let event_json = serde_json::to_string(&Event::EscrowCancelled).unwrap();
        log!("EVENT_JSON:{}", event_json);

        // Chain promises
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

    pub fn get_deployed_at(&self) -> u32 {
        self.deployed_at
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
            env::predecessor_account_id().to_string(),
            self.immutables.taker,
            "ERR_ONLY_TAKER"
        );
    }
}
