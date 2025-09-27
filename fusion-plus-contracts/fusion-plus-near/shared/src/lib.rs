use borsh::BorshSchema;
use near_sdk::{
    borsh::{self, BorshDeserialize, BorshSerialize},
    serde::{Deserialize, Serialize},
};
use schemars::JsonSchema;

// Based on TimelocksLib.sol
#[derive(
    BorshDeserialize, BorshSerialize, BorshSchema, Serialize, Deserialize, JsonSchema, Clone,
)]
#[serde(crate = "near_sdk::serde")]
pub struct Timelocks {
    pub src_chain_finality: u32,
    pub src_withdrawal: u32,
    pub src_cancellation: u32,
    pub src_public_withdrawal: u32,
    pub src_public_cancellation: u32,
    pub dst_chain_id: u128,
    pub dst_withdrawal: u32,
    pub dst_cancellation: u32,
    pub dst_public_withdrawal: u32,
}

// Based on IBaseEscrow.sol Immutables
#[derive(
    BorshDeserialize, BorshSerialize, BorshSchema, Serialize, Deserialize, JsonSchema, Clone,
)]
#[serde(crate = "near_sdk::serde")]
pub struct Immutables {
    pub order_hash: Vec<u8>,
    pub hashlock: Vec<u8>,
    pub maker: String,
    pub taker: String,
    pub token: String,
    pub amount: u128,
    pub safety_deposit: u128,
    pub timelocks: Timelocks,
}
