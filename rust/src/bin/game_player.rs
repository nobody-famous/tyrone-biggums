use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    ops::DerefMut,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{future::join_all, pin_mut, stream::SplitSink, SinkExt, StreamExt};

use rust::{
    error::BoomerError,
    server::message::{Message, MessageType},
};

use structopt::StructOpt;

use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite, WebSocketStream};
use url::Url;

#[derive(Debug, StructOpt, Clone)]
pub struct ServerOpts {
    /// Activate debug mode
    pub count: usize,

    /// The port to use for the events to be served on
    #[structopt(short = "h", long = "host", default_value = "0.0.0.0")]
    pub host: String,

    /// The address to use.  Should be 0.0.0.0
    #[structopt(short = "p", long = "port", default_value = "42069")]
    pub port: u16,

    /// The address to use.  Should be 0.0.0.0
    #[structopt(long = "path", default_value = "/")]
    pub path: String,

    /// time between added connections.  ms
    #[structopt(long = "time", default_value = "50")]
    pub time: usize,

    /// The address to use.  Should be 0.0.0.0
    #[structopt(short = "c", long = "connections")]
    pub connection_count: Option<usize>,
}

pub struct ServerConfig {
    /// Activate debug mode
    pub count: usize,
    pub host: String,
    pub time: usize,
    pub port: u16,
    pub path: String,
    pub connection_count: usize,
}

impl ServerConfig {
    fn new(opts: ServerOpts) -> ServerConfig {
        return ServerConfig {
            count: opts.count,
            host: opts.host,
            port: opts.port,
            path: opts.path,
            time: opts.time,
            connection_count: opts.connection_count.expect("connection_count should be set by some default value before creating the server config"),
        };
    }
}

const WRITE_COUNT: usize = 40;
type SplitStreamWrite = SplitSink<
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tungstenite::Message,
>;
type SplitStreamRead = futures::stream::SplitStream<
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

struct HashableWriter {
    writer: SplitStreamWrite,
    id: usize,
}

impl PartialEq for HashableWriter {
    fn eq(&self, other: &Self) -> bool {
        return self.id == other.id;
    }
}

impl Eq for HashableWriter {}

impl Hash for HashableWriter {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

type WriteArray = [HashMap<usize, HashableWriter>; WRITE_COUNT];
type AMVWrite = Arc<Mutex<WriteArray>>;

const TIME_BETWEEN_LOOPS: u64 = 5000;
async fn fire_loop(callees: AMVWrite) -> Result<(), BoomerError> {
    let mut then = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("come on")
        .as_micros();
    let mut idx = 0;
    let msg: tungstenite::Message = Message::new(MessageType::Fire).try_into()?;
    println!("msg to be sent: {}", msg);

    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("come on")
            .as_micros();
        let diff = now - then;
        if diff < TIME_BETWEEN_LOOPS.into() {
            let sleep_time = TIME_BETWEEN_LOOPS.saturating_sub(diff as u64);
            tokio::time::sleep(Duration::from_micros(sleep_time)).await;
        } else {
            // println!("unable to send messages within {} us", TIME_BETWEEN_LOOPS);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("come on")
            .as_micros();

        // let writers_mutex = &mut callees.lock().await;
        // let writers_maps = writers_mutex.deref_mut();

        // for callees in writers_maps {
        //     for writer in callees.iter_mut() {
        //         match writer.1.writer.send(msg.clone()).await {
        //             Err(e) => {
        //                 println!("error during fire_loop: {:?}", e);
        //             }
        //             _ => {}
        //         }
        //     }
        // }

        let callees = &mut callees.lock().await[idx];
        for writer in callees.iter_mut() {
            match writer.1.writer.send(msg.clone()).await {
                Err(e) => {
                    println!("error during fire_loop: {:?}", e);
                }
                _ => {}
            }
        }

        then = now;
        idx = (idx + 1) % WRITE_COUNT;
    }
}

async fn connect(url: Url, id: usize) -> Option<(HashableWriter, SplitStreamRead)> {
    let (ws_stream, _) = match connect_async(url).await {
        Ok((x, y)) => (x, y),
        Err(_) => return None,
    };
    let (write, read) = ws_stream.split();

    return Some((HashableWriter { writer: write, id }, read));
}

fn get_connection_count() -> usize {
    return str::parse(&std::env::var("CONNECTION_COUNT").expect(
        "There has to be a connection count set in either the cli args or through env vars.",
    ))
    .expect("connection count to be a number");
}

async fn next_message(read: &mut SplitStreamRead) -> Result<Option<Message>, BoomerError> {
    // wait for the readyup
    let next = read.next().await;
    if let Some(Ok(tungstenite::Message::Text(msg))) = next {
        let msg: Message = msg.try_into()?;
        return Ok(Some(msg));
    }
    return Ok(None);
}

async fn send_ready(writer: &mut HashableWriter) -> Result<(), BoomerError> {
    let msg: tungstenite::Message = Message::new(MessageType::ReadyUp).try_into()?;

    writer.writer.send(msg).await?;
    return Ok(());
}

async fn play(
    url: Url,
    id: usize,
    writers: AMVWrite,
    config: Arc<Mutex<ServerConfig>>,
    offset: usize,
) -> Result<(), BoomerError> {
    // helps prevent 500 connections at once then for games to be played in huge spirts...
    // seems unrealistic.
    tokio::time::sleep(Duration::from_millis(offset as u64)).await;
    while config.lock().await.count > 0 {
        {
            let mut config = config.lock().await;
            config.count = config.count.saturating_sub(1);
        }

        // if there is an error, no need to crash the whole test, just reconnect.
        let connected = connect(url.clone(), id).await;
        if connected.is_none() {
            {
                config.lock().await.count += 1;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            println!("failed to connect");
            continue;
        }

        let (mut write, mut read) = connected.unwrap();

        if config.lock().await.count % 500 == 0 {
            println!("{} games left to play", config.lock().await.count);
        }

        // TODO: there has to be better way...
        if let Ok(Some(Message::Message(msg))) = next_message(&mut read).await {
            if let MessageType::ReadyUp = msg.r#type {
                send_ready(&mut write).await?;
            } else {
                return Err(BoomerError::PlayerReadyUpError);
            }
        } else {
            return Err(BoomerError::PlayerReadyUpError);
        }

        // TODO: there has to be better way...
        if let Ok(Some(Message::Message(msg))) = next_message(&mut read).await {
            if let MessageType::Play = msg.r#type {
            } else {
                return Err(BoomerError::PlayerFireCommand);
            }
        } else {
            return Err(BoomerError::PlayerFireCommand);
        }

        writers.lock().await[id % WRITE_COUNT].insert(id, write);

        // TODO: there has to be better way...
        if let Ok(Some(Message::Message(msg))) = next_message(&mut read).await {
            if let MessageType::GameOver = msg.r#type {
                if let Some(msg) = msg.msg {
                    if msg.starts_with("winner") {
                        println!("{}", msg);
                    }
                }
            } else {
                return Err(BoomerError::PlayerGameOver);
            }
        } else {
            return Err(BoomerError::PlayerGameOver);
        }

        {
            let mut writer = writers.lock().await;
            writer[id % WRITE_COUNT].remove(&id);
        }
    }

    return Ok(());
}

fn get_config() -> Arc<Mutex<ServerConfig>> {
    let mut opts = ServerOpts::from_args();
    if opts.connection_count.is_none() {
        opts.connection_count = Some(get_connection_count());
    }

    return Arc::new(Mutex::new(ServerConfig::new(opts)));
}

#[tokio::main]
async fn main() -> Result<(), BoomerError> {
    env_logger::init();

    let opts = get_config();
    let url: Url;
    {
        let opts = opts.lock().await;
        url = url::Url::parse(format!("ws://{}:{}{}", opts.host, opts.port, opts.path).as_str())
            .unwrap();
    }

    let time_between = opts.lock().await.time;
    let maps = create_maps();
    let writers: AMVWrite = Arc::new(Mutex::new(maps));
    let fire_loop_await = fire_loop(writers.clone());

    let mut awaits = vec![];
    let connection_count = opts.lock().await.connection_count;
    for i in 0..connection_count {
        awaits.push(play(
            url.clone(),
            i,
            writers.clone(),
            opts.clone(),
            i * time_between,
        ));
    }
    println!("created {} players", connection_count);

    // TODO: don't care... should I?
    pin_mut!(fire_loop_await);
    match futures::future::select(join_all(awaits), fire_loop_await).await {
        _ => {}
    }
    println!("done.");

    return Ok(());
}

fn create_maps() -> [HashMap<usize, HashableWriter>; WRITE_COUNT] {
    // todo: clearly research macros.......
    return [
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    ];
}
