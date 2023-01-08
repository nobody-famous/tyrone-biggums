use std::sync::Arc;

use rust::{
    game::{play_the_game, ActiveGames},
    server::{
        server::{handle_connection, Server},
        socket::Socket,
    },
};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    env_logger::init();
    // console_subscriber::init();

    let mut server = Server::new().await?;
    println!("starting server");

    let active_games = Arc::new(Mutex::new(ActiveGames::new()));
    let mut other_socket: Option<Socket> = None;
    let listener = &mut server.listener;

    while let Ok((stream, _)) = listener.accept().await {
        let socket = handle_connection(stream).await;
        if let Some(other_socket) = other_socket.take() {
            tokio::spawn(play_the_game((other_socket, socket), active_games.clone()));
        } else {
            other_socket = Some(socket);
        }
    }

    Ok(())
}
