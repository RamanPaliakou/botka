#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
// False positives
#![allow(clippy::needless_pass_by_value)] // for dptree handlers
// Style
#![allow(clippy::items_after_statements)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::redundant_closure_for_method_calls)]
// FIXME: fix these
#![allow(clippy::too_many_lines)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use argh::FromArgs;
use common::{MyDialogue, State};
use diesel::sqlite::SqliteConnection;
use diesel::Connection;
use metrics_exporter_prometheus::PrometheusBuilder;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::dispatching::{Dispatcher, HandlerExt, UpdateFilterExt};
use teloxide::payloads::AnswerCallbackQuerySetters;
use teloxide::requests::Requester;
use teloxide::types::{CallbackQuery, Message, Update};
use teloxide::Bot;
use tokio_util::sync::CancellationToken;
use utils::HandlerExt as _;

mod common;
mod config;
mod db;
mod metrics;
mod models;
mod modules;
mod schema;
mod tracing_proxy;
mod utils;
mod web_srv;

static VERSION: OnceLock<String> = OnceLock::new();

fn version() -> &'static str {
    VERSION.get().expect("VERSION is not set")
}

#[derive(FromArgs, PartialEq, Debug)]
/// botka
struct Args {
    #[argh(option, hidden_help = true, long = "-set-revision")]
    set_revision: Option<String>,

    #[argh(subcommand)]
    subcommand: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    Bot(SubCommandBot),
    Scrape(SubCommandScrape),
}

#[derive(FromArgs, PartialEq, Debug)]
/// run the bot
#[argh(subcommand, name = "bot")]
struct SubCommandBot {
    #[argh(positional)]
    /// config file
    config_file: OsString,
}

#[derive(FromArgs, PartialEq, Debug)]
/// scrape the log
#[argh(subcommand, name = "scrape")]
struct SubCommandScrape {
    #[argh(positional)]
    /// db file
    db_file: String,

    #[argh(positional)]
    /// log file
    log_file: OsString,

    #[argh(positional)]
    /// list of residential_chats
    residential_chats: Vec<i64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    std::env::set_var("RUST_LOG", "info");
    pretty_env_logger::init();
    let args: Args = argh::from_env();
    VERSION
        .set(args.set_revision.unwrap_or_else(|| {
            git_version::git_version!(fallback = "unknown").to_string()
        }))
        .unwrap();
    log::info!("Version {}", version());
    match args.subcommand {
        SubCommand::Bot(c) => run_bot(&c.config_file).await?,
        SubCommand::Scrape(c) => {
            scrape_log(&c.db_file, &c.log_file, &c.residential_chats)?;
        }
    }
    Ok(())
}

async fn run_bot(config_fpath: &OsStr) -> Result<()> {
    let prometheus = PrometheusBuilder::new().install_recorder()?;
    metrics::register_metrics();
    modules::borrowed_items::register_metrics();

    let config: crate::config::Config =
        serde_yaml::from_reader(File::open(config_fpath)?)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

    let bot_env = Arc::new(common::BotEnv {
        conn: Mutex::new(SqliteConnection::establish(&config.db)?),
        reqwest_client: reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .build()?,
        openai_client: async_openai::Client::with_config(
            async_openai::config::OpenAIConfig::new()
                .with_api_key(config.services.openai.api_key.clone()),
        ),
        config: Arc::new(config),
    });

    let proxy_addr =
        tracing_proxy::start(bot_env.config.log_file.as_str()).await?;
    let bot = Bot::new(&bot_env.config.telegram.token).set_api_url(proxy_addr);

    let mut dispatcher = Dispatcher::builder(
        bot.clone(),
        dptree::entry()
            .inspect(modules::tg_scraper::scrape) // should be the first handler
            .inspect(modules::resident_tracker::handle_update)
            .branch(
                Update::filter_message()
                    .enter_dialogue::<Message, InMemStorage<State>, State>()
                    .inspect_async(reset_dialogue_on_command)
                    .inspect_err(modules::rename_closed_topics::inspect_message)
                    .inspect_err(modules::forward_topic_pins::inspect_message)
                    .branch(modules::basic::command_handler())
                    .branch(modules::debates::command_handler())
                    .branch(modules::userctl::command_handler())
                    .branch(
                        dptree::case![State::Forward]
                            .endpoint(modules::debates::debate_send),
                    )
                    .branch(modules::polls::message_handler())
                    .branch(modules::borrowed_items::command_handler())
                    .branch(modules::needs::message_handler())
                    .endpoint(drop_endpoint),
            )
            .branch(
                Update::filter_callback_query()
                    .branch(modules::needs::callback_handler())
                    .branch(modules::polls::callback_handler())
                    .branch(modules::borrowed_items::callback_handler())
                    .endpoint(drop_callback_query),
            )
            .branch(modules::polls::poll_answer_handler())
            .endpoint(drop_endpoint),
    )
    .dependencies(dptree::deps![InMemStorage::<State>::new(), bot_env.clone()])
    .build();
    let bot_shutdown_token = dispatcher.shutdown_token().clone();
    let mut join_handles = Vec::new();
    join_handles.push(tokio::spawn(async move { dispatcher.dispatch().await }));

    let cancel = CancellationToken::new();

    join_handles.push(tokio::spawn(modules::updates::task(
        bot_env.clone(),
        bot.clone(),
        cancel.clone(),
    )));

    join_handles.push(tokio::spawn(web_srv::run(
        SqliteConnection::establish(&bot_env.config.db)?,
        bot_env.config.clone(),
        prometheus,
        cancel.clone(),
    )));

    run_signal_handler(bot_shutdown_token.clone(), cancel.clone());

    futures::future::join_all(join_handles).await;

    Ok(())
}

fn scrape_log(
    db_fpath: &str,
    log_fpath: &OsStr,
    residential_chats: &[i64],
) -> Result<()> {
    let mut conn = SqliteConnection::establish(db_fpath)?;
    let mut log_file = File::open(log_fpath)?;
    let mut buf_reader = BufReader::new(&mut log_file);
    let mut line = String::new();

    conn.exclusive_transaction(|conn| {
        while buf_reader.read_line(&mut line)? > 0 {
            let update: Update = serde_json::from_str(&line)?;
            modules::tg_scraper::scrape_raw(conn, &update)?;
            modules::resident_tracker::handle_update_raw(
                conn,
                &update,
                &residential_chats
                    .iter()
                    .map(|&i| teloxide::types::ChatId(i))
                    .collect::<Vec<_>>(),
            )?;
            line.clear();
        }
        Result::<_, anyhow::Error>::Ok(())
    })?;
    Ok(())
}

async fn reset_dialogue_on_command(msg: Message, dialogue: MyDialogue) {
    let message_is_command =
        msg.entities().and_then(|e| e.first()).is_some_and(|e| {
            e.kind == teloxide::types::MessageEntityKind::BotCommand
                && e.offset == 0
        });
    if message_is_command {
        dialogue.update(State::Start).await.ok();
    }
}

async fn drop_callback_query(
    bot: Bot,
    callback_query: CallbackQuery,
) -> Result<()> {
    log::warn!(
        "Unexpected callback query: {:?}",
        serde_json::to_string(&callback_query).unwrap()
    );
    bot.answer_callback_query(&callback_query.id)
        .text("Error: unexpected callback query")
        .await?;
    Ok(())
}

async fn drop_endpoint() -> Result<()> {
    Ok(())
}

fn run_signal_handler(
    bot_shutdown_token: teloxide::dispatching::ShutdownToken,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::signal::ctrl_c().await.expect("Failed to listen for SIGINT");
            cancel.cancel();
            match bot_shutdown_token.shutdown() {
                Ok(f) => {
                    log::info!(
                        "^C received, trying to shutdown the dispatcher..."
                    );
                    f.await;
                    log::info!("dispatcher is shutdown...");
                }
                Err(_) => {
                    log::info!("^C received, the dispatcher isn't running, ignoring the signal");
                }
            }
        }
    });
}
