use log::{trace, debug, info, warn};

use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;

use mpris::{PlayerFinder, Player};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    //TODO config
    info!("Binding to address...");
    let listener = TcpListener::bind("0.0.0.0:6601").await?;
    info!("Bound to address, listening...");

    loop {
        let (mut socket, addr) = listener.accept().await?;
        info!("Connected client {addr}");

        tokio::spawn(async move {
            let mut buf = [0; 1024];
            if let Err(e) = socket.set_nodelay(true) {
                warn!("Failed to set nodelay: {:?}", e);
            }

            // Send initial greeting
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
        if remainder.is_empty() {
            continue;
        }
        match handle_mpd_query(&remainder, state).await {
            Ok(response) => {
                if response.len() > 0 {
                    trace!("Respond {}", safe_command_print(&response));
                    socket.write_all(&response).await?;
                }
                if state.in_command_list_ok && !state.command_list_ended {
                    if state.command_list_count > 0 {
                        trace!("Respond list_OK");
                        socket.write_all(&b"list_OK\n".to_vec()).await?;
                    }
                } else {
                    trace!("Respond OK");
                    socket.write_all(&b"OK\n".to_vec()).await?;
                }
            }
            Err(e) => {
                warn!("Handling MPD query failed. {}", e);
                let error_response = format!("ACK [{}@{}] {} {}\n", e.mpd_error_code, e.command_str, state.command_list_count, e);
                trace!("Respond {}", error_response);
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
    let (command, arguments) = match command.iter().position(|&b| b == b' ') {
        Some(i) => (&command[0..i], &command[i+1..command.len()]),
        None => (command, &[] as &[u8])
    };
    // TODO more re-usable command parsing?
    if state.command_list_failed && command != b"command_list_end" {
        debug!("Ignore list command while in failed state: {}", safe_command_print(command));
        return Ok(Vec::new())
    };
    let result = match command {
        // Health
        b"ping" => handle_ping(),
        // Playback
        b"play" => handle_play(),
        b"pause" => {
            match arguments {
                b"1" => handle_pause(),
                b"\"1\"" => handle_pause(),
                b"" => handle_pause(),
                _ => {
                    debug!("Pause command with arguments {} mapped to play", safe_command_print(arguments));
                    handle_play()
                }
            }
        }
        b"stop" => {
            // Some clients don't properly support stop, in which case pause is good enough
            match handle_stop() {
                Err(e) => {
                    warn!("Handling stop failed, try with pause instead: {:?}", e);
                    handle_pause()
                }
                v => v
            }
        }
        b"next" => handle_next(),
        b"previous" => handle_previous(),
        // Infos
        b"currentsong" => handle_current_song(),
        b"status" => handle_status(),
        b"idle" => handle_idle(arguments).await,
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
    let player = PlayerFinder::new()?.find_active()?;
    Ok(player)
}

fn handle_ping() -> anyhow::Result<Vec<u8>> {
    debug!("Ping successful");
    Ok(Vec::new())
}

fn handle_play() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.play()?;
    debug!("Handled play action");
    Ok(Vec::new())
}

fn handle_pause() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.pause()?;
    debug!("Handled pause action");
    Ok(Vec::new())
}

fn handle_stop() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.stop()?;
    debug!("Handled stop action");
    Ok(Vec::new())
}

fn handle_next() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.next()?;
    debug!("Handled next action");
    Ok(Vec::new())
}

fn handle_previous() -> anyhow::Result<Vec<u8>> {
    get_mpris_player()?.previous()?;
    debug!("Handled previous action");
    Ok(Vec::new())
}

fn handle_current_song() -> anyhow::Result<Vec<u8>> {
    let Ok(player) = get_mpris_player() else {
        info!("Handled current song without player");
        return Ok(Vec::new());
    };
    let metadata = player.get_metadata()?;
    let mut response: Vec<u8> = Vec::new();

    if let Some(title) = metadata.title() {
        response.append(&mut format!("file: {title}\nTitle: {title}\n").into());
    };
    if let Some(artist) = metadata.artists().map(|a| a.join(", ")) {
        response.append(&mut format!("Artist: {artist}\n").into());
    };
    if let Some(duration) = metadata.length().map(|d| d.as_secs_f32()) {
        response.append(&mut format!("Time: {duration}\nduration: {duration:.3}\n").into());
    };
    if let Some(art_url) = metadata.art_url() {
        response.append(&mut format!("arturl: {art_url}\n").into());
    };
    debug!("Handled current song");
    Ok(response.into())
}

fn handle_status() -> anyhow::Result<Vec<u8>> {
    let Ok(player) = get_mpris_player() else {
        info!("Handled status without player");
        return Ok("repeat: 0\n\
                   random: 0\n\
                   song: 0\n\
                   playlistlength: 0\n\
                   state: stop\n".into());
    };
    let metadata = player.get_metadata()?;

    // https://mpd.readthedocs.io/en/latest/protocol.html
    let state = match player.get_playback_status()? {
        mpris::PlaybackStatus::Playing => "play",
        mpris::PlaybackStatus::Paused => "pause",
        mpris::PlaybackStatus::Stopped => "stop"
    };

    let response: &mut Vec<u8> =
        &mut format!(
            "repeat: 0\n\
             random: 0\n\
             song: 0\n\
             playlistlength: 1\n\
             state: {state}\n"
        ).into();

    let duration = metadata.length().map(|d| d.as_secs_f32());
    if let Some(duration) = duration {
        response.append(&mut format!("duration: {duration}\n").into());
    };
    if let Ok(elapsed) = player.get_position().map(|d| d.as_secs_f32()) {
        response.append(&mut format!("elapsed: {elapsed}\n").into());
        if let Some(duration) = duration {
            response.append(&mut format!("time: {elapsed}:{duration}\n").into());
        }
    };
    if let Some(art_url) = metadata.art_url() {
        response.append(&mut format!("arturl: {art_url}\n").into());
    };
    debug!("Handled status: {state}");
    Ok(response.to_vec())
}

fn get_player_state_for_idle() -> anyhow::Result<(mpris::PlaybackStatus, std::collections::HashMap<String, mpris::MetadataValue>)> {
    let player = get_mpris_player()?;
    let status = player.get_playback_status()?;
    let metadata: std::collections::HashMap<_, _> = player.get_metadata()?.into();
    Ok((status, metadata))
}

async fn handle_idle(arguments: &[u8]) -> anyhow::Result<Vec<u8>> {
    let arguments = std::str::from_utf8(&arguments)?;
    if arguments.len() > 0 && !arguments.contains("\"player\"") {
        return Err(anyhow::anyhow!("No supported subsystem in {}", arguments));
    }
    debug!("Handling idle... subsystems: {}", arguments);
    let initial_player_state = get_player_state_for_idle().ok();
    let sleep_duration = Duration::from_millis(333);
    loop {
        sleep(sleep_duration).await;
        if get_player_state_for_idle().ok() != initial_player_state {
            debug!("Handling idle finished with player status change");
            return Ok(b"changed: player\n".to_vec());
        }
    }
}

fn handle_unknown_command(command: &[u8]) -> anyhow::Result<Vec<u8>> {
    let safe_command = safe_command_print(command);
    debug!("Ignoring unknown command: {safe_command}");
    Err(anyhow::anyhow!("Unknown command: {safe_command}"))
}

fn handle_dummy(name: &str) -> anyhow::Result<Vec<u8>> {
    debug!("Handling dummy action {name}");
    Ok(Vec::new())
}
