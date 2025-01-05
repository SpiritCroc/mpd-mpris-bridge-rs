use log::{debug, info, warn};

use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mpris::{PlayerFinder, Player};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    //TODO config
    info!("Binding to address...");
    let listener = TcpListener::bind("0.0.0.0:6602").await?;
    info!("Bound to address, listening...");

    loop {
        let (mut socket, addr) = listener.accept().await?;
        info!("Connected client {addr}");

        tokio::spawn(async move {
            // Greeting as per spec https://mpd.readthedocs.io/en/latest/protocol.html
            let mut buf = [0; 1024];
            if let Err(e) = socket.set_nodelay(true) {
                warn!("Failed to set nodelay: {:?}", e);
            }
            if let Err(e) = socket.write_all(b"OK MPD 0.23.16\n").await {
                warn!("Failed to write to socket; err = {:?}", e);
                return;
            }

            // In a loop, read data from the socket and write the data back.
            loop {
                debug!("Reading from {addr}...");
                let n = match socket.read(&mut buf).await {
                    // socket closed
                    Ok(0) => {
                        debug!("Socket closed: {addr}");
                        return
                    }
                    Ok(n) => n,
                    Err(e) => {
                        warn!("Failed to read from socket; err = {:?}", e);
                        return;
                    }
                };

                // Write the data back
                if let Err(e) = socket.write_all(&handle_mpd_query(&buf[0..n]).await).await {
                    warn!("Failed to write to socket; err = {:?}", e);
                    return;
                }
                debug!("Done reading {n} from {addr}");
            }
        });
    }
}

async fn handle_mpd_query(command: &[u8]) -> Vec<u8> {
    // TODO do things https://docs.rs/mpris/latest/mpris/
    // Compare also https://github.com/SpiritCroc/mpd-mpris-bridge/blob/master/index.js
    // And https://github.com/depuits/mpd-server
    // TODO more re-usable command parsing
    let result = match command {
        // Health
        b"ping\n" => handle_ping(),
        // Playback
        b"play\n" => handle_play(),
        b"pause \"1\"\n" => handle_pause(),
        b"pause\n" => handle_pause(),
        b"stop\n" => handle_stop(),
        b"next\n" => handle_next(),
        b"previous\n" => handle_previous(),
        // Infos
        b"currentsong\n" => handle_current_song(),
        b"status\n" => handle_status(),
        // Unsupported commands
        b"playlistinfo\n" => handle_dummy("playlistinfo"),
        b"close\n" => handle_dummy("close"),
        _ => handle_unknown_command(command)
    };
    return match result {
        Ok(v) => v,
        Err(e) => {
            warn!("Handling MPD query failed: {}", e);
            vec!(b'\n')
        }
    }
}

fn get_mpris_player() -> anyhow::Result<Player> {
    // TODO should I cache dbus connections or players, how fast is re-querying here?
    let player = PlayerFinder::new()?.find_active()?;
    Ok(player)
}

fn handle_ping() -> anyhow::Result<Vec<u8>> {
    match get_mpris_player() {
        Ok(_) => {
            debug!("Ping received, return OK");
            Ok(b"OK\n".to_vec())
        }
        Err(e) => {
            debug!("Ping failed: {:?}", e);
            Err(e)
        }
    }
}

fn handle_play() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.play()?;
    debug!("Handled play action");
    Ok(vec!(b'\n'))
}

fn handle_pause() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.pause()?;
    debug!("Handled pause action");
    Ok(vec!(b'\n'))
}

fn handle_stop() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.stop()?;
    debug!("Handled stop action");
    Ok(vec!(b'\n'))
}

fn handle_next() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.next()?;
    debug!("Handled next action");
    Ok(vec!(b'\n'))
}

fn handle_previous() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.previous()?;
    debug!("Handled previous action");
    Ok(vec!(b'\n'))
}

fn handle_current_song() -> anyhow::Result<Vec<u8>> {
    // TODO
    let file = "";
    let title = "";
    let artist = "";
    let time = 0;
    let duration = 0;
    let art_url = "";
    let response = format!("file: {file}\n\
                            Title: {title}\n\
                            Artist: {artist}\n\
                            Time: {time}\n\
                            duration: {duration}\n\
                            arturl: {art_url}\n");
    Ok(response.into())
}

fn handle_status() -> anyhow::Result<Vec<u8>> {
    // TODO
    let state = "";
    let time = 0;
    let elapsed = 0;
    let art_url = "";
    let response = format!("repeat: 0\n\
                            random: 0\n\
                            playlistlength: 1\n\
                            state: {state}\n\
                            time: {time}\n\
                            elapsed: {elapsed}\n\
                            arturl: {art_url}\n");
    Ok(response.into())
}

fn handle_unknown_command(command: &[u8]) -> anyhow::Result<Vec<u8>> {
    let err = match std::str::from_utf8(command) {
        Ok(s) => anyhow::anyhow!("Unknown command: {s}"),
        Err(_) => anyhow::anyhow!("Unknown non-UTF8 command"),
    };
    Err(err)
}

fn handle_dummy(name: &str) -> anyhow::Result<Vec<u8>> {
    debug!("Handling dummy action {name}");
    Ok(vec!(b'\n'))
}
