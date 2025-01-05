use log::{debug, info, warn};

use tokio::net::{TcpListener, TcpStream};
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

                // Handle commands
                if let Err(e) = handle_mpd_queries(&mut socket, &buf[0..n]).await {
                    warn!("Failed to handle MPD queries: {:?}", e);
                    return;
                }
                debug!("Done reading {n} from {addr}");
            }
        });
    }
}

struct MpdQueryState {
    in_command_list: bool,
    in_command_list_ok: bool,
    command_list_ended: bool,
    command_list_count: usize,
}

async fn handle_mpd_queries(socket: &mut TcpStream, commands: &[u8]) -> anyhow::Result<()> {
    let mut remainder = commands.to_vec();
    let mut state = MpdQueryState {
        in_command_list: false,
        in_command_list_ok: false,
        command_list_ended: false,
        command_list_count: 0,
    };
    loop {
        if remainder.is_empty() {
            break;
        }
        if remainder[0] == b'\n' {
            remainder.remove(0);
            continue;
        }
        let new_remainder = match remainder.iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(i) => remainder.split_off(i),
            None => remainder.clone()
        };
        match handle_mpd_query(&remainder, &mut state).await {
            Ok(response) => {
                socket.write_all(&response).await?;
                if state.command_list_ended {
                    socket.write_all(&b"OK\n".to_vec()).await?;
                } else if state.in_command_list_ok && state.command_list_count > 0 {
                    socket.write_all(&b"list_OK\n".to_vec()).await?;
                }
            }
            Err(e) => {
                warn!("Handling MPD query failed. {}", e);
                // TODO replace "hmmm" with command name, and replace `2` with some appropriate
                //  error code number
                let error_response = format!("ACK [2@{}] hmmm {}", state.command_list_count, e);
                socket.write_all(&error_response.as_bytes()).await?;
            }
        }
        remainder = new_remainder;
        if state.command_list_ended {
            state.in_command_list = false;
            state.in_command_list_ok = false;
            state.command_list_ended = false;
        } else if state.in_command_list {
            state.command_list_count += 1;
        }
    }
    Ok(())
}

/// Execute a query and returns the response to send back
async fn handle_mpd_query(command: &[u8], state: &mut MpdQueryState) -> anyhow::Result<Vec<u8>> {
    // TODO do things https://docs.rs/mpris/latest/mpris/
    // Compare also https://github.com/SpiritCroc/mpd-mpris-bridge/blob/master/index.js
    // And https://github.com/depuits/mpd-server
    // TODO more re-usable command parsing
    match command {
        // Health
        b"ping" => handle_ping(),
        // Playback
        b"play" => handle_play(),
        b"pause \"1\"" => handle_pause(),
        b"pause" => handle_pause(),
        b"stop" => handle_stop(),
        b"next" => handle_next(),
        b"previous" => handle_previous(),
        // Infos
        b"currentsong" => handle_current_song(),
        b"status" => handle_status(),
        // Aggregating commands
        b"command_list_begin" => {
            debug!("Received command_list_begin");
            state.in_command_list = true;
            Ok(Vec::new())
        }
        b"command_list_ok_begin" => {
            debug!("Received command_list_ok_begin");
            state.in_command_list = true;
            state.in_command_list_ok = true;
            Ok(Vec::new())
        }
        b"command_list_end" => {
            debug!("Received command_list_end");
            state.command_list_ended = true;
            Ok(Vec::new())
        }
        // Unsupported commands
        b"playlistinfo" => handle_dummy("playlistinfo"),
        b"close" => handle_dummy("close"),
        _ => handle_unknown_command(command)
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
    debug!("Handled current song: {title}");
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
    debug!("Handled status: {state}");
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
