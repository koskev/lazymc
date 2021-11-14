use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use futures::FutureExt;
use minecraft_protocol::data::server_status::ServerStatus;
use tokio::process::Command;
use tokio::time;

use crate::config::Config;
use crate::os;

/// Server cooldown after the process quit.
/// Used to give it some more time to quit forgotten threads, such as for RCON.
const SERVER_QUIT_COOLDOWN: Duration = Duration::from_millis(2500);

/// Server state.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum State {
    /// Server is stopped.
    Stopped,

    /// Server is starting.
    Starting,

    /// Server is online and responding.
    Started,

    /// Server is stopping.
    Stopping,
}

impl State {
    /// From u8, panics if invalid.
    pub fn from_u8(state: u8) -> Self {
        match state {
            0 => Self::Stopped,
            1 => Self::Starting,
            2 => Self::Started,
            3 => Self::Stopping,
            _ => panic!("invalid State u8"),
        }
    }

    /// To u8.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Starting => 1,
            Self::Started => 2,
            Self::Stopping => 3,
        }
    }
}

/// Shared server state.
#[derive(Debug)]
pub struct Server {
    /// Server state.
    ///
    /// Matches `State`, utilzes AtomicU8 for better performance.
    state: AtomicU8,

    /// Server process PID.
    ///
    /// Set if a server process is running.
    pid: Mutex<Option<u32>>,

    /// Last known server status.
    ///
    /// Will remain set once known, not cleared if server goes offline.
    status: RwLock<Option<ServerStatus>>,

    /// Last active time.
    ///
    /// The last time there was activity on the server. Also set at the moment the server comes
    /// online.
    last_active: RwLock<Option<Instant>>,

    /// Force server to stay online until.
    keep_online_until: RwLock<Option<Instant>>,

    /// Time to force kill the server process at.
    ///
    /// Used as starting/stopping timeout.
    kill_at: RwLock<Option<Instant>>,
}

impl Server {
    /// Get current state.
    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// Set a new state.
    ///
    /// This updates various other internal things depending on how the state changes.
    ///
    /// Returns false if the state didn't change, in which case nothing happens.
    fn update_state(&self, state: State, config: &Config) -> bool {
        self.update_state_from(None, state, config)
    }

    /// Set new state, from a current state.
    ///
    /// This updates various other internal things depending on how the state changes.
    ///
    /// Returns false if current state didn't match `from` or if nothing changed.
    fn update_state_from(&self, from: Option<State>, new: State, config: &Config) -> bool {
        // Atomically swap state to new, return if from doesn't match
        let old = State::from_u8(match from {
            Some(from) => match self.state.compare_exchange(
                from.to_u8(),
                new.to_u8(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(old) => old,
                Err(_) => return false,
            },
            None => self.state.swap(new.to_u8(), Ordering::Relaxed),
        });

        // State must be changed
        if old == new {
            return false;
        }

        trace!("Change server state from {:?} to {:?}", old, new);

        // Update kill at time for starting/stopping state
        *self.kill_at.write().unwrap() = match new {
            State::Starting if config.time.start_timeout > 0 => {
                Some(Instant::now() + Duration::from_secs(config.time.start_timeout as u64))
            }
            State::Stopping if config.time.stop_timeout > 0 => {
                Some(Instant::now() + Duration::from_secs(config.time.stop_timeout as u64))
            }
            _ => None,
        };

        // Online/offline messages
        match new {
            State::Started => info!(target: "lazymc::monitor", "Server is now online"),
            State::Stopped => info!(target: "lazymc::monitor", "Server is now sleeping"),
            _ => {}
        }

        // If Starting -> Started, update active time and keep it online for configured time
        if old == State::Starting && new == State::Started {
            self.update_last_active();
            self.keep_online_for(Some(config.time.min_online_time));
        }

        true
    }

    /// Update status as polled from the server.
    ///
    /// This updates various other internal things depending on the current state and the given
    /// status.
    pub fn update_status(&self, config: &Config, status: Option<ServerStatus>) {
        // Update state based on curren
        match (self.state(), &status) {
            (State::Stopped | State::Starting, Some(_)) => {
                self.update_state(State::Started, config);
            }
            (State::Started, None) => {
                self.update_state(State::Stopped, config);
            }
            _ => {}
        }

        // Update last status if known
        if let Some(status) = status {
            // Update last active time if there are online players
            if status.players.online > 0 {
                self.update_last_active();
            }

            self.status.write().unwrap().replace(status);
        }
    }

    /// Try to start the server.
    ///
    /// Does nothing if currently not in stopped state.
    pub fn start(config: Arc<Config>, server: Arc<Server>, username: Option<String>) -> bool {
        // Must set state from stopped to starting
        if !server.update_state_from(Some(State::Stopped), State::Starting, &config) {
            return false;
        }

        // Log starting message
        match username {
            Some(username) => info!(target: "lazymc", "Starting server for '{}'...", username),
            None => info!(target: "lazymc", "Starting server..."),
        }

        // Invoke server command in separate task
        tokio::spawn(invoke_server_cmd(config, server).map(|_| ()));
        true
    }

    /// Stop running server.
    ///
    /// This requires the server PID to be known.
    #[allow(unused_variables)]
    pub async fn stop(&self, config: &Config) -> bool {
        // We must have a running process
        let has_process = self.pid.lock().unwrap().is_some();
        if !has_process {
            debug!(target: "lazymc", "Tried to stop server, while no PID is known");
            return false;
        }

        // Try to stop through RCON if started
        #[cfg(feature = "rcon")]
        if self.state() == State::Started && stop_server_rcon(config, self).await {
            return true;
        }

        // Try to stop through signal
        #[cfg(unix)]
        if stop_server_signal(config, self) {
            return true;
        }

        warn!(target: "lazymc", "Failed to stop server, no more suitable stopping method to use");
        false
    }

    /// Force kill running server.
    ///
    /// This requires the server PID to be known.
    pub async fn force_kill(&self) -> bool {
        if let Some(pid) = *self.pid.lock().unwrap() {
            return os::force_kill(pid);
        }
        false
    }

    /// Decide whether the server should sleep.
    ///
    /// Always returns false if it is currently not online.
    pub fn should_sleep(&self, config: &Config) -> bool {
        // Server must be online
        if self.state() != State::Started {
            return false;
        }

        // Never sleep if players are online
        let players_online = self
            .status
            .read()
            .unwrap()
            .as_ref()
            .map(|status| status.players.online > 0)
            .unwrap_or(false);
        if players_online {
            trace!(target: "lazymc", "Not sleeping because players are online");
            return false;
        }

        // Don't sleep when keep online until isn't expired
        let keep_online = self
            .keep_online_until
            .read()
            .unwrap()
            .map(|i| i >= Instant::now())
            .unwrap_or(false);
        if keep_online {
            trace!(target: "lazymc", "Not sleeping because of keep online");
            return false;
        }

        // Last active time must have passed sleep threshold
        if let Some(last_idle) = self.last_active.read().unwrap().as_ref() {
            return last_idle.elapsed() >= Duration::from_secs(config.time.sleep_after as u64);
        }

        false
    }

    /// Decide whether to force kill the server process.
    pub fn should_kill(&self) -> bool {
        self.kill_at
            .read()
            .unwrap()
            .map(|t| t <= Instant::now())
            .unwrap_or(false)
    }

    /// Read last known server status.
    pub fn status(&self) -> RwLockReadGuard<Option<ServerStatus>> {
        self.status.read().unwrap()
    }

    /// Update the last active time.
    fn update_last_active(&self) {
        self.last_active.write().unwrap().replace(Instant::now());
    }

    /// Force the server to be online for the given number of seconds.
    fn keep_online_for(&self, duration: Option<u32>) {
        *self.keep_online_until.write().unwrap() = duration
            .filter(|d| *d > 0)
            .map(|d| Instant::now() + Duration::from_secs(d as u64));
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(State::Stopped.to_u8()),
            pid: Default::default(),
            status: Default::default(),
            last_active: Default::default(),
            keep_online_until: Default::default(),
            kill_at: Default::default(),
        }
    }
}

/// Invoke server command, store PID and wait for it to quit.
pub async fn invoke_server_cmd(
    config: Arc<Config>,
    state: Arc<Server>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build command
    let args = shlex::split(&config.server.command).expect("invalid server command");
    let mut cmd = Command::new(&args[0]);
    cmd.args(args.iter().skip(1));
    cmd.kill_on_drop(true);

    // Set working directory
    if let Some(ref dir) = config.server.directory {
        cmd.current_dir(dir);
    }

    // Spawn process
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            error!(target: "lazymc", "Failed to start server process through command");
            return Err(err.into());
        }
    };

    // Remember PID
    state
        .pid
        .lock()
        .unwrap()
        .replace(child.id().expect("unknown server PID"));

    // Wait for process to exit, handle status
    let crashed = match child.wait().await {
        Ok(status) if status.success() => {
            debug!(target: "lazymc", "Server process stopped successfully ({})", status);
            false
        }
        Ok(status) => {
            warn!(target: "lazymc", "Server process stopped with error code ({})", status);
            state.state() == State::Started
        }
        Err(err) => {
            error!(target: "lazymc", "Failed to wait for server process to quit: {}", err);
            error!(target: "lazymc", "Assuming server quit, cleaning up...");
            false
        }
    };

    // Forget server PID
    state.pid.lock().unwrap().take();

    // Give server a little more time to quit forgotten threads
    time::sleep(SERVER_QUIT_COOLDOWN).await;

    // Set server state to stopped
    state.update_state(State::Stopped, &config);

    // Restart on crash
    if crashed && config.server.wake_on_crash {
        warn!(target: "lazymc", "Server crashed, restarting...");
        Server::start(config, state, None);
    }

    Ok(())
}

/// Stop server through RCON.
#[cfg(feature = "rcon")]
async fn stop_server_rcon(config: &Config, server: &Server) -> bool {
    use crate::mc::rcon::Rcon;

    // RCON must be enabled
    if !config.rcon.enabled {
        trace!(target: "lazymc", "Not using RCON to stop server, disabled in config");
        return false;
    }

    // RCON address
    let mut addr = config.server.address;
    addr.set_port(config.rcon.port);
    let addr = addr.to_string();

    // Create RCON client
    let mut rcon = match Rcon::connect(&addr, &config.rcon.password).await {
        Ok(rcon) => rcon,
        Err(err) => {
            error!(target: "lazymc", "Failed to RCON server to sleep: {}", err);
            return false;
        }
    };

    // Invoke stop
    if let Err(err) = rcon.cmd("stop").await {
        error!(target: "lazymc", "Failed to invoke stop through RCON: {}", err);
        return false;
    }

    // Set server to stopping state
    // TODO: set before stop command, revert state on failure
    server.update_state(State::Stopping, config);

    true
}

/// Stop server by sending SIGTERM signal.
///
/// Only available on Unix.
#[cfg(unix)]
fn stop_server_signal(config: &Config, server: &Server) -> bool {
    // Grab PID
    let pid = match *server.pid.lock().unwrap() {
        Some(pid) => pid,
        None => {
            debug!(target: "lazymc", "Could not send stop signal to server process, PID unknown");
            return false;
        }
    };

    // Send kill signal
    if !crate::os::kill_gracefully(pid) {
        error!(target: "lazymc", "Failed to send stop signal to server process");
        return false;
    }

    // Update from starting/started to stopping
    server.update_state_from(Some(State::Starting), State::Stopping, config);
    server.update_state_from(Some(State::Started), State::Stopping, config);

    true
}
