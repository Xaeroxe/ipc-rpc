use std::ffi::OsString;

use ipc_rpc::IpcRpc;

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
    let mut server_client_combo = IpcRpc::build()
        .finish("target/debug/examples/client", message_handler, |key, cmd| {
            let key: OsString = key.into();
            cmd.arg(key);
        })
        .await
        .unwrap();
    server_client_combo
        .server
        .wait_for_client_to_connect()
        .await
        .unwrap();
    log::info!("client connected!");
    server_client_combo
        .server
        .wait_for_client_to_disconnect()
        .await
        .unwrap();
}

async fn message_handler(message: Message) -> Option<Message> {
    log::info!("Message from client: {message:?}");
    match message {
        Message::HowAreYou => Some(Message::Good),
        Message::GoodToHear => {
            // Do nothing, this is a response to our response.
            None
        }
        Message::Good => {
            log::info!("... huh? I didn't say anything to you.");
            None
        }
        Message::MakeMeASandwich => Some(Message::ASandwich(vec![
            SandwichComponent::Bread,
            SandwichComponent::PeanutButter,
            SandwichComponent::Jelly,
            SandwichComponent::Bread,
        ])),
        Message::ASandwich(_) => {
            log::info!("...I didn't really need a sandwich.");
            None
        }
    }
}
