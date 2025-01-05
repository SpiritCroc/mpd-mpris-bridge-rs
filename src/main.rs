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

            let mut state = MpdQueryState {
                in_command_list: false,
                in_command_list_ok: false,
                command_list_ended: false,
                command_list_count: 0,
                command_list_failed: false,
            };

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
                if let Err(e) = handle_mpd_queries(&mut socket, &buf[0..n], &mut state).await {
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
    command_list_failed: bool,
}

#[derive(Debug)]
struct MpdCommandError {
    //command: Vec<u8>,
    command_str: String,
    message: String,
    mpd_error_code: i8,
}
impl std::error::Error for MpdCommandError {}
impl std::fmt::Display for MpdCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Command {} failed: {}", self.command_str, self.message)
    }
}

fn safe_command_print(command: &[u8]) -> &str {
    match std::str::from_utf8(&command) {
        Ok(s) => s,
        Err(_) => "[un-utf8 command]"
    }
}

impl MpdCommandError {
    pub fn new(command: &[u8], message: &str) -> MpdCommandError {
        let command_str = safe_command_print(&command);
        MpdCommandError {
            //command: command.to_vec(),
            command_str: command_str.to_string(),
            message: message.to_string(),
            // Error codes: https://github.com/MusicPlayerDaemon/MPD/blob/master/src/protocol/Ack.hxx
            // 5 is unknown
            mpd_error_code: 5,
        }
    }
}

async fn handle_mpd_queries(socket: &mut TcpStream, commands: &[u8], state: &mut MpdQueryState) -> anyhow::Result<()> {
    let mut remainder = commands.to_vec();
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
        match handle_mpd_query(&remainder, state).await {
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
                let error_response = format!("ACK [{}@{}] {} {}\n", e.mpd_error_code, e.command_str, state.command_list_count, e);
                socket.write_all(&error_response.as_bytes()).await?;
                break;
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
async fn handle_mpd_query(command: &[u8], state: &mut MpdQueryState) -> Result<Vec<u8>, MpdCommandError> {
    // TODO do things https://docs.rs/mpris/latest/mpris/
    // Compare also https://github.com/SpiritCroc/mpd-mpris-bridge/blob/master/index.js
    // And https://github.com/depuits/mpd-server
    // TODO more re-usable command parsing
    if state.command_list_failed && command != b"command_list_end" {
        debug!("Ignore list command while in failed state: {}", safe_command_print(command));
        return Ok(Vec::new())
    };
    let result = match command {
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
    };
    result.map_err(|e|
        MpdCommandError::new(command, &format!("{:?}", e))
    )
}

fn get_mpris_player() -> anyhow::Result<Player> {
    // TODO should I cache dbus connections or players, how fast is re-querying here?
    // TODO if no active found, should try to re-use the one which was active last, if available
    let player = PlayerFinder::new()?.find_active()?;
    Ok(player)
}

fn handle_ping() -> anyhow::Result<Vec<u8>> {
    let _ = get_mpris_player()?;
    debug!("Ping successful, return OK");
    Ok(b"OK\n".to_vec())
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
    let player = get_mpris_player()?;
    let metadata = player.get_metadata()?;
    let file = metadata.url().unwrap_or_default();
    let title = metadata.title().unwrap_or_default();
    let artist = metadata.artists().unwrap_or_default().join(", ");
    let duration = metadata.length().unwrap_or_default();
    let duration = duration.as_secs_f32();
    let art_url = "";
    let response = format!("file: {file}\n\
                            Title: {title}\n\
                            Artist: {artist}\n\
                            Time: {duration}\n\
                            duration: {duration}\n\
                            arturl: {art_url}\n");
    debug!("Handled current song: {title}");
    Ok(response.into())
}

fn handle_status() -> anyhow::Result<Vec<u8>> {
    let player = get_mpris_player()?;
    // https://mpd.readthedocs.io/en/latest/protocol.html
    let state = match player.get_playback_status()? {
        mpris::PlaybackStatus::Playing => "play",
        mpris::PlaybackStatus::Paused => "pause",
        mpris::PlaybackStatus::Stopped => "stop"
    };
    let metadata = player.get_metadata()?;
    let duration = metadata.length().unwrap_or_default();
    let duration = duration.as_secs_f32();
    let elapsed = player.get_position().ok().unwrap_or_default();
    let elapsed = elapsed.as_secs_f32();
    let art_url = metadata.art_url().unwrap_or_default();
    let response = format!("repeat: 0\n\
                            random: 0\n\
                            playlistlength: 1\n\
                            state: {state}\n\
                            time: {duration}\n\
                            elapsed: {elapsed}\n\
                            arturl: {art_url}\n");
    debug!("Handled status: {state}");
    Ok(response.into())
}

fn handle_unknown_command(command: &[u8]) -> anyhow::Result<Vec<u8>> {
    let safe_command = safe_command_print(command);
    debug!("Ignoring unknown command: {safe_command}");
    Err(anyhow::anyhow!("Unknown command: {safe_command}"))
}

fn handle_dummy(name: &str) -> anyhow::Result<Vec<u8>> {
    debug!("Handling dummy action {name}");
    Ok(vec!(b'\n'))
}
