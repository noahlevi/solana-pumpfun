pub mod cli;
pub mod utils;

use futures_util::stream::StreamExt;
use log::error;
use solana_sdk::{pubkey, pubkey::Pubkey};
use solana_transaction_status::option_serializer::OptionSerializer;

use clap::Parser;
use tokio::sync::mpsc;
use tonic::transport::ClientTlsConfig;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    SubscribeRequest, SubscribeRequestFilterTransactions, SubscribeUpdate,
    subscribe_update::UpdateOneof,
};

use crate::cli::{Cli, Commands};
use crate::utils::{
    CreateTokenInfo, TransactionPretty, append_to_json_file, parse_create_token_data,
};

// static DEFAULT_GEYSER_ENDPOINT: &str = "https://solana-yellowstone-grpc.publicnode.com:443";
static DEFAULT_GEYSER_ENDPOINT: &str = "https://printworld.shyft.to";
const PUMPFUN_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Stream => {
            stream_pumpfun_launches().await?;
        }
    }

    Ok(())
}

async fn stream_pumpfun_launches() -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel::<SubscribeUpdate>(100);

    let mut client = GeyserGrpcClient::build_from_static(DEFAULT_GEYSER_ENDPOINT)
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to Geyser: {:?}", e))?;

    println!("Connected to Geyser at {}", DEFAULT_GEYSER_ENDPOINT);

    let mut subscribe_request = SubscribeRequest::default();
    subscribe_request.transactions.insert(
        "pumpfun".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            signature: None,
            account_include: vec![PUMPFUN_PROGRAM_ID.to_string()],
            account_exclude: vec![],
            account_required: vec![],
        },
    );

    tokio::spawn(async move {
        let (mut _subscribe_tx, subscribe_stream) =
            match client.subscribe_with_request(Some(subscribe_request)).await {
                Ok((tx, stream)) => (tx, stream),
                Err(e) => {
                    error!("Failed to subscribe: {:?}", e);
                    return;
                }
            };

        // process stream
        tokio::pin!(subscribe_stream);
        while let Some(message) = subscribe_stream.next().await {
            match message {
                Ok(update) => {
                    if let Err(e) = tx.send(update).await {
                        error!("Failed to send update: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    error!("Stream error: {:?}", e);
                    continue;
                }
            }
        }
    });

    // updates
    while let Some(msg) = rx.recv().await {
        if let Some(UpdateOneof::Transaction(subscribe_update_tx)) = msg.update_oneof {
            if let Err(e) = process_tx_update(TransactionPretty::from(subscribe_update_tx)).await {
                error!("Error processing account update: {:?}", e);
                continue;
            }
        }
    }

    Ok(())
}

async fn process_tx_update(transaction_pretty: TransactionPretty) -> anyhow::Result<()> {
    let trade_raw = transaction_pretty.tx;
    let meta = trade_raw
        .meta
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Missing transaction metadata"))?;

    if meta.err.is_some() {
        return Ok(());
    }

    let logs = if let OptionSerializer::Some(logs) = &meta.log_messages {
        logs
    } else {
        &vec![]
    };

    let instructions = parse_instruction(logs)?;

    for token_info in instructions {
        // print to console
        println!(
            "New Pumpfun Launch:\n\
        Token Address: {}\n\
        Bonding Curve Address: {}\n\
        Name: {}\n\
        Symbol: {}\n\
        Owner: {}\n\
        Slot: {}\n\
        ---",
            token_info.mint,
            token_info.bonding_curve,
            token_info.name,
            token_info.symbol,
            token_info.user,
            transaction_pretty.slot
        );

        append_to_json_file(&token_info)?;
    }

    Ok(())
}

pub fn parse_instruction(logs: &[String]) -> anyhow::Result<Vec<CreateTokenInfo>> {
    let mut current_instruction = None;
    let mut program_data = String::new();
    let mut invoke_depth = 0;
    let mut last_data_len = 0;

    let mut instructions: Vec<CreateTokenInfo> = vec![];

    for log in logs {
        // check program invocation
        if log.contains(&format!("Program {} invoke", PUMPFUN_PROGRAM_ID)) {
            invoke_depth += 1;
            if invoke_depth == 1 {
                // Only reset state at top level call
                current_instruction = None;
                program_data.clear();
                last_data_len = 0;
            }
            continue;
        }

        // skip if not
        if invoke_depth == 0 {
            continue;
        }

        // identify instruction type (only at top level)
        if invoke_depth == 1 && log.contains("Program log: Instruction:") {
            if log.contains("Create") {
                current_instruction = Some("create");
            } else if log.contains("Buy") || log.contains("Sell") {
                current_instruction = Some("trade");
            }
            continue;
        }

        // collect program data
        if log.starts_with("Program data: ") {
            let data = log.trim_start_matches("Program data: ");
            if data.len() > last_data_len {
                program_data = data.to_string();
                last_data_len = data.len();
            }
        }

        // check if program ends
        if log.contains(&format!("Program {} success", PUMPFUN_PROGRAM_ID)) {
            invoke_depth -= 1;
            if invoke_depth == 0 {
                // Only process data when top level program ends
                if let Some(instruction_type) = current_instruction {
                    if !program_data.is_empty() {
                        match instruction_type {
                            "create" => {
                                if let Ok(token_info) = parse_create_token_data(&program_data) {
                                    instructions.push(token_info);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    Ok(instructions)
}
