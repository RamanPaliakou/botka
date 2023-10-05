// @generated automatically by Diesel CLI.

diesel::table! {
    borrowed_items (chat_id, user_message_id) {
        chat_id -> BigInt,
        thread_id -> Integer,
        user_message_id -> Integer,
        bot_message_id -> Integer,
        user_id -> BigInt,
        items -> Text,
    }
}

diesel::table! {
    forwards (orig_chat_id) {
        orig_chat_id -> BigInt,
        orig_msg_id -> Integer,
        backup_chat_id -> BigInt,
        backup_msg_id -> Integer,
        backup_text -> Text,
    }
}

diesel::table! {
    options (name) {
        name -> Text,
        value -> Text,
    }
}

diesel::table! {
    residents (tg_id) {
        tg_id -> BigInt,
        is_resident -> Bool,
        is_bot_admin -> Bool,
    }
}

diesel::table! {
    tg_chats (id) {
        id -> BigInt,
        kind -> Text,
        username -> Nullable<Text>,
        title -> Nullable<Text>,
    }
}

diesel::table! {
    tg_users (id) {
        id -> BigInt,
        username -> Nullable<Text>,
        first_name -> Text,
        last_name -> Nullable<Text>,
    }
}

diesel::table! {
    tracked_polls (tg_poll_id) {
        tg_poll_id -> Text,
        creator_id -> BigInt,
        info_chat_id -> BigInt,
        info_message_id -> Integer,
        voted_users -> Text,
    }
}

diesel::table! {
    user_macs (tg_id, mac) {
        tg_id -> BigInt,
        mac -> Text,
    }
}

diesel::joinable!(forwards -> tg_users (orig_chat_id));

diesel::allow_tables_to_appear_in_same_query!(
    borrowed_items,
    forwards,
    options,
    residents,
    tg_chats,
    tg_users,
    tracked_polls,
    user_macs,
);
