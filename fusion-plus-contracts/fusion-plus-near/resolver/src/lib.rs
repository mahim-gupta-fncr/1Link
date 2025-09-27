use near_sdk::serde::Serialize;
use near_sdk::serde_json;
use near_sdk::{env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError};
use shared_fusion_data::Immutables;
use near_sdk::json_types::Base64VecU8;
/// Gas attached to the initialization call of the dst escrow contract (50 Tgas).
const GAS_FOR_INIT: Gas = Gas::from_tgas(50);
/// Gas attached to the fungible token transfer call (30 Tgas).
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(30);
/// Cost of 1 byte of storage on NEAR (≈0.01 Ⓝ per KB).
const NEAR_PER_STORAGE: NearToken = NearToken::from_yoctonear(10u128.pow(19));

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct ResolverContract {
    owner: AccountId,
    code: Option<Vec<u8>>,     // Stored escrow contract code
    src_code: Option<Vec<u8>>, // Stored source escrow contract code
}

/// NEP-297 events for the resolver contract.
#[derive(Serialize)]
#[serde(crate = "near_sdk::serde", tag = "event", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum ResolverEvent<'a> {
    DeployDst {
        dst_account_id: &'a AccountId,
    },
    DstEscrowCreated {
        escrow: &'a AccountId,
        hashlock: &'a Vec<u8>,
        taker: &'a String,
    },
    SrcEscrowCreated {
        escrow: &'a AccountId,
        hashlock: &'a Vec<u8>,
        maker: &'a String,
        taker: &'a String,
    },
    OwnerChanged {
        new_owner: &'a AccountId,
    },
}

#[near]
impl ResolverContract {
    #[init]
    pub fn new(owner: AccountId) -> Self {
        assert!(!env::state_exists(), "ERR_ALREADY_INITIALIZED");
        Self {
            owner,
            code: None,
            src_code: None,
        }
    }

    /// Updates the stored escrow contract code. Only owner can call.
    /// The new contract code should be passed as raw bytes in the transaction input.
    #[private]
    pub fn update_stored_contract(&mut self, data: Base64VecU8) {
        self.assert_owner();
        let wasm_bytes: Vec<u8> = data.0;
        self.code = Some(wasm_bytes);

        env::log_str("Escrow contract code updated successfully");
    }

    /// Updates the stored source escrow contract code. Only owner can call.
    /// The new contract code should be passed as raw bytes in the transaction input.
    #[private]
    pub fn update_stored_src_contract(&mut self, data: Base64VecU8) {
        self.assert_owner();
        let bytes: Vec<u8> = data.0;
        self.src_code = Some(bytes);
        env::log_str("Source escrow contract code updated successfully");
    }
    /// Deploys the stored escrow contract to a deterministic address based on the hashlock.
    /// The escrow account ID will be: {hashlock_prefix}.{resolver_account_id}
    ///
    /// * The caller must attach enough deposit to cover storage staking for the new account.
    ///   The entire attached deposit will be forwarded to the new account's balance.
    /// * For fungible token escrows, the caller must first transfer the required tokens to this
    ///   resolver contract before calling deploy_dst. The resolver will then transfer these tokens
    ///   to the newly created escrow contract.
    #[payable]
    pub fn deploy_dst(&mut self, immutables: Immutables) -> Promise {
        self.assert_owner();

        // Get the stored contract code
        let code = self
            .code
            .as_ref()
            .expect("No escrow contract stored. Call update_stored_contract first.");

        // Compute deterministic account ID from hashlock
        // Use base58 encoding of the entire hashlock for NEAR account naming
        // Use only the first 32 characters of the base58-encoded hashlock to avoid account name too long
        let hashlock_str = near_sdk::bs58::encode(&immutables.hashlock)
            .into_string()
            .to_lowercase();
        let hashlock_prefix = &hashlock_str[..std::cmp::min(32, hashlock_str.len())];
        let dst_account_id: AccountId =
            format!("{}.{}", hashlock_prefix, env::current_account_id())
                .parse()
                .expect("Invalid account ID format");

        let attached: NearToken = env::attached_deposit();
        // Ensure the caller attached enough deposit to cover contract storage plus a small buffer.
        let contract_bytes = code.len() as u128;
        let contract_storage_cost = NEAR_PER_STORAGE.saturating_mul(contract_bytes);
        let minimum_needed = contract_storage_cost.saturating_add(NearToken::from_millinear(100)); // +0.1 Ⓝ safety buffer
        assert!(
            attached >= minimum_needed,
            "ERR_DEPOSIT_TOO_LOW: attach at least {minimum_needed} yoctoNEAR"
        );

        // Create the new account, deploy the contract code and call its `new` initializer.
        let mut promise = Promise::new(dst_account_id.clone())
            .create_account()
            .transfer(attached)
            .deploy_contract(code.clone())
            .function_call(
                "new".to_string(),
                near_sdk::serde_json::json!({ "immutables": immutables })
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_FOR_INIT,
            );

        // If there's a token specified (not NEAR/empty), transfer the tokens to the escrow contract
        // Note: The resolver must already have these tokens (transferred by the caller beforehand)
        if !immutables.token.is_empty() {
            let token_account_id: AccountId =
                immutables.token.parse().expect("Invalid token account ID");

            // Transfer tokens from this resolver contract to the newly created escrow
            promise = promise.then(
                Promise::new(token_account_id).function_call(
                    "ft_transfer".to_string(),
                    near_sdk::serde_json::json!({
                        "receiver_id": dst_account_id.to_string(),
                        "amount": immutables.amount.to_string(),
                    })
                    .to_string()
                    .into_bytes(),
                    NearToken::from_yoctonear(1), // 1 yoctoNEAR required for ft_transfer
                    GAS_FOR_FT_TRANSFER,
                ),
            );
        }

        // Add callback to refund funds in case of failure.
        promise = promise.then(Self::ext(env::current_account_id()).deploy_dst_callback(
            dst_account_id.clone(),
            env::predecessor_account_id(),
            attached,
            immutables.token.clone(),
            immutables.amount,
        ));

        // Log event.
        let event = ResolverEvent::DstEscrowCreated {
            escrow: &dst_account_id,
            hashlock: &immutables.hashlock,
            taker: &immutables.taker,
        };
        let event_json = serde_json::to_string(&event).unwrap();
        env::log_str(&format!("EVENT_JSON:{}", event_json));

        promise
    }

    /// Deploys a source escrow contract for NEAR -> EVM swaps.
    /// The escrow account ID will be: src-{hashlock_prefix}.{resolver_account_id}
    ///
    /// * The caller must attach enough deposit to cover storage staking for the new account.
    /// * After deployment, the maker signs a delegate action (SDA) transferring
    /// *   the tokens via `ft_transfer_call` directly to this escrow. The resolver
    /// *   submits that SDA and pays the gas.
    #[payable]
    pub fn deploy_src(&mut self, immutables: Immutables) -> Promise {
        self.assert_owner();

        // Get the stored source contract code
        let code = self
            .src_code
            .as_ref()
            .expect("No source escrow contract stored. Call update_stored_src_contract first.");

        let hashlock_str = near_sdk::bs58::encode(&immutables.hashlock)
            .into_string()
            .to_lowercase();
        let hashlock_prefix = &hashlock_str[..std::cmp::min(32, hashlock_str.len())];
        let src_account_id: AccountId =
            format!("{}.{}", hashlock_prefix, env::current_account_id())
                .parse()
                .expect("Invalid account ID format");

        let attached: NearToken = env::attached_deposit();
        // Ensure the caller attached enough deposit to cover contract storage plus a small buffer.
        let contract_bytes = code.len() as u128;
        let contract_storage_cost = NEAR_PER_STORAGE.saturating_mul(contract_bytes);
        let minimum_needed = contract_storage_cost.saturating_add(NearToken::from_millinear(100)); // +0.1 Ⓝ safety buffer
        assert!(
            attached >= minimum_needed,
            "ERR_DEPOSIT_TOO_LOW: attach at least {minimum_needed} yoctoNEAR"
        );

        // Create the new account, deploy the contract code and call its `new` initializer.
        let mut promise = Promise::new(src_account_id.clone())
            .create_account()
            .transfer(attached)
            .deploy_contract(code.clone())
            .function_call(
                "new".to_string(),
                near_sdk::serde_json::json!({ "immutables": immutables })
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_FOR_INIT,
            );

        // Add callback to handle any failures
        promise = promise.then(Self::ext(env::current_account_id()).deploy_src_callback(
            src_account_id.clone(),
            env::predecessor_account_id(),
            attached,
        ));

        // Log event.
        let event = ResolverEvent::SrcEscrowCreated {
            escrow: &src_account_id,
            hashlock: &immutables.hashlock,
            maker: &immutables.maker,
            taker: &immutables.taker,
        };
        let event_json = serde_json::to_string(&event).unwrap();
        env::log_str(&format!("EVENT_JSON:{}", event_json));

        promise
    }

    /// Handles meta-transaction deployment of source escrow with user's signed delegate action.
    /// This allows gasless token transfer from maker to escrow.
    #[payable]
    pub fn deploy_src_with_delegate(&mut self, immutables: Immutables) -> Promise {
        // This method would handle the signed delegate action
        // For hackathon purposes, we'll use the simpler deploy_src method above
        // In production, this would parse and execute the signed delegate action
        self.deploy_src(immutables)
    }

    // --- Callback ---
    #[private]
    pub fn deploy_dst_callback(
        &mut self,
        dst_account_id: AccountId,
        caller: AccountId,
        attached: NearToken,
        token: String,
        _amount: u128,
        #[callback_result] create_deploy_result: Result<(), PromiseError>,
    ) -> bool {
        if create_deploy_result.is_ok() {
            env::log_str(&format!("Successfully deployed escrow to {dst_account_id}"));
            true
        } else {
            env::log_str(&format!(
                "Error deploying {dst_account_id}, refunding {} yoctoNEAR to {caller}",
                attached.as_yoctonear()
            ));
            Promise::new(caller).transfer(attached);

            // If token transfer failed and tokens were involved, log it
            // Note: We cannot refund tokens here as we don't have a generic way to handle all token types
            if !token.is_empty() {
                env::log_str(&format!(
                    "Token transfer may have failed. Please check {} tokens balance for resolver and escrow.",
                    token
                ));
            }

            false
        }
    }

    // --- Callback for source escrow deployment ---
    #[private]
    pub fn deploy_src_callback(
        &mut self,
        src_account_id: AccountId,
        caller: AccountId,
        attached: NearToken,
        #[callback_result] create_deploy_result: Result<(), PromiseError>,
    ) -> bool {
        if create_deploy_result.is_ok() {
            env::log_str(&format!(
                "Successfully deployed source escrow to {src_account_id}"
            ));

            // Note: In a full implementation, after successful deployment,
            // we would trigger the ft_transfer_call from maker to escrow here
            // using the signed delegate action. For the hackathon, the maker
            // needs to manually call ft_transfer_call to fund the escrow.

            true
        } else {
            env::log_str(&format!(
                "Error deploying {src_account_id}, refunding {} yoctoNEAR to {caller}",
                attached.as_yoctonear()
            ));
            Promise::new(caller).transfer(attached);
            false
        }
    }

    /// Transfers ownership to `new_owner`.
    pub fn change_owner(&mut self, new_owner: AccountId) {
        self.assert_owner();
        self.owner = new_owner.clone();
        let event = ResolverEvent::OwnerChanged {
            new_owner: &new_owner,
        };
        let event_json = serde_json::to_string(&event).unwrap();
        env::log_str(&format!("EVENT_JSON:{}", event_json));
    }

    // --- View methods ---

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    /// Computes the deterministic escrow address for a given hashlock.
    /// This allows users to know the escrow address before deployment.
    pub fn get_escrow_address(&self, hashlock: Vec<u8>) -> String {
        let hashlock_str = near_sdk::bs58::encode(&hashlock)
            .into_string()
            .to_lowercase();
        let hashlock_prefix = &hashlock_str[..std::cmp::min(32, hashlock_str.len())];
        format!("{}.{}", hashlock_prefix, env::current_account_id())
    }

    /// Computes the deterministic source escrow address for a given hashlock.
    /// This allows users to know the source escrow address before deployment.
    pub fn get_src_escrow_address(&self, hashlock: Vec<u8>) -> String {
        let hashlock_str = near_sdk::bs58::encode(&hashlock)
            .into_string()
            .to_lowercase();
        let hashlock_prefix = &hashlock_str[..std::cmp::min(28, hashlock_str.len())];
        format!("src-{}.{}", hashlock_prefix, env::current_account_id())
    }

    // --- Internal helpers ---

    fn assert_owner(&self) {
        assert_eq!(env::predecessor_account_id(), self.owner, "ERR_NOT_OWNER");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;
    use near_sdk::NearToken as NT;
    use shared_fusion_data::Timelocks;

    fn get_context(predecessor: AccountId, deposit: u128) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .signer_account_id(predecessor.clone())
            .predecessor_account_id(predecessor)
            .attached_deposit(NT::from_yoctonear(deposit));
        builder
    }

    #[test]
    fn test_change_owner() {
        let mut resolver = ResolverContract::new(accounts(0));
        let context = get_context(accounts(0), 0);
        testing_env!(context.build());
        resolver.change_owner(accounts(1));
        assert_eq!(resolver.get_owner(), accounts(1));
    }

    #[test]
    #[should_panic(expected = "ERR_NOT_OWNER")]
    fn test_change_owner_not_owner() {
        let mut resolver = ResolverContract::new(accounts(0));
        let context = get_context(accounts(1), 0);
        testing_env!(context.build());
        resolver.change_owner(accounts(2));
    }

    #[test]
    fn test_deploy_dst_logs_event() {
        let mut resolver = ResolverContract::new(accounts(0));
        // Store some dummy contract code first
        resolver.code = Some(vec![1, 2, 3, 4]); // Simulate stored contract

        let mut context = get_context(accounts(0), 1_000_000_000_000_000_000_000_000);
        context.block_timestamp(0);
        testing_env!(context.build());

        let immutables = Immutables {
            order_hash: vec![0; 32],
            hashlock: vec![0; 32],
            maker: accounts(1).to_string(),
            taker: accounts(2).to_string(),
            token: accounts(3).to_string(),
            amount: 1000,
            safety_deposit: 1000,
            timelocks: Timelocks {
                src_chain_finality: 10,
                src_withdrawal: 20,
                src_cancellation: 30,
                src_public_withdrawal: 40,
                src_public_cancellation: 45,
                dst_chain_id: 137u128,
                dst_withdrawal: 50,
                dst_cancellation: 60,
                dst_public_withdrawal: 70,
            },
        };

        // The promise returned is not executed in unit tests, but we can ensure it is created.
        resolver.deploy_dst(immutables);
    }

    #[test]
    fn test_address() {
        // This is the hashlock for secret "0x29de92d04dd2aea6da0e9595ed8b628ae755d60e200e90299459757ee50b3e7b"
        let hashlock_array: [u8; 32] = [
            41, 222, 146, 208, 77, 210, 174, 166, 218, 14, 149, 149, 237, 139, 98, 138, 231, 85,
            214, 14, 32, 14, 144, 41, 148, 89, 117, 126, 229, 11, 62, 123,
        ];
        let hashlock_str = near_sdk::bs58::encode(&hashlock_array)
            .into_string()
            .to_lowercase();
        let hashlock_prefix = &hashlock_str[..std::cmp::min(32, hashlock_str.len())];
        let dst_account_id: AccountId =
            format!("{}.{}", hashlock_prefix, env::current_account_id())
                .parse()
                .expect("Invalid account ID format");
        println!("dst_account_id: {:?}", dst_account_id);
    }
}
