use crate::{
    traces::TraceKind,
    tx::{CastTxBuilder, SenderKind},
    Cast,
};
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use clap::Parser;
use eyre::Result;
use foundry_cli::{
    opts::{EthereumOpts, TransactionOpts},
    utils::{self, handle_traces, parse_ether_value, TraceResult, get_state_overrides},
};
use foundry_common::{ens::NameOrAddress, shell};
use foundry_compilers::artifacts::EvmVersion;
use foundry_config::{
    figment::{
        self,
        value::{Dict, Map},
        Figment, Metadata, Profile,
    },
    Config,
};
use foundry_evm::{
    executors::TracingExecutor,
    opts::EvmOpts,
    traces::{InternalTraceMode, TraceMode},
};
use std::str::FromStr;

/// CLI arguments for `cast call`.
///
/// ## State Override Flags
///
/// The following flags can be used to override the state for the call:
///
/// * `--override-balance <address>:<balance>` - Override the balance of an account
/// * `--override-nonce <address>:<nonce>` - Override the nonce of an account
/// * `--override-code <address>:<code>` - Override the code of an account
/// * `--override-state <address>:<slot>:<value>` - Override a storage slot of an account
///
/// Multiple overrides can be specified for the same account. For example:
///
/// ```bash
/// cast call 0x... "transfer(address,uint256)" 0x... 100 \
///   --override-balance 0x123:0x1234 \
///   --override-nonce 0x123:1 \
///   --override-code 0x123:0x1234 \
///   --override-state 0x123:0x1:0x1234
/// ```
#[derive(Debug, Parser)]
pub struct CallArgs {
    /// The destination of the transaction.
    #[arg(value_parser = NameOrAddress::from_str)]
    to: Option<NameOrAddress>,

    /// The signature of the function to call.
    #[arg(value_name = "SIG")]
    sig: Option<String>,

    /// The arguments of the function to call.
    #[arg(value_name = "ARGS")]
    args: Vec<String>,

    /// Raw hex-encoded data for the transaction. Used instead of \[SIG\] and \[ARGS\].
    #[arg(
        long,
        conflicts_with_all = &["sig", "args"]
    )]
    data: Option<String>,

    /// Forks the remote rpc, executes the transaction locally and prints a trace
    #[arg(long, default_value_t = false)]
    trace: bool,

    /// Opens an interactive debugger.
    /// Can only be used with `--trace`.
    #[arg(long, requires = "trace")]
    debug: bool,

    #[arg(long, requires = "trace")]
    decode_internal: bool,

    /// Labels to apply to the traces; format: `address:label`.
    /// Can only be used with `--trace`.
    #[arg(long, requires = "trace")]
    labels: Vec<String>,

    /// The EVM Version to use.
    /// Can only be used with `--trace`.
    #[arg(long, requires = "trace")]
    evm_version: Option<EvmVersion>,

    /// The block height to query at.
    ///
    /// Can also be the tags earliest, finalized, safe, latest, or pending.
    #[arg(long, short, value_name = "BLOCK")]
    block: Option<BlockId>,

    /// Enable Odyssey features.
    #[arg(long, alias = "alphanet")]
    pub odyssey: bool,

    #[command(subcommand)]
    command: Option<CallSubcommands>,

    #[command(flatten)]
    tx: TransactionOpts,

    #[command(flatten)]
    eth: EthereumOpts,

    /// Use current project artifacts for trace decoding.
    #[arg(long, visible_alias = "la")]
    pub with_local_artifacts: bool,

    /// Override the balance of an account.
    /// Format: address:balance
    #[arg(long = "override-balance", value_name = "ADDRESS:BALANCE")]
    pub balance_overrides: Vec<String>,

    /// Override the nonce of an account.
    /// Format: address:nonce
    #[arg(long = "override-nonce", value_name = "ADDRESS:NONCE")]
    pub nonce_overrides: Vec<String>,

    /// Override the code of an account.
    /// Format: address:code
    #[arg(long = "override-code", value_name = "ADDRESS:CODE")]
    pub code_overrides: Vec<String>,

    /// Override the state of an account.
    /// Format: address:slot:value
    #[arg(long = "override-state", value_name = "ADDRESS:SLOT:VALUE")]
    pub state_overrides: Vec<String>,

    /// Override the state diff of an account.
    /// Format: address:slot:value
    #[arg(long = "override-state-diff", value_name = "ADDRESS:SLOT:VALUE")]
    pub state_diff_overrides: Vec<String>,
}

#[derive(Debug, Parser)]
pub enum CallSubcommands {
    /// ignores the address field and simulates creating a contract
    #[command(name = "--create")]
    Create {
        /// Bytecode of contract.
        code: String,

        /// The signature of the constructor.
        sig: Option<String>,

        /// The arguments of the constructor.
        args: Vec<String>,

        /// Ether to send in the transaction.
        ///
        /// Either specified in wei, or as a string with a unit type.
        ///
        /// Examples: 1ether, 10gwei, 0.01ether
        #[arg(long, value_parser = parse_ether_value)]
        value: Option<U256>,
    },
}

impl CallArgs {
    pub async fn run(self) -> Result<()> {
        let figment = Into::<Figment>::into(&self.eth).merge(&self);
        let evm_opts = figment.extract::<EvmOpts>()?;
        let mut config = Config::from_provider(figment)?.sanitized();

        // Store state_diff_overrides in a local variable to avoid partial move
        let state_diff_overrides = self.state_diff_overrides.clone();
        
        let Self {
            to,
            mut sig,
            mut args,
            mut tx,
            eth,
            command,
            block,
            trace,
            evm_version,
            debug,
            decode_internal,
            labels,
            ref data,
            with_local_artifacts,
            balance_overrides,
            nonce_overrides,
            code_overrides,
            state_overrides,
            ..
        } = self;

        if let Some(data) = data {
            sig = Some(data.clone());
        }

        let provider = utils::get_provider(&config)?;
        let sender = SenderKind::from_wallet_opts(eth.wallet).await?;
        let from = sender.address();

        let code = if let Some(CallSubcommands::Create {
            code,
            sig: create_sig,
            args: create_args,
            value,
        }) = command
        {
            sig = create_sig;
            args = create_args;
            if let Some(value) = value {
                tx.value = Some(value);
            }
            Some(code)
        } else {
            None
        };

        let (tx, func) = CastTxBuilder::new(&provider, tx, &config)
            .await?
            .with_to(to)
            .await?
            .with_code_sig_and_args(code, sig, args)
            .await?
            .build_raw(sender)
            .await?;

        let state_override = get_state_overrides(balance_overrides,nonce_overrides, code_overrides, state_overrides, &state_diff_overrides)?;

        if trace {
            if let Some(BlockId::Number(BlockNumberOrTag::Number(block_number))) = self.block {
                // Override Config `fork_block_number` (if set) with CLI value.
                config.fork_block_number = Some(block_number);
            }

            let create2_deployer = evm_opts.create2_deployer;
            let (mut env, fork, chain, odyssey) =
                TracingExecutor::get_fork_material(&config, evm_opts).await?;

            // modify settings that usually set in eth_call
            env.cfg.disable_block_gas_limit = true;
            env.block.gas_limit = U256::MAX;

            let trace_mode = TraceMode::Call
                .with_debug(debug)
                .with_decode_internal(if decode_internal {
                    InternalTraceMode::Full
                } else {
                    InternalTraceMode::None
                })
                .with_state_changes(shell::verbosity() > 4);
            let mut executor = TracingExecutor::new(
                env,
                fork,
                evm_version,
                trace_mode,
                odyssey,
                create2_deployer,
            )?;

            let value = tx.value.unwrap_or_default();
            let input = tx.inner.input.into_input().unwrap_or_default();
            let tx_kind = tx.inner.to.expect("set by builder");

            let trace = match tx_kind {
                TxKind::Create => {
                    let deploy_result = executor.deploy(from, input, value, None);
                    TraceResult::try_from(deploy_result)?
                }
                TxKind::Call(to) => TraceResult::from_raw(
                    executor.transact_raw(from, to, input, value)?,
                    TraceKind::Execution,
                ),
            };

            handle_traces(
                trace,
                &config,
                chain,
                labels,
                with_local_artifacts,
                debug,
                decode_internal,
            )
            .await?;

            return Ok(());
        }

        let cast = Cast::new(provider);
        sh_println!("{}", cast.call(&tx, func.as_ref(), block, state_override).await?)?;

        Ok(())
    }
}

impl figment::Provider for CallArgs {
    fn metadata(&self) -> Metadata {
        Metadata::named("CallArgs")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut map = Map::new();

        if self.odyssey {
            map.insert("odyssey".into(), self.odyssey.into());
        }

        if let Some(evm_version) = self.evm_version {
            map.insert("evm_version".into(), figment::value::Value::serialize(evm_version)?);
        }

        Ok(Map::from([(Config::selected_profile(), map)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{hex};

    #[test]
    fn can_parse_call_data() {
        let data = hex::encode("hello");
        let args = CallArgs::parse_from(["foundry-cli", "--data", data.as_str()]);
        assert_eq!(args.data, Some(data));

        let data = hex::encode_prefixed("hello");
        let args = CallArgs::parse_from(["foundry-cli", "--data", data.as_str()]);
        assert_eq!(args.data, Some(data));
    }

    #[test]
    fn can_parse_state_overrides() {
        let args = CallArgs::parse_from([
            "foundry-cli",
            "--override-balance",
            "0x123:0x1234",
            "--override-nonce",
            "0x123:1",
            "--override-code",
            "0x123:0x1234",
            "--override-state",
            "0x123:0x1:0x1234",
        ]);

        assert_eq!(args.balance_overrides, vec!["0x123:0x1234"]);
        assert_eq!(args.nonce_overrides, vec!["0x123:1"]);
        assert_eq!(args.code_overrides, vec!["0x123:0x1234"]);
        assert_eq!(args.state_overrides, vec!["0x123:0x1:0x1234"]);
    }

    #[test]
    fn can_parse_multiple_state_overrides() {
        let args = CallArgs::parse_from([
            "foundry-cli",
            "--override-balance",
            "0x123:0x1234",
            "--override-balance",
            "0x456:0x5678",
            "--override-nonce",
            "0x123:1",
            "--override-nonce",
            "0x456:2",
            "--override-code",
            "0x123:0x1234",
            "--override-code",
            "0x456:0x5678",
            "--override-state",
            "0x123:0x1:0x1234",
            "--override-state",
            "0x456:0x2:0x5678",
        ]);

        assert_eq!(args.balance_overrides, vec!["0x123:0x1234", "0x456:0x5678"]);
        assert_eq!(args.nonce_overrides, vec!["0x123:1", "0x456:2"]);
        assert_eq!(args.code_overrides, vec!["0x123:0x1234", "0x456:0x5678"]);
        assert_eq!(args.state_overrides, vec!["0x123:0x1:0x1234", "0x456:0x2:0x5678"]);
    }
}
