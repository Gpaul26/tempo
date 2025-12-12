```rust
pub mod admin;
pub mod amm;
pub mod dex;
pub mod error;
pub mod eth_ext;
pub mod policy;
pub mod token;
pub use admin::{TempoAdminApi, TempoAdminApiServer};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::{Log, ReceiptWithBloom};
pub use amm::{TempoAmm, TempoAmmApiServer};
pub use dex::{TempoDex, api::TempoDexApiServer};
pub use eth_ext::{TempoEthExt, TempoEthExtApiServer};
use futures::{TryFutureExt, future::Either};
pub use policy::{TempoPolicy, TempoPolicyApiServer};
use reth_errors::RethError;
use reth_primitives_traits::{Recovered, TransactionMeta, WithEncoded, transaction::TxHashRef};
use reth_transaction_pool::PoolPooledTx;
use std::sync::Arc;
pub use tempo_alloy::rpc::TempoTransactionRequest;
use tempo_chainspec::{TempoChainSpec, hardfork::TempoHardfork};
use tempo_evm::TempoStateAccess;
use tempo_precompiles::{NONCE_PRECOMPILE_ADDRESS, nonce::NonceManager};
pub use token::{TempoToken, TempoTokenApiServer};
use crate::{node::TempoNode, rpc::error::TempoEthApiError};
use alloy::{
    consensus::TxReceipt,
    primitives::{U256, uint},
};
use reth_ethereum::tasks::{
    TaskSpawner,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};
use reth_evm::{
    EvmEnvFor, TxEnvFor,
    revm::{Database, context::result::EVMError},
};
use reth_node_api::{FullNodeComponents, FullNodeTypes, HeaderTy, PrimitivesTy};
use reth_node_builder::{
    NodeAdapter,
    rpc::{EthApiBuilder, EthApiCtx},
};
use reth_provider::{ChainSpecProvider, ProviderError};
use reth_rpc::{DynRpcConverter, eth::EthApi};
use reth_rpc_eth_api::{
    EthApiTypes, RpcConverter, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        Call, EthApiSpec, EthBlocks, EthCall, EthFees, EthState, EthTransactions, LoadBlock,
        LoadFee, LoadPendingBlock, LoadReceipt, LoadState, LoadTransaction, SpawnBlocking, Trace,
        estimate::EstimateCall, pending_block::PendingEnvBuilder, spec::SignersForRpc,
    },
};
use reth_rpc_eth_types::{
    EthApiError, EthStateCache, FeeHistoryCache, GasPriceOracle, PendingBlock,
    builder::config::PendingBlockKind, receipt::EthReceiptConverter,
};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_evm::TempoEvmConfig;
use tempo_primitives::{
    TEMPO_GAS_PRICE_SCALING_FACTOR, TempoPrimitives, TempoReceipt, TempoTxEnvelope,
};
use tokio::sync::{Mutex, broadcast};
use tokio::time::{sleep, Duration};
use tracing::log;

pub const NATIVE_BALANCE_PLACEHOLDER: U256 =
    uint!(4242424242424242424242424242424242424242424242424242424242424242424242424242_U256);

#[derive(Clone)]
pub struct TempoEthApi<N: FullNodeTypes<Types = TempoNode>> {
    inner: EthApi<NodeAdapter<N>, DynRpcConverter<TempoEvmConfig, TempoNetwork>>,
    subblock_transactions_tx: broadcast::Sender<Recovered<TempoTxEnvelope>>,
}
impl<N: FullNodeTypes<Types = TempoNode>> TempoEthApi<N> {
    pub fn new(
        eth_api: EthApi<NodeAdapter<N>, DynRpcConverter<TempoEvmConfig, TempoNetwork>>,
    ) -> Self {
        Self {
            inner: eth_api,
            subblock_transactions_tx: broadcast::channel(100).0,
        }
    }
    pub fn subblock_transactions_rx(&self) -> broadcast::Receiver<Recovered<TempoTxEnvelope>> {
        self.subblock_transactions_tx.subscribe()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> EthApiTypes for TempoEthApi<N> {
    type Error = TempoEthApiError;
    type NetworkTypes = TempoNetwork;
    type RpcConvert = DynRpcConverter<TempoEvmConfig, TempoNetwork>;
    fn converter(&self) -> &Self::RpcConvert {
        self.inner.converter()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> RpcNodeCore for TempoEthApi<N> {
    type Primitives = PrimitivesTy<N::Types>;
    type Provider = N::Provider;
    type Pool = <NodeAdapter<N> as FullNodeComponents>::Pool;
    type Evm = <NodeAdapter<N> as FullNodeComponents>::Evm;
    type Network = <NodeAdapter<N> as FullNodeComponents>::Network;
    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.pool()
    }
    #[inline]
    fn evm_config(&self) -> &Self::Evm {
        self.inner.evm_config()
    }
    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.network()
    }
    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.provider()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> RpcNodeCoreExt for TempoEthApi<N> {
    #[inline]
    fn cache(&self) -> &EthStateCache<PrimitivesTy<N::Types>> {
        self.inner.cache()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> EthApiSpec for TempoEthApi<N> {
    #[inline]
    fn starting_block(&self) -> U256 {
        self.inner.starting_block()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> SpawnBlocking for TempoEthApi<N> {
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.inner.task_spawner()
    }
    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.blocking_task_pool()
    }
    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.blocking_task_guard()
    }
    #[inline]
    fn blocking_io_task_guard(&self) -> &Arc<tokio::sync::Semaphore> {
        self.inner.blocking_io_task_guard()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> LoadPendingBlock for TempoEthApi<N> {
    #[inline]
    fn pending_block(&self) -> &Mutex<Option<PendingBlock<Self::Primitives>>> {
        self.inner.pending_block()
    }
    #[inline]
    fn pending_env_builder(&self) -> &dyn PendingEnvBuilder<Self::Evm> {
        self.inner.pending_env_builder()
    }
    #[inline]
    fn pending_block_kind(&self) -> PendingBlockKind {
        self.inner.pending_block_kind()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> LoadFee for TempoEthApi<N> {
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.gas_oracle()
    }
    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<HeaderTy<N::Types>> {
        self.inner.fee_history_cache()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> LoadState for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> EthState for TempoEthApi<N> {
    #[inline]
    async fn balance(
        &self,
        _address: alloy_primitives::Address,
        _block_id: Option<alloy_eips::BlockId>,
    ) -> Result<U256, Self::Error> {
        Ok(NATIVE_BALANCE_PLACEHOLDER)
    }
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_proof_window()
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> EthFees for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> Trace for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> EthCall for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> Call for TempoEthApi<N> {
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.gas_cap()
    }
    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.max_simulate_blocks()
    }
    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.evm_memory_limit()
    }
    fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        _evm_env: &EvmEnvFor<Self::Evm>,
        tx_env: &TxEnvFor<Self::Evm>,
    ) -> Result<u64, Self::Error> {
        let fee_payer = tx_env
            .fee_payer()
            .map_err(EVMError::<ProviderError, _>::from)?;
        let fee_token = db
            .get_fee_token(tx_env, Address::ZERO, fee_payer, TempoHardfork::default())
            .map_err(Into::into)?;
        let fee_token_balance = db
            .get_token_balance(fee_token, fee_payer)
            .map_err(Into::into)?;
        Ok(fee_token_balance
            .saturating_mul(TEMPO_GAS_PRICE_SCALING_FACTOR)
            .checked_div(U256::from(tx_env.inner.gas_price))
            .unwrap_or_default()
            .saturating_to())
    }
    fn create_txn_env(
        &self,
        evm_env: &EvmEnvFor<Self::Evm>,
        mut request: TempoTransactionRequest,
        mut db: impl Database<Error: Into<EthApiError>>,
    ) -> Result<TxEnvFor<Self::Evm>, Self::Error> {
        if let Some(nonce_key) = request.nonce_key
            && request.nonce.is_none()
            && !nonce_key.is_zero()
        {
            let slot = NonceManager::new()
                .nonces
                .at(request.from.unwrap_or_default())
                .at(nonce_key)
                .slot();
            request.nonce = Some(
                db.storage(NONCE_PRECOMPILE_ADDRESS, slot)
                    .map_err(Into::into)?
                    .saturating_to(),
            );
        }
        Ok(self.inner.create_txn_env(evm_env, request, db)?)
    }
}
impl<N: FullNodeTypes<Types = TempoNode>> EstimateCall for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> LoadBlock for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> LoadReceipt for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> EthBlocks for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> LoadTransaction for TempoEthApi<N> {}
impl<N: FullNodeTypes<Types = TempoNode>> EthTransactions for TempoEthApi<N> {
    fn signers(&self) -> &SignersForRpc<Self::Provider, Self::NetworkTypes> {
        self.inner.signers()
    }
    fn send_raw_transaction_sync_timeout(&self) -> std::time::Duration {
        self.inner.send_raw_transaction_sync_timeout()
    }
    fn send_transaction(
        &self,
        tx: WithEncoded<Recovered<PoolPooledTx<Self::Pool>>>,
    ) -> impl Future<Output = Result<B256, Self::Error>> + Send {
        let tx_hash = *tx.value().tx_hash();
        let mut attempts = 0;
        let cloned_self = self.clone();
        let cloned_tx = tx.clone();
        async move {
            while attempts < 5 {
                let result = if cloned_tx.value().inner().subblock_proposer().is_some() {
                    cloned_self.subblock_transactions_tx.send(cloned_tx.clone().into_value()).map(|_| tx_hash).map_err(|e| {
                        EthApiError::from(RethError::msg(format!("subblocks service channel error: {}", e)))
                    })
                } else {
                    cloned_self.inner.send_transaction(cloned_tx.clone()).await.map_err(Into::into)
                };
                match result {
                    Ok(hash) => return Ok(hash),
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 5 {
                            return Err(e);
                        }
                        sleep(Duration::from_millis(100 * 2u64.pow(attempts as u32))).await;
                        log::warn!("Retry forwarding tx {}: {}", tx_hash, e);
                    }
                }
            }
            Err(EthApiError::from(RethError::msg("Failed to forward tx after retries")))
        }
    }
}
#[derive(Debug, Clone)]
#[expect(clippy::type_complexity)]
pub struct TempoReceiptConverter {
    inner: EthReceiptConverter<
        TempoChainSpec,
        fn(TempoReceipt, usize, TransactionMeta) -> ReceiptWithBloom<TempoReceipt<Log>>,
    >,
}
impl TempoReceiptConverter {
    pub fn new(chain_spec: Arc<TempoChainSpec>) -> Self {
        Self {
            inner: EthReceiptConverter::new(chain_spec).with_builder(
                |receipt: TempoReceipt, next_log_index, meta| {
                    receipt.into_rpc(next_log_index, meta).into_with_bloom()
                },
            ),
        }
    }
}
impl ReceiptConverter<TempoPrimitives> for TempoReceiptConverter {
    type RpcReceipt = TempoTransactionReceipt;
    type Error = EthApiError;
    fn convert_receipts(
        &self,
        receipts: Vec<ConvertReceiptInput<'_, TempoPrimitives>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        let txs = receipts.iter().map(|r| r.tx).collect::<Vec<_>>();
        self.inner
            .convert_receipts(receipts)?
            .into_iter()
            .zip(txs)
            .map(|(inner, tx)| {
                let mut receipt = TempoTransactionReceipt {
                    inner,
                    fee_token: None,
                    fee_payer: tx
                        .fee_payer(tx.signer())
                        .map_err(|_| EthApiError::InvalidTransactionSignature)?,
                };
                if receipt.effective_gas_price == 0 || receipt.gas_used == 0 {
                    return Ok(receipt);
                }
                receipt.fee_token = receipt.logs().last().map(|log| log.address());
                Ok(receipt)
            })
            .collect()
    }
}
#[derive(Debug, Default)]
pub struct TempoEthApiBuilder;
impl<N> EthApiBuilder<NodeAdapter<N>> for TempoEthApiBuilder
where
    N: FullNodeTypes<Types = TempoNode>,
{
    type EthApi = TempoEthApi<N>;
    async fn build_eth_api(self, ctx: EthApiCtx<'_, NodeAdapter<N>>) -> eyre::Result<Self::EthApi> {
        let chain_spec = ctx.components.provider.chain_spec();
        let eth_api = ctx
            .eth_api_builder()
            .modify_gas_oracle_config(|config| config.default_suggested_fee = Some(U256::ZERO))
            .map_converter(|_| RpcConverter::new(TempoReceiptConverter::new(chain_spec)).erased())
            .build();
        Ok(TempoEthApi::new(eth_api))
    }
}
```
