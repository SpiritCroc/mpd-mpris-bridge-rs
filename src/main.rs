use log::{trace, debug, info, warn, error};

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;

use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use mpris::{PlayerFinder, Player};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = 6601)]
    port: u16,
    #[arg(short, long, default_value_t = String::from("0.0.0.0"))]
    bind_address: String,
}

#[derive(Debug)]
enum Command {
    Play,
    Pause,
    Stop,
    Next,
    Prev,
}

struct MpdQueryState {
    command_tx: mpsc::Sender<Command>,
    in_command_list: bool,
    in_command_list_ok: bool,
    command_list_ended: bool,
    command_list_count: usize,
    command_list_failed: bool,
    last_idle_player_state: Option<PlayerState>,
    last_idle_playlist_state: Option<PlayerState>,
    last_idle_mixer_state: Option<u8>,
    should_close: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct PlayerState {
    playback_status: mpris::PlaybackStatus,
    title: Option<String>,
    artist: Option<String>,
    duration: Option<f32>,
    elapsed: Option<f32>,
    art_url: Option<String>,
}

struct MpdSharedState {
    null_volume: AtomicU8,
    player_state: Arc<RwLock<Option<PlayerState>>>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let args = Args::parse();

    let address = format!("{}:{}", args.bind_address, args.port);
    info!("Binding to address {address}...");

    let listener = TcpListener::bind(address).await?;
    info!("Bound to address, listening...");

    // TODO some signaling for idle in the other way round as well
    let (command_tx, command_rx) = mpsc::channel(8);
    let player_state = Arc::new(RwLock::new(None));

    let shared_state = Arc::new(MpdSharedState {
        player_state: player_state.clone(),
        null_volume: AtomicU8::new(0),
    });

    // Accept incoming MPD clients
    tokio::spawn(async move {
        loop {
            let (mut socket, addr) = listener.accept().await.unwrap();
            info!("Connected client {addr}");

            let shared_state = shared_state.clone();
            let command_tx = command_tx.clone();
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
                    command_tx: command_tx,
                    in_command_list: false,
                    in_command_list_ok: false,
                    command_list_ended: false,
                    command_list_count: 0,
                    command_list_failed: false,
                    last_idle_player_state: None,
                    last_idle_playlist_state: None,
                    last_idle_mixer_state: None,
                    should_close: false,
                };

                loop {
                    trace!("Reading from {addr}...");
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
                    trace!("Done reading {n} from {addr}");

                    // Handle commands
                    if let Err(e) = handle_mpd_queries(&mut socket, &buf[0..n], &mut state, shared_state.clone()).await {
                        warn!("Failed to handle MPD queries: {:?}", e);
                        return;
                    }
                    if state.should_close {
                        // Socket will close automatically when out of scope
                        return;
                    }
                }
            });
        }
    });

    // Observe and control local MPRIS player
    observe_mpris(command_rx, player_state).await;

    Ok(())
}

fn try_set_player_state(
    player_state: &Arc<RwLock<Option<PlayerState>>>,
    value: Option<PlayerState>,
    last_emitted_value: &mut Option<PlayerState>,
) {
    // Keep track locally of last set value to avoid retrieving the write lock if not necessary
    if *last_emitted_value == value {
        return
    }
    match player_state.write() {
        Ok(mut guard) => {
            *guard = value.clone();
            *last_emitted_value = value;
            trace!("Player state updated");
        }
        Err(_) => error!("Failed to write player state"),
    }
}

async fn observe_mpris(mut command_rx: mpsc::Receiver<Command>, player_state: Arc<RwLock<Option<PlayerState>>>) {
    let fail_delay = Duration::from_millis(1500);
    let poll_delay = Duration::from_millis(1000);
    let mut last_connect_err = None;
    let mut last_emitted_player_state = None;
    loop {
        try_set_player_state(&player_state, None, &mut last_emitted_player_state);
        let mut player = match find_mpris_player() {
            Ok(player) => player,
            Err(e) => {
                let connect_err = Some(format!("{e}"));
                if last_connect_err != connect_err {
                    warn!("Cannot select MPRIS player. {}", e);
                }
                last_connect_err = connect_err;
                sleep(fail_delay).await;
                continue;
            }
        };
        info!("Connected to MPRIS player. {:?}", player);
        last_connect_err = None;
        loop {
            match timeout(poll_delay, command_rx.recv()).await {
                Ok(Some(command)) => {
                    debug!("Handle command {command:?}");
                    match command {
                        Command::Play => {
                            if let Err(e) = player.play() {
                                error!("Failed to execute command {command:?}: {e}");
                            }
                        },
                        Command::Pause => {
                            if let Err(e) = player.pause() {
                                error!("Failed to execute command {command:?}: {e}");
                            }
                        },
                        Command::Stop => {
                            if let Err(e) = player.stop() {
                                error!("Failed to execute command {command:?}: {e}");
                            }
                        },
                        Command::Next => {
                            if let Err(e) = player.next() {
                                error!("Failed to execute command {command:?}: {e}");
                            }
                        },
                        Command::Prev => {
                            if let Err(e) = player.previous() {
                                error!("Failed to execute command {command:?}: {e}");
                            }
                        },
                    }
                }
                Ok(None) => warn!("Command channel closed"),
                Err(_) => trace!("Polling"),
            }
            let playback_status = match player.get_playback_status() {
                Ok(status) => status,
                Err(e) => {
                    warn!("Failed to read playback status, {}", e);
                    break;
                }
            };
            let metadata = match player.get_metadata() {
                Ok(metadata) => metadata,
                Err(e) => {
                    warn!("Failed to read metadata, {}", e);
                    break;
                }
            };
            let state = PlayerState {
                playback_status,
                title: metadata.title().map(|t| t.into()),
                artist: metadata.artists().map(|a| a.join(", ")),
                duration: metadata.length().map(|d| d.as_secs_f32()),
                elapsed: player.get_position().map(|d| d.as_secs_f32()).ok(),
                art_url: metadata.art_url().map(|u| u.into()),
            };
            try_set_player_state(&player_state, Some(state), &mut last_emitted_player_state);
            // If this player is not playing, need to check if another is
            if playback_status != mpris::PlaybackStatus::Playing {
                if let Ok(new_player) = find_mpris_player() {
                    if new_player.unique_name() != player.unique_name() {
                        info!("Switching active player to {new_player:?}");
                        player = new_player;
                    }
                }
            }
        };
    }
}

async fn handle_mpd_queries(
    socket: &mut TcpStream,
    commands: &[u8],
    state: &mut MpdQueryState,
    shared_state: Arc<MpdSharedState>,
) -> anyhow::Result<()> {
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
        match handle_mpd_query(&remainder, state, shared_state.clone(), socket).await {
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
                } else if state.should_close {
                    debug!("Closing the socket per request");
                    return Ok(());
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
async fn handle_mpd_query(
    command: &[u8],
    state: &mut MpdQueryState,
    shared_state: Arc<MpdSharedState>,
    socket: &mut TcpStream
) -> Result<Vec<u8>, MpdCommandError> {
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
        // Health/static commands
        b"ping" => handle_ping(),
        b"commands" => handle_commands(),
        b"tagtypes" => handle_tagtypes(),
        // Playback
        b"play" => handle_play(state).await,
        b"pause" => {
            match arguments {
                b"1" => handle_pause(state).await,
                b"\"1\"" => handle_pause(state).await,
                b"" => handle_pause(state).await,
                _ => {
                    debug!("Pause command with arguments {} mapped to play", safe_command_print(arguments));
                    handle_play(state).await
                }
            }
        }
        b"stop" => {
            // Some clients don't properly support stop, in which case pause is good enough
            match handle_stop(state).await {
                Err(e) => {
                    warn!("Handling stop failed, try with pause instead: {:?}", e);
                    handle_pause(state).await
                }
                v => v
            }
        }
        b"next" => handle_next(state).await,
        b"previous" => handle_previous(state).await,
        // Infos
        b"currentsong" => handle_current_song(shared_state),
        b"status" => handle_status(shared_state),
        b"idle" => handle_idle(arguments, state, shared_state, socket).await,
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
        // Silently ignored commands
        b"playlistinfo" => handle_dummy("playlistinfo"),
        b"lsinfo" => handle_dummy("lsinfo"),
        b"stats" => handle_dummy("stats"),
        b"close" => {
            state.should_close = true;
            Ok(Vec::new())
        }
        b"volume" => handle_volume(arguments, shared_state),
        b"setvol" => handle_setvol(arguments, shared_state),
        b"getvol" => handle_getvol(shared_state),
        b"noidle" => handle_dummy("noidle"),
        _ => handle_unknown_command(command)
    };
    result.map_err(|e|
        MpdCommandError::new(command, &format!("{:?}", e))
    )
}

fn find_mpris_player() -> anyhow::Result<Player> {
    let player = PlayerFinder::new()?.find_active()?;
    Ok(player)
}

fn handle_ping() -> anyhow::Result<Vec<u8>> {
    debug!("Ping successful");
    Ok(Vec::new())
}

fn handle_commands() -> anyhow::Result<Vec<u8>> {
    debug!("Returning supported commands");
    Ok("command: close\n\
        command: commands\n\
        command: currentsong\n\
        command: getvol\n\
        command: idle\n\
        command: lsinfo\n\
        command: next\n\
        command: pause\n\
        command: ping\n\
        command: play\n\
        command: playlistinfo\n\
        command: previous\n\
        command: setvol\n\
        command: stats\n\
        command: status\n\
        command: stop\n\
        command: tagtypes\n\
        command: volume\n".into())
}

fn handle_tagtypes() -> anyhow::Result<Vec<u8>> {
    debug!("Returning supported tagtypes");
    Ok("tagtype: Artist\n\
        tagtype: Album\n\
        tagtype: Title\n".into())
}


async fn handle_play(state: &mut MpdQueryState) -> anyhow::Result<Vec<u8>> {
    state.command_tx.send(Command::Play).await?;
    debug!("Ack play action");
    Ok(Vec::new())
}

async fn handle_pause(state: &mut MpdQueryState) -> anyhow::Result<Vec<u8>> {
    state.command_tx.send(Command::Pause).await?;
    debug!("Ack pause action");
    Ok(Vec::new())
}

async fn handle_stop(state: &mut MpdQueryState) -> anyhow::Result<Vec<u8>> {
    state.command_tx.send(Command::Stop).await?;
    debug!("Ack stop action");
    Ok(Vec::new())
}

async fn handle_next(state: &mut MpdQueryState) -> anyhow::Result<Vec<u8>> {
    state.command_tx.send(Command::Next).await?;
    debug!("Ack next action");
    Ok(Vec::new())
}

async fn handle_previous(state: &mut MpdQueryState) -> anyhow::Result<Vec<u8>> {
    state.command_tx.send(Command::Prev).await?;
    debug!("Ack prev action");
    Ok(Vec::new())
}

fn handle_current_song(shared_state: Arc<MpdSharedState>) -> anyhow::Result<Vec<u8>> {
    let Ok(player_state) = shared_state.player_state.read() else {
        error!("Failed to read player state for current song");
        return Ok(Vec::new());
    };
    let Some(ref player_state) = *player_state else {
        info!("Handled current song without player");
        return Ok(Vec::new());
    };
    let mut response: Vec<u8> = Vec::new();

    if let Some(title) = &player_state.title {
        response.append(&mut format!("file: {title}\nTitle: {title}\n").into());
    };
    if let Some(artist) = &player_state.artist {
        response.append(&mut format!("Artist: {artist}\n").into());
    };
    if let Some(duration) = &player_state.duration {
        response.append(&mut format!("Time: {duration}\nduration: {duration:.3}\n").into());
    };
    if let Some(art_url) = &player_state.art_url {
        response.append(&mut format!("arturl: {art_url}\n").into());
    };
    debug!("Handled current song with player state {:?}", player_state);
    Ok(response.into())
}

fn handle_dummy_status(volume: u8) -> Vec<u8> {
    format!("repeat: 0\n\
             random: 0\n\
             song: 0\n\
             playlistlength: 0\n\
             volume: {volume}\n\
             state: stop\n").into()
}

fn handle_status(shared_state: Arc<MpdSharedState>) -> anyhow::Result<Vec<u8>> {
    let volume = shared_state.null_volume.load(Ordering::SeqCst);
    let Ok(player_state) = shared_state.player_state.read() else {
        error!("Failed to read player state for status");
        return Ok(handle_dummy_status(volume));
    };
    let Some(ref player_state) = *player_state else {
        info!("Handled status without player");
        return Ok(handle_dummy_status(volume));
    };

    // https://mpd.readthedocs.io/en/latest/protocol.html
    let state = match player_state.playback_status {
        mpris::PlaybackStatus::Playing => "play",
        mpris::PlaybackStatus::Paused => "pause",
        mpris::PlaybackStatus::Stopped => "stop",
    };

    let response: &mut Vec<u8> =
        &mut format!(
            "repeat: 0\n\
             random: 0\n\
             song: 0\n\
             playlistlength: 1\n\
             volume: {volume}\n\
             state: {state}\n"
        ).into();

    if let Some(duration) = player_state.duration {
        response.append(&mut format!("duration: {duration}\n").into());
    };
    if let Some(elapsed) = player_state.elapsed {
        response.append(&mut format!("elapsed: {elapsed}\n").into());
        if let Some(duration) = player_state.duration {
            response.append(&mut format!("time: {elapsed:.0}:{duration:.0}\n").into());
        }
    };
    if let Some(art_url) = &player_state.art_url {
        response.append(&mut format!("arturl: {art_url}\n").into());
    };
    debug!("Handled status: {state}, volume {volume}");
    Ok(response.to_vec())
}

fn get_state_for_idle_player(player_state: &PlayerState) -> PlayerState {
    // Just a subset of values interesting for the idle command
    PlayerState {
        playback_status: player_state.playback_status,
        title: player_state.title.clone(),
        artist: player_state.artist.clone(),
        duration: None,
        elapsed: None,
        art_url: player_state.art_url.clone(),
    }
}

fn get_state_for_idle_playlist(player_state: &PlayerState) -> PlayerState {
    // Just a subset of values interesting for the idle command
    PlayerState {
        playback_status: mpris::PlaybackStatus::Paused,
        title: player_state.title.clone(),
        artist: player_state.artist.clone(),
        duration: None,
        elapsed: None,
        art_url: None,
    }
}

async fn handle_idle(
    arguments: &[u8],
    state: &mut MpdQueryState,
    shared_state: Arc<MpdSharedState>,
    socket: &mut TcpStream
) -> anyhow::Result<Vec<u8>> {
    let arguments = std::str::from_utf8(&arguments)?;
    let idle_all = arguments.len() == 0;
    let idle_player = idle_all || arguments.contains("\"player\"") || arguments.contains("player");
    let idle_playlist = idle_all || arguments.contains("\"playlist\"") || arguments.contains("playlist");
    let idle_mixer = idle_all || arguments.contains("\"mixer\"") || arguments.contains("mixer");
    if !idle_player && !idle_mixer && !idle_playlist {
        return Err(anyhow::anyhow!("No supported subsystem in {}", arguments));
    }
    debug!("Handling idle... subsystems: {}", arguments);
    let sleep_duration = Duration::from_millis(333);
    loop {
        if idle_player || idle_playlist {
            let current_raw_state = shared_state.player_state
                .read()
                .ok()
                .map(|inner| inner.clone())
                .flatten();
            if idle_player {
                let current_state = current_raw_state.as_ref().map(|state| get_state_for_idle_player(state));
                if current_state != state.last_idle_player_state {
                    info!("Handling idle finished with player status change");
                    state.last_idle_player_state = current_state;
                    return Ok(b"changed: player\n".to_vec());
                }
            }
            if idle_playlist {
                let current_state = current_raw_state.as_ref().map(|state| get_state_for_idle_playlist(state));
                if current_state != state.last_idle_playlist_state {
                    info!("Handling idle finished with playlist status change");
                    state.last_idle_playlist_state = current_state;
                    return Ok(b"changed: playlist\n".to_vec());
                }
            }
        }
        if idle_mixer {
            let current_volume = Some(shared_state.null_volume.load(Ordering::SeqCst));
            if current_volume != state.last_idle_mixer_state {
                debug!("Handling idle finished with mixer status change");
                state.last_idle_mixer_state = current_volume;
                return Ok(b"changed: mixer\n".to_vec());
            }
        }
        let mut buf = [0; 1024];
        let mut read: Vec<u8> = Vec::new();
        match timeout(sleep_duration, socket.read(&mut buf)).await {
            Ok(Ok(n)) => {
                read.append(&mut buf[0..n].to_vec());
                if let Some(i) = read.iter().position(|&b| b == b'\n' || b == b'\r') {
                    if &read[0..i] == b"noidle" {
                        debug!("Finish idle early due to noidle command");
                        return Ok(Vec::new());
                    }
                }
            }
            Ok(Err(e)) => {
                error!("Failed to read while idling: {e}");
                return Err(e.into());
            }
            Err(_) => {} // Just a timeout for idle state polling
        }
    }
}

fn handle_volume(arguments: &[u8], shared_state: Arc<MpdSharedState>) -> anyhow::Result<Vec<u8>> {
    let arguments = std::str::from_utf8(&arguments)?;
    debug!("Handling volume: {arguments}");
    let arguments = arguments.replace("\"", "");
    // Only allow u8 volume changes, but use bigger type for calculation without overflows
    let volume_change = arguments.parse::<i8>()? as i16;
    let volume = shared_state.null_volume.load(Ordering::SeqCst) as i16;
    let volume = (volume + volume_change).min(100).max(0) as u8;
    shared_state.null_volume.store(volume, Ordering::SeqCst);
    Ok(Vec::new())
}

fn handle_setvol(arguments: &[u8], shared_state: Arc<MpdSharedState>) -> anyhow::Result<Vec<u8>> {
    let arguments = std::str::from_utf8(&arguments)?;
    debug!("Handling setvol: {arguments}");
    let arguments = arguments.replace("\"", "");
    let volume = arguments.parse::<u8>()?;
    shared_state.null_volume.store(volume, Ordering::SeqCst);
    Ok(Vec::new())
}

fn handle_getvol(shared_state: Arc<MpdSharedState>) -> anyhow::Result<Vec<u8>> {
    let volume = shared_state.null_volume.load(Ordering::SeqCst);
    debug!("Handling getvol: {volume}");
    Ok(format!("volume: {volume}\n").into())
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
