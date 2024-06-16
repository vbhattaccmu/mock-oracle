use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use axum::{
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Extension, Json, Router,
};
use bigdecimal::{BigDecimal, Zero};
use clap::Parser;
use log::{debug, error, info, trace};
use num_integer::Integer;
use serde::{Deserialize, Serialize};
use starknet::{
    accounts::{Account, Call, ConnectedAccount, ExecutionEncoding, SingleOwnerAccount},
    core::{
        types::{
            BlockId, BlockTag, FieldElement, FunctionCall, MaybePendingTransactionReceipt,
            StarknetError,
        },
        utils::{cairo_short_string_to_felt, parse_cairo_short_string},
    },
    macros::selector,
    providers::{
        jsonrpc::{HttpTransport, JsonRpcClient},
        AnyProvider, MaybeUnknownErrorCode, Provider, ProviderError, SequencerGatewayProvider,
        StarknetErrorWithMessage,
    },
    signers::{LocalWallet, Signer, SigningKey},
};
use tokio::sync::Mutex;
use url::Url;

const TX_WATCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Cli {
    #[clap(long, env = "PRIVATE_KEY", help = "Private key in plain text")]
    private_key: FieldElement,
    #[clap(
        long,
        env = "ACCOUNT_ADDRESS",
        help = "Address of the account contract"
    )]
    account_address: FieldElement,
    #[clap(
        long,
        env = "ORACLE_ADDRESS",
        help = "Address of the mock oracle contract"
    )]
    oracle_address: FieldElement,
    #[clap(long, env = "CHAIN_ID", help = "Network ID")]
    chain_id: FieldElement,
    #[clap(
        long,
        env = "PAIR_IDS",
        help = "Price feed pair IDs separated by commas"
    )]
    pair_ids: String,
    #[clap(long, env = "PORT", default_value = "3000", help = "Port to listen on")]
    port: u16,
    #[clap(flatten)]
    provider: ProviderOptions,
    #[clap(
        long,
        env = "MAX_FEE",
        help = "Optionally specify transaction max fees manually"
    )]
    max_fee: Option<BigDecimal>,
    #[clap(
        long,
        env = "MIN_INTERNAL",
        default_value = "30",
        help = "Minimal duration between transactions in seconds"
    )]
    min_internal: u64,
}

#[derive(Debug, Parser)]
struct ProviderOptions {
    #[clap(long, env = "JSONRPC", help = "URL of the JSON-RPC interface")]
    jsonrpc: Option<Url>,
    #[clap(long, env = "GATEWAY", help = "URL of the sequencer gateway")]
    gateway: Option<Url>,
    #[clap(
        long,
        env = "FEEDER_GATEWAY",
        help = "URL of the sequencer feeder gateway"
    )]
    feeder_gateway: Option<Url>,
    #[clap(
        long,
        env = "CHAIN_ID",
        help = "Chain ID to be used for sequencer gateway only"
    )]
    chain_id: Option<FieldElement>,
}

#[derive(Debug, Clone, Serialize)]
struct Pair {
    id: String,
    decimals: u32,
    current_price: FieldElement,
    next_price: Option<FieldElement>,
}

#[derive(Debug, Clone, Serialize)]
struct PairModel {
    id: String,
    decimals: u32,
    current_price: BigDecimal,
    next_price: Option<BigDecimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangePriceRequest {
    pair: String,
    price: BigDecimal,
}

impl TryFrom<ProviderOptions> for AnyProvider {
    type Error = anyhow::Error;

    fn try_from(value: ProviderOptions) -> Result<Self> {
        match (
            value.jsonrpc,
            value.gateway,
            value.feeder_gateway,
            value.chain_id,
        ) {
            (Some(jsonrpc), None, None, None) => Ok(AnyProvider::JsonRpcHttp(JsonRpcClient::new(
                HttpTransport::new_with_client(
                    jsonrpc,
                    reqwest::ClientBuilder::new()
                        .timeout(Duration::from_secs(10))
                        .build()
                        .expect("unable to build HTTP client"),
                ),
            ))),
            (None, Some(gateway), Some(feeder_gateway), Some(chain_id)) => {
                Ok(AnyProvider::SequencerGateway(
                    SequencerGatewayProvider::new(gateway, feeder_gateway, chain_id),
                ))
            }
            _ => anyhow::bail!("invalid provider options"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    run(cli).await
}

async fn run(cli: Cli) -> Result<()> {
    let provider: AnyProvider = cli.provider.try_into()?;
    let provider = Arc::new(provider);

    let pair_ids = cli
        .pair_ids
        .split(',')
        .map(cairo_short_string_to_felt)
        .collect::<Result<Vec<_>, _>>()?;
    debug!("{} pairs configured", pair_ids.len());

    debug!("Fetching pair data...");
    let pairs = fetch_pairs(&provider, cli.oracle_address, &pair_ids).await?;
    debug!("Successfully fetched pairs");

    let pairs = Arc::new(Mutex::new(pairs));

    // Kicks start long-running services
    tokio::spawn(send_indefinitely(
        provider,
        pairs.clone(),
        cli.private_key,
        cli.account_address,
        cli.oracle_address,
        cli.chain_id,
        match cli.max_fee {
            Some(value) => Some(bigdecimal_to_felt(&value, 18)?),
            None => None,
        },
        Duration::from_secs(cli.min_internal),
    ));

    let app = Router::new()
        .route("/", get(home))
        .route("/api/prices", get(get_prices))
        .route("/api/price", post(change_price))
        .layer(Extension(pairs));
    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    debug!("Listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_indefinitely<'a, P>(
    provider: Arc<P>,
    pairs: Arc<Mutex<Vec<Pair>>>,
    private_key: FieldElement,
    account_address: FieldElement,
    oracle_address: FieldElement,
    chain_id: FieldElement,
    max_fee: Option<FieldElement>,
    min_interval: Duration,
) -> Result<()>
where
    P: Provider + Send + Sync,
    P::Error: 'static,
{
    let signer = LocalWallet::from(SigningKey::from_secret_scalar(private_key));
    let signer_public_key = signer.get_public_key().await?.scalar();

    let mut account = SingleOwnerAccount::new(
        provider.clone(),
        signer,
        account_address,
        chain_id,
        ExecutionEncoding::Legacy,
    );
    account.set_block_id(BlockId::Tag(BlockTag::Pending));

    info!(
        "Using account {:#064x} with public key {:#064x}",
        signer_public_key,
        account.address()
    );

    loop {
        match send_once(&provider, &account, &pairs, oracle_address, max_fee).await {
            Ok(tx_send_time) => {
                let time_since_sent = SystemTime::now()
                    .duration_since(tx_send_time)
                    .unwrap_or_default();

                if time_since_sent < min_interval {
                    let time_to_wait = min_interval - time_since_sent;

                    debug!(
                        "Sleeping for {}s before sending next transaction...",
                        time_to_wait.as_secs_f64()
                    );
                    tokio::time::sleep(time_to_wait).await;
                }
            }
            Err(err) => error!("Error submitting new prices: {err}"),
        }
    }
}

async fn send_once<'a, A, P>(
    provider: &'a P,
    account: &'a A,
    pairs: &Mutex<Vec<Pair>>,
    oracle_address: FieldElement,
    max_fee: Option<FieldElement>,
) -> Result<SystemTime>
where
    A: ConnectedAccount + Sync,
    A::SignError: 'static,
    <A::Provider as Provider>::Error: 'static,
    P: Provider + Sync,
    P::Error: 'static,
{
    struct PricedPair {
        pair_id: String,
        decimals: u32,
        price: FieldElement,
    }

    let priced_pairs = {
        let pairs = pairs.lock().await;

        pairs
            .iter()
            .map(|pair| PricedPair {
                pair_id: pair.id.clone(),
                decimals: pair.decimals,
                price: pair.next_price.unwrap_or(pair.current_price),
            })
            .collect::<Vec<_>>()
    };

    let calls = priced_pairs
        .iter()
        .map(|pair| {
            Ok(Call {
                to: oracle_address,
                selector: selector!("submit"),
                calldata: vec![
                    cairo_short_string_to_felt(&pair.pair_id)?,
                    pair.price,
                    pair.decimals.into(),
                ],
            })
        })
        .collect::<Result<Vec<_>, anyhow::Error>>()?;

    let submit_call = account.execute(calls).fee_estimate_multiplier(4.0);
    let submit_call = if let Some(max_fee) = max_fee {
        submit_call.max_fee(max_fee)
    } else {
        submit_call
    };

    debug!("Sending submission transaction...");
    let submit_tx = submit_call.send().await?;
    info!(
        "Submission transaction sent: {:#064x}",
        submit_tx.transaction_hash
    );

    let tx_send_time = SystemTime::now();

    debug!("Waiting for transaction to confirm...");
    watch_tx(provider, submit_tx.transaction_hash).await?;
    info!("Transaction confirmed");

    // Update pair prices
    {
        let mut pairs = pairs.lock().await;

        pairs.iter_mut().enumerate().for_each(|(ind_pair, pair)| {
            let submitted_price = priced_pairs[ind_pair].price;

            pair.current_price = submitted_price;
            if let Some(next_price) = pair.next_price {
                if submitted_price == next_price {
                    pair.next_price = None;
                }
            }
        });
    };

    Ok(tx_send_time)
}

async fn fetch_pairs<'a, P>(
    provider: &'a P,
    oracle_address: FieldElement,
    pair_ids: &'a [FieldElement],
) -> Result<Vec<Pair>>
where
    P: Provider,
    P::Error: 'static,
{
    let mut result = vec![];

    for pair_id in pair_ids.iter() {
        // 0: price
        // 1: decimals
        // 2: last_updated_timestamp
        // 3: num_sources_aggregated
        let spot_median = provider
            .call(
                FunctionCall {
                    contract_address: oracle_address,
                    entry_point_selector: selector!("get_spot_median"),
                    calldata: vec![*pair_id],
                },
                BlockId::Tag(BlockTag::Pending),
            )
            .await?;

        result.push(Pair {
            id: parse_cairo_short_string(pair_id)?,
            decimals: spot_median[1].try_into()?,
            current_price: spot_median[0],
            next_price: None,
        });
    }

    Ok(result)
}

async fn watch_tx<P>(provider: P, tx_hash: FieldElement) -> Result<MaybePendingTransactionReceipt>
where
    P: Provider,
    P::Error: 'static,
{
    let start_time = SystemTime::now();

    let receipt = loop {
        // TODO: check with sequencer gateway if it's not confirmed after an extended period of
        // time, as full nodes don't have access to failed transactions and would report them
        // as `NotReceived`.
        match provider.get_transaction_receipt(tx_hash).await {
            Ok(receipt) => {
                // With JSON-RPC, once we get a receipt, the transaction must have been confirmed.
                // Rejected transactions simply aren't available. This needs to be changed once we
                // implement the sequencer fallback.

                trace!("Transaction {:#064x} confirmed", tx_hash);
                break receipt;
            }
            Err(ProviderError::StarknetError(StarknetErrorWithMessage {
                code: MaybeUnknownErrorCode::Known(StarknetError::TransactionHashNotFound),
                ..
            })) => {
                trace!("Transaction not confirmed yet...");

                if SystemTime::now().duration_since(start_time)? >= TX_WATCH_TIMEOUT {
                    anyhow::bail!("transaction watching timed out");
                }
            }
            Err(err) => return Err(err.into()),
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    Ok(receipt)
}

async fn home() -> (StatusCode, Html<&'static str>) {
    (StatusCode::OK, Html(include_str!("../html/index.html")))
}

async fn get_prices(
    Extension(pairs): Extension<Arc<Mutex<Vec<Pair>>>>,
) -> (StatusCode, Json<Vec<PairModel>>) {
    (
        StatusCode::OK,
        Json(
            pairs
                .lock()
                .await
                .iter()
                .map(|pair| PairModel {
                    id: pair.id.clone(),
                    decimals: pair.decimals,
                    current_price: pair.current_price.to_big_decimal(pair.decimals),
                    next_price: pair
                        .next_price
                        .map(|value| value.to_big_decimal(pair.decimals)),
                })
                .collect::<Vec<_>>(),
        ),
    )
}

async fn change_price(
    Extension(pairs): Extension<Arc<Mutex<Vec<Pair>>>>,
    Json(body): Json<ChangePriceRequest>,
) -> StatusCode {
    match pairs
        .lock()
        .await
        .iter_mut()
        .find(|pair| pair.id == body.pair)
    {
        Some(pair) => {
            let new_price = match bigdecimal_to_felt(&body.price, pair.decimals) {
                Ok(value) => value,
                Err(_) => return StatusCode::BAD_REQUEST,
            };

            pair.next_price = if pair.current_price == new_price {
                None
            } else {
                Some(new_price)
            };

            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

#[allow(clippy::comparison_chain)]
fn bigdecimal_to_felt<D>(dec: &BigDecimal, decimals: D) -> Result<FieldElement>
where
    D: Into<i64>,
{
    let decimals: i64 = decimals.into();

    // Scale the bigint part up or down
    let (bigint, exponent) = dec.as_bigint_and_exponent();

    let mut biguint = match bigint.to_biguint() {
        Some(value) => value,
        None => anyhow::bail!("too many decimal places"),
    };

    if exponent < decimals {
        for _ in 0..(decimals - exponent) {
            biguint *= 10u32;
        }
    } else if exponent > decimals {
        for _ in 0..(exponent - decimals) {
            let (quotient, remainder) = biguint.div_rem(&10u32.into());
            if !remainder.is_zero() {
                anyhow::bail!("too many decimal places")
            }
            biguint = quotient;
        }
    }

    Ok(FieldElement::from_byte_slice_be(&biguint.to_bytes_be())?)
}
