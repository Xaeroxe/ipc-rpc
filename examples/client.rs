use std::env;

use ipc_rpc::{rpc_call, ConnectionKey, IpcRpcClient};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, JsonSchema)]
enum Message {
    HowAreYou,
    Good,
    GoodToHear,
    MakeMeASandwich,
    ASandwich(Vec<SandwichComponent>),
}

#[derive(Deserialize, Serialize, Debug, Clone, JsonSchema)]
enum SandwichComponent {
    Bread,
    PeanutButter,
    Jelly,
}

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .try_init()
        .unwrap();
    let key = env::args().nth(1).expect("First argument is server key");
    let client =
        IpcRpcClient::initialize_client(ConnectionKey::try_from(key).unwrap(), message_handler)
            .await
            .unwrap();
    let resp = client.send(Message::HowAreYou).await.unwrap();
    log::info!("Message from server: {resp:?}");
    match resp {
        Message::Good => {
            // Drop the response, we're not expecting to get one.
            let _ = client.send(Message::GoodToHear);
        }
        _ => {
            log::info!("Hm, not sure what to do with that response.");
        }
    }
    rpc_call!(
        sender: client,
        to_send: Message::MakeMeASandwich,
        receiver: Message::ASandwich(parts) => {
            log::info!("I got a sandwich! It contains\n{parts:#?}");
        },
    )
    .unwrap()
}

async fn message_handler(_message: Message) -> Option<Message> {
    // The client doesn't respond to spontaneous messages, it only sends spontaneous messages.
    None
}
