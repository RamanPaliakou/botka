use std::fmt::Write;
use std::sync::Arc;

use anyhow::Result;
use diesel::prelude::*;
use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::*;
use teloxide::types::{
    Forward, ForwardedFrom, InlineKeyboardButton, InlineKeyboardMarkup, Me,
    MediaKind, MessageCommon, MessageKind, ReplyMarkup, User,
};

use crate::common::{
    format_user, format_users, is_resident, BotEnv, CommandHandler,
};
use crate::db::DbUserId;
use crate::utils::{BotExt, ResultExt, Sqlizer};
use crate::{models, schema};

pub fn message_handler() -> CommandHandler<Result<()>> {
    dptree::filter_map(filter_polls).endpoint(handle_message)
}

pub fn poll_answer_handler() -> CommandHandler<Result<()>> {
    Update::filter_poll_answer().endpoint(handle_poll_answer)
}

pub fn callback_handler() -> CommandHandler<Result<()>> {
    dptree::filter_map(filter_callbacks).endpoint(handle_callback)
}

#[derive(Debug, Clone)]
enum PollKind {
    New(Poll),
    Forward(String),
}

fn filter_polls(me: Me, env: Arc<BotEnv>, msg: Message) -> Option<PollKind> {
    let poll = match &msg.kind {
        MessageKind::Common(MessageCommon {
            media_kind: MediaKind::Poll(poll),
            ..
        }) => &poll.poll,
        _ => return None,
    };

    match msg.forward() {
        #[allow(clippy::nonminimal_bool)]
        None if true
                && poll.question.starts_with('!')
                // Do not touch polls that already have votes or closed
                && poll.total_voter_count == 0
                && !poll.is_closed
                // We don't need to track anonymous polls
                && !poll.is_anonymous
                // Bots can't obtain information from quiz polls, so skip them
                && poll.poll_type == teloxide::types::PollType::Regular
                // Allow only residents
                && is_resident(&mut env.conn(), msg.from.as_ref()?) =>
        {
            Some(PollKind::New(poll.clone()))
        }
        Some(Forward {
            from: ForwardedFrom::User(User { id, .. }), ..
        }) if id == &me.user.id
            && msg.chat.is_private()
            && is_resident(&mut env.conn(), msg.from.as_ref()?) =>
        {
            Some(PollKind::Forward(poll.id.clone()))
        }
        _ => None,
    }
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    env: Arc<BotEnv>,
    kind: PollKind,
) -> Result<()> {
    match kind {
        PollKind::New(poll) => intercept_new_poll(bot, msg, poll, env).await,
        PollKind::Forward(poll_id) => {
            hande_poll_forward(bot, msg, &poll_id, env).await
        }
    }
}

async fn intercept_new_poll(
    bot: Bot,
    msg: Message,
    poll: Poll,
    env: Arc<BotEnv>,
) -> Result<()> {
    let mut new_poll = bot
        .send_poll(
            msg.chat.id,
            poll.question,
            poll.options.iter().map(|o| o.text.clone()),
        )
        .is_anonymous(poll.is_anonymous)
        .allows_multiple_answers(poll.allows_multiple_answers);
    new_poll.close_date = poll.close_date;
    new_poll.message_thread_id = msg.thread_id;
    new_poll.reply_to_message_id = msg.reply_to_message().map(|m| m.id);
    let new_poll = new_poll.await?;

    #[allow(clippy::single_match_else)]
    let poll_id = match new_poll.kind {
        MessageKind::Common(MessageCommon {
            media_kind: MediaKind::Poll(poll),
            ..
        }) => poll.poll.id,
        _ => {
            // TODO: return error
            log::error!("Expected poll, got {new_poll:?}");
            bot.delete_message(msg.chat.id, msg.id)
                .await
                .log_error("delete message");
            return Ok(());
        }
    };

    if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
        // TODO: check rights before sending message
        log::warn!("Failed to delete poll message: {e}");
        bot.delete_message(msg.chat.id, new_poll.id)
            .await
            .log_error("delete message");
        return Ok(());
    }

    let non_voters = db_find_non_voters(&mut env.conn(), &[]);

    // TODO: clean up this
    let from = msg.from.as_ref().unwrap();
    let creator_id = from.id.into();
    let creator_info = models::TgUser {
        id: creator_id,
        username: from.username.clone(),
        first_name: from.first_name.clone(),
        last_name: from.last_name.clone(),
    };

    let poll_info = bot
        .reply_message(
            &msg,
            poll_text((creator_id, Some(creator_info)), &non_voters?, 0),
        )
        .reply_to_message_id(new_poll.id)
        .parse_mode(teloxide::types::ParseMode::Html)
        .reply_markup(ReplyMarkup::InlineKeyboard(make_keyboard(&poll_id)))
        .disable_web_page_preview(true)
        .await?;

    diesel::insert_into(schema::tracked_polls::table)
        .values(&models::TrackedPoll {
            tg_poll_id: poll_id,
            creator_id,
            info_chat_id: poll_info.chat.id.into(),
            info_message_id: poll_info.id.into(),
            voted_users: Sqlizer::new(Vec::new()).unwrap(),
        })
        .execute(&mut *env.conn())?;

    Ok(())
}

async fn hande_poll_forward(
    bot: Bot,
    msg: Message,
    poll_id: &str,
    env: Arc<BotEnv>,
) -> Result<()> {
    let poll_results = env.transaction(|conn| {
        let Some((db_poll, _)) = db_find_poll(conn, poll_id)? else {
            return Ok(None);
        };
        let non_voters = db_find_non_voters(conn, &db_poll.voted_users)?;
        Ok(Some(non_voters))
    })?;

    let mut text = String::new();

    if let Some(non_voters) = poll_results {
        if non_voters.is_empty() {
            write!(text, "Everyone voted!").unwrap();
        } else {
            non_voters
                .iter()
                .flat_map(|(_, u)| u)
                .filter_map(|u| u.username.as_ref())
                .for_each(|u| {
                    write!(text, " @{u}").unwrap();
                });
        }
    } else {
        write!(text, "Unknown poll").unwrap();
    }

    bot.reply_message(&msg, text).disable_web_page_preview(true).await?;

    Ok(())
}

async fn handle_poll_answer(
    bot: Bot,
    poll_answer: PollAnswer,
    env: Arc<BotEnv>,
) -> Result<()> {
    let update = env.transaction(|conn| {
        let Some((db_poll, creator)) =
            db_find_poll(conn, &poll_answer.poll_id)?
        else {
            return Ok(None);
        };

        let mut voted_users = (*db_poll.voted_users).clone();
        if poll_answer.option_ids.is_empty() {
            voted_users.retain(|&u| u != poll_answer.user.id.into());
        } else {
            voted_users.push(poll_answer.user.id.into());
        }
        voted_users.sort();
        voted_users.dedup();

        diesel::update(schema::tracked_polls::table)
            .filter(schema::tracked_polls::tg_poll_id.eq(&poll_answer.poll_id))
            .set(
                schema::tracked_polls::voted_users
                    .eq(Sqlizer::new(voted_users.clone()).unwrap()),
            )
            .execute(conn)?;

        let non_voters = db_find_non_voters(conn, &voted_users)?;

        Ok(Some((
            db_poll.info_chat_id,
            db_poll.info_message_id,
            (db_poll.creator_id, creator),
            non_voters,
            voted_users.len(),
        )))
    })?;

    let Some((
        info_chat_id,
        info_message_id,
        creator,
        non_voters,
        total_voters,
    )) = update
    else {
        return Ok(());
    };

    bot.edit_message_text(
        info_chat_id,
        info_message_id.into(),
        poll_text(creator, &non_voters, total_voters),
    )
    .parse_mode(teloxide::types::ParseMode::Html)
    .reply_markup(make_keyboard(&poll_answer.poll_id))
    .disable_web_page_preview(true)
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
struct StopPollQuery {
    poll_id: String,
    action: Action,
}

#[derive(Debug, Copy, Clone)]
enum Action {
    Stop,
    Confirm,
    Cancel,
}

fn filter_callbacks(callback: CallbackQuery) -> Option<StopPollQuery> {
    let data = callback.data.as_ref()?.strip_prefix("p:")?;
    let (action, poll_id) = data.split_once(':')?;
    let action = match action {
        "stop" => Action::Stop,
        "confirm" => Action::Confirm,
        "cancel" => Action::Cancel,
        _ => return None,
    };
    Some(StopPollQuery { poll_id: poll_id.to_string(), action })
}

async fn handle_callback(
    bot: Bot,
    env: Arc<BotEnv>,
    stop: StopPollQuery,
    callback: CallbackQuery,
) -> Result<()> {
    let db_poll = db_find_poll(&mut env.conn(), &stop.poll_id)?;
    let Some((db_poll, _)) = db_poll else {
        bot.answer_callback_query(&callback.id).text("Poll not found.").await?;
        return Ok(());
    };

    if callback.from.id != UserId::from(db_poll.creator_id) {
        bot.answer_callback_query(&callback.id)
            .text("You are not the creator of this poll.")
            .await?;
        return Ok(());
    }

    // TODO: store poll message id in the database
    let poll_message =
        callback.message.as_ref().and_then(|m| m.reply_to_message());
    let Some(poll_message) = poll_message else {
        bot.answer_callback_query(&callback.id)
            .text("Poll message not found.")
            .await?;
        return Ok(());
    };

    let reply_markup = match stop.action {
        Action::Stop => {
            bot.answer_callback_query(&callback.id)
                .text(
                    // Based on the original Telegram client message
                    "If you stop this poll now, \
                    nobody will be able to vote in anymore. \
                    This action cannot be undone.",
                )
                .show_alert(true)
                .await?;
            Some(make_keyboard_confirmation(&stop.poll_id))
        }
        Action::Confirm => {
            bot.answer_callback_query(&callback.id).await?;
            bot.stop_poll(db_poll.info_chat_id, poll_message.id).await?;
            None
        }
        Action::Cancel => {
            bot.answer_callback_query(&callback.id).await?;
            Some(make_keyboard(&stop.poll_id))
        }
    };

    let mut edit = bot.edit_message_reply_markup(
        db_poll.info_chat_id,
        db_poll.info_message_id.into(),
    );
    edit.reply_markup = reply_markup;
    edit.await?;

    Ok(())
}

fn poll_text(
    creator: (DbUserId, Option<models::TgUser>),
    non_voters: &[(DbUserId, Option<models::TgUser>)],
    total_voters: usize,
) -> String {
    let mut text = String::new();

    text.push_str("Poll by ");
    format_user(&mut text, creator.0, &creator.1, true);
    text.push_str(". ");

    if non_voters.is_empty() {
        text.push_str("Everyone voted!");
    } else {
        write!(
            text,
            "Voted {} user{}, pending vote {} user{}: ",
            total_voters,
            if total_voters == 1 { "" } else { "s" },
            non_voters.len(),
            if non_voters.len() == 1 { "" } else { "s" },
        )
        .unwrap();
        format_users(&mut text, non_voters.iter().map(|(id, u)| (*id, u)));
        text.push_str(".\n");
    }

    text
}

fn make_keyboard(poll_id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Stop poll",
        format!("p:stop:{poll_id}"),
    )]])
}

fn make_keyboard_confirmation(poll_id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(
            "Cancel (do not stop)",
            format!("p:cancel:{poll_id}"),
        ),
        InlineKeyboardButton::callback(
            "Confirm (stop poll)",
            format!("p:confirm:{poll_id}"),
        ),
    ]])
}

fn db_find_poll(
    conn: &mut SqliteConnection,
    poll_id: &str,
) -> Result<
    Option<(models::TrackedPoll, Option<models::TgUser>)>,
    diesel::result::Error,
> {
    schema::tracked_polls::table
        .filter(schema::tracked_polls::tg_poll_id.eq(poll_id))
        .left_join(
            schema::tg_users::table
                .on(schema::tracked_polls::creator_id.eq(schema::tg_users::id)),
        )
        .first(conn)
        .optional()
}

fn db_find_non_voters(
    conn: &mut SqliteConnection,
    voted_users: &[DbUserId],
) -> Result<Vec<(DbUserId, Option<models::TgUser>)>, diesel::result::Error> {
    // TODO: filter only residents at the moment of poll creation
    schema::residents::table
        .filter(schema::residents::tg_id.ne_all(voted_users))
        .filter(schema::residents::end_date.is_null())
        .left_join(
            schema::tg_users::table
                .on(schema::residents::tg_id.eq(schema::tg_users::id)),
        )
        .select((
            schema::residents::tg_id,
            schema::tg_users::all_columns.nullable(),
        ))
        .load(conn)
}
