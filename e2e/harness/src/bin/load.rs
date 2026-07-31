use goose::prelude::*;
use serde::Serialize;

const PROMPTS: [&str; 5] = [
    "reply with exactly: ok",
    "what is 3 + 4? reply with only the number",
    "say hi in one word",
    "reply with exactly: pong",
    "what is 12 * 12? reply with only the number",
];

#[derive(Serialize)]
struct NewThread<'a> {
    thread_id: &'a str,
}

#[derive(Serialize)]
struct Turn<'a> {
    message: &'a str,
}

struct Chat {
    thread_id: String,
    sent: usize,
}

async fn open_thread(user: &mut GooseUser) -> TransactionResult {
    let thread_id = format!("lt-{}-{}", std::process::id(), user.weighted_users_index);
    user.post_json("/threads", &NewThread { thread_id: &thread_id })
        .await?;
    user.set_session_data(Chat { thread_id, sent: 0 });
    Ok(())
}

async fn send_turn(user: &mut GooseUser) -> TransactionResult {
    let (path, prompt) = {
        let chat = user
            .get_session_data_mut::<Chat>()
            .expect("thread opened on start");
        let prompt = PROMPTS[chat.sent % PROMPTS.len()];
        chat.sent += 1;
        (format!("/threads/{}/runs/wait", chat.thread_id), prompt)
    };
    user.post_json(&path, &Turn { message: prompt }).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), GooseError> {
    GooseAttack::initialize()?
        .register_scenario(
            scenario!("Chat")
                .register_transaction(transaction!(open_thread).set_on_start())
                .register_transaction(transaction!(send_turn)),
        )
        .set_default(GooseDefault::Host, "http://127.0.0.1:8080")?
        .set_default(GooseDefault::Timeout, "300")?
        .execute()
        .await?;
    Ok(())
}
