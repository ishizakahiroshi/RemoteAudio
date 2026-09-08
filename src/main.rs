// No console window in a release build. Debug keeps one, because `cargo run`
// with nowhere to print is a miserable way to develop — and the attribute only
// has to be right in the artifact people actually install.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod about;
#[cfg(windows)]
mod assets;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod auth;
mod autostart;
#[cfg(windows)]
mod callout;
#[cfg(windows)]
mod config;
#[cfg(windows)]
mod history;
#[cfg(windows)]
mod lang;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod server;
#[cfg(windows)]
mod splash;
#[cfg(windows)]
mod supervisor;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod welcome;

/// What a bare `RemoteAudio.exe` does.
///
/// `supervise` and not `serve` since v0.2: with no console window, a
/// double-clicked exe that only starts a server puts *nothing* on screen. The
/// supervisor shows a notification-area icon, so the app is visibly running and
/// reachable from the first click.
const DEFAULT_COMMAND: &str = "supervise";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or(DEFAULT_COMMAND);

    // Must happen before the first `println!`: a release build starts with no
    // console at all, and anything printed before the attach is gone for good.
    #[cfg(windows)]
    if prints_to_a_terminal(cmd) {
        attach_parent_console();
    }

    match cmd {
        "--help" | "-h" | "help" => banner(),
        "--install-autostart" => run_install_autostart(),
        "--uninstall-autostart" => run_uninstall_autostart(),
        // Internal: the elevated half of the two commands above. Deliberately
        // absent from the banner — `elevate_self` is the only caller.
        "--firewall-install" => run_firewall_helper(args.get(2).map(String::as_str)),
        "--firewall-uninstall" => run_firewall_helper_uninstall(),
        "serve" => {
            let no_open = args.iter().skip(2).any(|a| a == "--no-open");
            run_serve(no_open);
        }
        "supervise" => run_supervise(),
        "list" => run_list(),
        "set" => run_set(args.get(2).map(String::as_str)),
        "share" => run_share(),
        "token" => run_token(&args),
        "setup" | "--setup" => run_setup(),
        // A leading flag with no subcommand still means "serve", the way it did
        // in v0.1 (`RemoteAudio.exe --no-open`). A *bare* `RemoteAudio.exe` no longer
        // lands here — see `DEFAULT_COMMAND`.
        _ if cmd == "serve" || cmd.starts_with("--") || cmd.is_empty() => {
            let no_open = args.iter().skip(1).any(|a| a == "--no-open");
            run_serve(no_open);
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!();
            banner();
            std::process::exit(2);
        }
    }
}

/// Whether `cmd` exists to print something at a person.
///
/// The exceptions matter more than the rule. `serve` and `supervise` outlive the
/// shell that started them, and a resident process writing into a prompt
/// somebody is still typing at is worse than staying quiet — `RemoteAudio share`
/// is how you get the URLs back afterwards. The firewall helpers run elevated on
/// another desktop with no console to borrow, and answer through their exit
/// code. Everything else, including a typo, gets a console: an error message
/// nobody can read is the same as no error at all.
#[cfg(windows)]
fn prints_to_a_terminal(cmd: &str) -> bool {
    match cmd {
        "serve" | "supervise" | "--firewall-install" | "--firewall-uninstall" => false,
        "--help" | "--setup" | "--install-autostart" | "--uninstall-autostart" => true,
        // Every other `--flag`, and an empty argument, falls through to the
        // server in `main`.
        other => !other.starts_with("--") && !other.is_empty(),
    }
}

/// Borrow the console of whoever launched us, if there is one.
///
/// Deliberately never `AllocConsole`: a window popping up would undo the very
/// thing `windows_subsystem = "windows"` is here for. Launched from Explorer,
/// this is a no-op and the command runs silently.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Point one standard handle at the console — but only if it is still empty.
    ///
    /// `RemoteAudio list > out.txt` arrives with a real file handle already in
    /// place. Overwriting that would put the output on screen and leave the file
    /// empty, which is a worse regression than the one this whole function is
    /// fixing.
    unsafe fn bind(id: STD_HANDLE, device: PCWSTR) {
        if GetStdHandle(id).is_ok() {
            return;
        }
        let opened = CreateFileW(
            device,
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            HANDLE::default(),
        );
        if let Ok(handle) = opened {
            let _ = SetStdHandle(id, handle);
        }
    }

    unsafe {
        // Fails when there is no parent console, or when we somehow have one
        // already. Either way there is nothing left to do.
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        // Attaching gives us the console; it does not promise usable standard
        // handles, and a GUI-subsystem process starts without any.
        bind(STD_INPUT_HANDLE, w!("CONIN$"));
        bind(STD_OUTPUT_HANDLE, w!("CONOUT$"));
        bind(STD_ERROR_HANDLE, w!("CONOUT$"));
    }
}

fn banner() {
    println!(
        "Remote Audio v{} (Windows 11 host agent)",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("Subcommands:");
    println!(
        "  RemoteAudio               run in the notification area, keeping the server alive (default)"
    );
    println!("  RemoteAudio supervise     same as above (explicit)");
    println!("  RemoteAudio serve         run one server in the foreground, no tray icon");
    println!("  RemoteAudio serve --no-open   skip auto-opening the browser");
    println!(
        "  RemoteAudio setup         interactive config wizard (bind / token / sort / recovery)"
    );
    println!("  RemoteAudio list          list playback endpoints + current defaults");
    println!("  RemoteAudio set <id>      switch default (Console/Multimedia/Communications)");
    println!("  RemoteAudio share         print the LAN URLs with the token in full");
    println!(
        "  RemoteAudio token ...     manage LAN tokens (list [--show] / add <name> / revoke <name|token>)"
    );
    println!("  RemoteAudio.exe --install-autostart     start server at logon (HKCU Run)");
    println!("  RemoteAudio.exe --uninstall-autostart  remove the Remote Audio logon entry");
    println!("  RemoteAudio.exe --help        show this help");
    println!();
    println!("Repository: {}", env!("CARGO_PKG_REPOSITORY"));
}

/// Register the logon entry, then ask once for the elevation the firewall rule
/// needs. The registry half never depends on the elevated half: declining UAC
/// costs the LAN rule, not the autostart.
#[cfg(windows)]
fn run_install_autostart() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[fatal] cannot determine current executable: {e}");
            std::process::exit(1);
        }
    };
    let installed = match autostart::install(&exe) {
        Ok(installed) => installed,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            eprintln!("[unsupported] {e}");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("[fatal] cannot install autostart: {e}");
            std::process::exit(1);
        }
    };

    let port = configured_port();

    // The Store build shares no step with the portable one here: both halves are
    // declared in the package manifest and installed by Windows, so there is
    // nothing to do and no UAC prompt to raise. The one case left is a port that
    // the manifest's firewall rule does not cover.
    let command = match installed {
        autostart::Installed::PackagedStartupTask => {
            println!("Remote Audio is installed from the Microsoft Store.");
            println!("  autostart: already on — the package declares it, and Windows starts");
            println!("             Remote Audio at sign-in. Turn it off in Task Manager →");
            println!("             Startup apps → Remote Audio.");
            if port == autostart::PACKAGED_FIREWALL_PORT {
                println!("  firewall:  already open — the package declares inbound TCP {port} on");
                println!("             private and domain networks.");
            } else {
                // The manifest cannot read config.toml, so its rule is pinned to
                // the default port. Somebody who moved the port off it has a
                // server nothing can reach, and no way to guess why.
                println!("  firewall:  NOT open for this port. The package only declares TCP");
                println!(
                    "             {}, and `RemoteAudio setup` moved the server to {port}.",
                    autostart::PACKAGED_FIREWALL_PORT
                );
                println!("             Add the rule once from an elevated prompt:");
                println!(
                    "               {}",
                    autostart::firewall_command_hint(None, port)
                );
            }
            return;
        }
        autostart::Installed::RunValue(command) => command,
    };

    println!("Remote Audio autostart installed.");
    println!(r"  registry: HKCU\{}", autostart::RUN_KEY_PATH);
    println!("  value:    {}", autostart::VALUE_NAME);
    println!("  command:  {command}");

    match autostart::elevate_self(&["--firewall-install", &port.to_string()]) {
        autostart::Elevation::Done => {
            println!("  firewall: inbound TCP {port} allowed on private and domain networks.");
        }
        autostart::Elevation::Declined => {
            println!("  firewall: skipped — the elevation prompt was dismissed.");
            println!("            Other machines cannot reach this host until an inbound");
            println!("            rule exists. Add it later from an elevated prompt:");
            println!(
                "              {}",
                autostart::firewall_command_hint(Some(&exe), port)
            );
        }
        autostart::Elevation::Failed(reason) => {
            println!("  firewall: not added ({reason}).");
            println!("            Add it from an elevated prompt:");
            println!(
                "              {}",
                autostart::firewall_command_hint(Some(&exe), port)
            );
        }
    }

    println!("  note:     run this command again if the exe is moved.");
}

#[cfg(windows)]
fn run_uninstall_autostart() {
    if autostart::packaged() {
        println!("Remote Audio is installed from the Microsoft Store.");
        println!("  autostart: this command cannot switch it off — Windows owns it.");
        println!("             Task Manager → Startup apps → Remote Audio → Disable.");
        println!("  firewall:  remove the inbound rule from an elevated prompt:");
        println!(
            "               netsh advfirewall firewall delete rule name=\"{}\"",
            autostart::FIREWALL_RULE_NAME
        );
        return;
    }

    match autostart::uninstall() {
        Ok(()) => println!("Remote Audio autostart removed (if it was registered)."),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            eprintln!("[unsupported] {e}");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("[fatal] cannot uninstall autostart: {e}");
            std::process::exit(1);
        }
    }

    match autostart::elevate_self(&["--firewall-uninstall"]) {
        autostart::Elevation::Done => println!("  firewall: inbound rule removed."),
        autostart::Elevation::Declined | autostart::Elevation::Failed(_) => {
            println!("  firewall: the inbound rule is still there — removing it needs");
            println!("            elevation. From an elevated prompt:");
            println!(
                "              netsh advfirewall firewall delete rule name=\"{}\"",
                autostart::FIREWALL_RULE_NAME
            );
        }
    }
}

/// The port the resident server will actually listen on. Reading the config
/// here (rather than assuming the default) is what keeps the firewall rule and
/// the listener in agreement after someone runs `RemoteAudio setup`.
#[cfg(windows)]
fn configured_port() -> u16 {
    match config::load_or_init(&config::default_config_path()) {
        Ok((cfg, _)) => cfg.server.port,
        Err(_) => config::Config::default().server.port,
    }
}

/// Elevated half of `--install-autostart`. Started by `elevate_self`, never by
/// a person: the release build has no console, so the exit code is the only
/// channel back to the caller.
#[cfg(windows)]
fn run_firewall_helper(port: Option<&str>) {
    let Some(port) = port.and_then(|p| p.parse::<u16>().ok()) else {
        eprintln!("usage: RemoteAudio.exe --firewall-install <port>");
        std::process::exit(2);
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("cannot determine current executable: {e}");
            std::process::exit(1);
        }
    };
    // `None` in a packaged build: the exe path under `%ProgramFiles%\
    // WindowsApps\` is stamped with the package version, so a program-scoped
    // rule would stop matching at the next Store update.
    let program = (!autostart::packaged()).then_some(exe.as_path());
    if let Err(e) = autostart::firewall_install(program, port) {
        eprintln!("cannot add the firewall rule: {e}");
        std::process::exit(1);
    }
    println!("firewall rule installed for TCP {port}");
}

/// Elevated half of `--uninstall-autostart`.
#[cfg(windows)]
fn run_firewall_helper_uninstall() {
    if let Err(e) = autostart::firewall_uninstall() {
        eprintln!("cannot remove the firewall rule: {e}");
        std::process::exit(1);
    }
    println!("firewall rule removed");
}

#[cfg(windows)]
fn run_serve(no_open: bool) {
    use std::sync::Arc;

    let config_path = config::default_config_path();
    let history_path = history::default_history_path();

    let (cfg, generated) = match config::load_or_init(&config_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "[fatal] cannot load config at {}: {}",
                config_path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let history_state = match history::load(&history_path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!(
                "[warn] cannot load history at {}: {} (starting empty)",
                history_path.display(),
                e
            );
            history::History::default()
        }
    };

    // Resolve bind + port before the runtime starts: a hand-edited `bind = "::1"`
    // or `port = 0` used to surface as an opaque parse failure (or, worse, an
    // ephemeral port the banner and share URLs then reported wrong).
    let addr = match cfg.socket_addr() {
        Ok(addr) => addr,
        Err(reason) => {
            eprintln!(
                "[fatal] invalid [server] settings in {}: {reason}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };

    let host_url = net::build_host_url(&cfg);
    let share_entries = net::build_share_entries(&cfg, cfg.share_token());
    print_startup_banner(
        &cfg,
        &config_path,
        generated,
        &host_url,
        &share_entries,
        no_open,
    );

    // Auto-open the loopback URL in the default browser on every manual `serve`.
    // Suppress with `--no-open` (used by the autostart entry so logon doesn't
    // pop a browser tab).
    if !no_open {
        open_url(&host_url);
    }

    let allowed_hosts = net::build_allowed_hosts(&cfg);
    // Tokens are the one setting that must not be a startup snapshot: the CLI
    // edits config.toml from another process, and a revoke has to take effect
    // without a restart.
    let auth_state = auth::AuthState::new(config_path.clone(), cfg.auth.clone());
    let state = server::AppState {
        config: Arc::new(cfg),
        auth: Arc::new(auth_state),
        audio: server::AudioGate::new(),
        history: Arc::new(tokio::sync::Mutex::new(history_state)),
        history_path: Arc::new(history_path),
        allowed_hosts: Arc::new(allowed_hosts),
        supervised: supervisor::is_supervised(),
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[fatal] tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = rt.block_on(server::serve(state, addr)) {
        eprintln!("[fatal] server: {e}");
        std::process::exit(1);
    }
}

/// Run the resident supervisor: one `serve --no-open` child, restarted on a
/// bounded backoff. This is what the logon entry points at from v0.2 on.
#[cfg(windows)]
fn run_supervise() {
    // Before anything else, and before the tray icon exists. Only one process
    // can bind the port, so a second supervisor's child dies on every attempt
    // until the restart budget runs out — leaving a dead notification-area icon
    // beside the working one and no clue why.
    let config_path = config::default_config_path();
    let cfg = match config::load_or_init(&config_path) {
        Ok((cfg, _)) => cfg,
        Err(e) => {
            eprintln!("[fatal] cannot load config: {e}");
            std::process::exit(1);
        }
    };

    let _instance = match supervisor::acquire_instance_lock() {
        Some(lock) => lock,
        None => {
            welcome::already_running(&lang::Strings::load(&cfg.tray.ui_language));
            return;
        }
    };

    let (handle, joiner) = match supervisor::start(cfg.tray.auto_restart) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[fatal] cannot start the supervisor: {e}");
            std::process::exit(1);
        }
    };
    supervisor::install_console_ctrl_handler();
    println!("RemoteAudio supervisor running. Ctrl+C stops it and the server together.");

    // The tray owns the main thread from here: Windows delivers its messages
    // only to the thread that created the window. Losing the icon is not worth
    // losing the supervision, so a failure here degrades to a headless run that
    // Ctrl+C still ends.
    if let Err(e) = tray::run(handle.clone()) {
        eprintln!("[warn] no notification-area icon ({e}); running without a tray.");
    } else {
        handle.send(supervisor::Request::Quit);
    }

    // `handle` stays in scope for the whole join: dropping the last handle is
    // the monitor's signal that nobody can send it requests any more.
    let _ = joiner.join();
}

#[cfg(windows)]
fn print_startup_banner(
    cfg: &config::Config,
    config_path: &std::path::Path,
    generated: bool,
    host_url: &str,
    share_entries: &[net::ShareEntry],
    no_open: bool,
) {
    // Splash art (ANSI colored). Silently downgrades to plain when NO_COLOR is set.
    print!("{}", splash::render());
    println!();
    println!("  config      {}", config_path.display());
    println!("  bind        {}:{}", cfg.server.bind, cfg.server.port);
    if generated {
        if let Some(tok) = cfg.share_token() {
            println!("  token       {tok}   <-- first-run token (saved; not shown again)");
        }
    } else {
        let active = cfg.auth.active_count();
        println!("  token       {active} active   (RemoteAudio token list to view/manage)");
    }
    println!(
        "  device_sort {}",
        match cfg.audio.device_sort {
            config::SortPolicy::State => "state",
            config::SortPolicy::Name => "name",
            config::SortPolicy::Recent => "recent",
        }
    );

    let invalid_networks = cfg.invalid_networks();
    if !invalid_networks.is_empty() {
        println!();
        println!("  [WARNING] allowed_networks contains entries that are not valid networks:");
        for entry in &invalid_networks {
            println!("              {entry:?}");
        }
        println!("            They match nothing, so every LAN client outside the remaining");
        println!("            entries is refused. Use CIDR (\"203.0.113.0/24\") or a bare");
        println!("            address (\"203.0.113.20\").");
    }

    if cfg.lan_exposed() {
        if !cfg.auth.require_token {
            println!();
            println!("  [WARNING] require_token = false while bound to the LAN — the API is");
            println!("            OPEN to every machine that can reach this host. Set");
            println!("            require_token = true (or bind = 127.0.0.1) unless intentional.");
        }
        if generated {
            println!();
            println!("  [firewall] Windows may prompt to allow Remote Audio on the LAN — pick");
            println!("             \"Private networks\" the first time.");
        }
    } else {
        println!();
        println!(
            "  [note] LAN is DISABLED (bind = {}). This host will not answer other",
            cfg.server.bind
        );
        println!("         machines. Run `RemoteAudio setup` to re-enable LAN.");
    }

    println!();
    println!("  Open on this host           :");
    println!("    {host_url}");

    if !share_entries.is_empty() {
        println!();
        println!("  Open on your other machine (guest Win11 / phone / VM) - token embedded:");
        for e in share_entries {
            let tag = if e.virtual_iface {
                "  [virtual switch]"
            } else {
                ""
            };
            // The token is masked on every start except the very first one. This
            // console is long-lived: it survives in scrollback, screen shares,
            // recordings and redirected logs, and it is re-printed at every logon
            // by the autostart entry. `RemoteAudio share` prints it in full when
            // it is actually needed.
            let url = if generated {
                e.url.clone()
            } else {
                mask_share_url(&e.url)
            };
            println!("    {}   ({}){}", url, e.interface, tag);
        }
        if !generated {
            println!();
            println!("    (token masked - run `RemoteAudio share` for the full URLs)");
        }
    }

    if !no_open {
        println!();
        println!("  (Opening the host URL in your browser now …)");
    }

    println!();
    println!("  Ctrl+C to stop.");
    println!();
}

/// Shorten a token for display: `ar_live_1a2b…9f8e`. Enough to tell two entries
/// apart in `token list`, not enough to authenticate with.
#[cfg(windows)]
fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 16 {
        // Too short to reveal a prefix and a suffix without giving away most of
        // it — a hand-added token can be any length.
        return "…".to_string();
    }
    let head: String = chars[..12].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Mask the `#t=<token>` fragment of a share URL, leaving the address readable.
#[cfg(windows)]
fn mask_share_url(url: &str) -> String {
    match url.split_once("#t=") {
        Some((base, token)) => format!("{base}#t={}", mask_token(token)),
        None => url.to_string(),
    }
}

/// Print the LAN URLs with the token in full. Explicit command = explicit
/// consent to put the token on screen, which the startup banner no longer does.
#[cfg(windows)]
fn run_share() {
    let path = config::default_config_path();
    let (cfg, _) = match config::load_or_init(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cannot load config: {e}");
            std::process::exit(1);
        }
    };

    if !cfg.lan_exposed() {
        println!(
            "LAN is disabled (bind = {}). Run `RemoteAudio setup` to enable it.",
            cfg.server.bind
        );
        return;
    }
    let entries = net::build_share_entries(&cfg, cfg.share_token());
    if entries.is_empty() {
        println!("No LAN address found on this host, so there is nothing to share yet.");
        return;
    }

    println!("Open on your other machine (guest Win11 / phone / VM).");
    println!("These URLs contain a live token - treat them like a password:");
    println!();
    for e in entries {
        let tag = if e.virtual_iface {
            "  [virtual switch]"
        } else {
            ""
        };
        println!("  {}   ({}){}", e.url, e.interface, tag);
    }
}

/// Fire-and-forget open of `url` in the default browser (Windows only).
#[cfg(windows)]
fn open_url(url: &str) {
    // Strip trailing "  (label)" the printer adds for readability.
    let clean = url.split_whitespace().next().unwrap_or(url);
    // `cmd /C start "" <url>` picks the OS default handler for http:// URLs.
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", clean])
        .spawn();
}

#[cfg(windows)]
fn run_setup() {
    use std::io::Write;

    let path = config::default_config_path();
    let (mut cfg, _) = match config::load_or_init(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cannot load config: {e}");
            std::process::exit(1);
        }
    };

    println!("RemoteAudio setup — current config: {}", path.display());
    println!();

    // 1. LAN mode
    let currently_lan = cfg.lan_exposed();
    println!(
        "1) Network mode  (currently: {})",
        if currently_lan {
            "LAN exposed (0.0.0.0)"
        } else {
            "host-only (127.0.0.1)"
        }
    );
    print!(
        "   Switch to {}? [y/N]: ",
        if currently_lan {
            "host-only"
        } else {
            "LAN exposed"
        }
    );
    std::io::stdout().flush().ok();
    if yes() {
        cfg.server.bind = if currently_lan {
            "127.0.0.1".to_string()
        } else {
            "0.0.0.0".to_string()
        };
        println!("   -> bind = {}", cfg.server.bind);
    }
    println!();

    // 2. Reissue token (revokes ALL current tokens, issues one fresh "default")
    println!("2) Reissue auth token? (revokes ALL current tokens)");
    print!("   Old clients will need the new URL. [y/N]: ");
    std::io::stdout().flush().ok();
    if yes() {
        for t in &mut cfg.auth.tokens {
            t.revoked = true;
        }
        let token = config::add_named_token(&mut cfg, "default")
            .expect("the built-in default token name is valid");
        println!("   -> new token: {token}");
    }
    println!();

    // 3. Sort policy
    println!("3) Device sort");
    println!("   1) state (active first)      2) name          3) recent (most recently used)");
    print!(
        "   Choose [current: {}] (enter to keep): ",
        match cfg.audio.device_sort {
            config::SortPolicy::State => "1) state",
            config::SortPolicy::Name => "2) name",
            config::SortPolicy::Recent => "3) recent",
        }
    );
    std::io::stdout().flush().ok();
    let line = read_line();
    match line.trim() {
        "1" => cfg.audio.device_sort = config::SortPolicy::State,
        "2" => cfg.audio.device_sort = config::SortPolicy::Name,
        "3" => cfg.audio.device_sort = config::SortPolicy::Recent,
        _ => {}
    }
    println!("   -> device_sort = {:?}", cfg.audio.device_sort);
    println!();

    // Port
    println!("4) Port  (currently: {})", cfg.server.port);
    print!("   Change? [y/N]: ");
    std::io::stdout().flush().ok();
    if yes() {
        print!("   New port (1024–65535): ");
        std::io::stdout().flush().ok();
        if let Ok(n) = read_line().trim().parse::<u16>() {
            if n >= 1024 {
                cfg.server.port = n;
                println!("   -> port = {n}");
            } else {
                println!("   port must be >= 1024; keeping {}", cfg.server.port);
            }
        }
    }
    println!();

    // 5. Crash recovery
    println!(
        "5) Automatic server recovery (currently: {})",
        if cfg.tray.auto_restart { "on" } else { "off" }
    );
    print!(
        "   Turn automatic recovery {}? [y/N]: ",
        if cfg.tray.auto_restart { "off" } else { "on" }
    );
    std::io::stdout().flush().ok();
    if yes() {
        cfg.tray.auto_restart = !cfg.tray.auto_restart;
        println!(
            "   -> automatic recovery = {}",
            if cfg.tray.auto_restart { "on" } else { "off" }
        );
    }
    println!();

    if let Err(e) = config::save(&path, &cfg) {
        eprintln!("failed to save config: {e}");
        std::process::exit(1);
    }
    println!("Saved. Restart RemoteAudio for changes to take effect.");
}

#[cfg(windows)]
fn yes() -> bool {
    matches!(
        read_line().trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )
}

#[cfg(windows)]
fn read_line() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s
}

#[cfg(windows)]
fn run_list() {
    match audio::list_devices() {
        Ok(devs) => {
            println!("{:<3} {:<9} {:<3} {:<40} id", "#", "state", "def", "name");
            println!("{}", "-".repeat(120));
            for (i, d) in devs.iter().enumerate() {
                let def = default_marker(d);
                println!(
                    "{:<3} {:<9} {:<3} {:<40} {}",
                    i,
                    d.state.as_str(),
                    def,
                    truncate(&sanitize_terminal_text(&d.name), 40),
                    sanitize_terminal_text(&d.id)
                );
            }
        }
        Err(e) => {
            eprintln!("list failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn default_marker(d: &audio::AudioDevice) -> String {
    let mut s = String::new();
    if d.is_default_console {
        s.push('C');
    }
    if d.is_default_multimedia {
        s.push('M');
    }
    if d.is_default_communications {
        s.push('X');
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Keep device names, token labels and child output from changing terminal
/// state or injecting additional lines. The visible replacement also makes
/// hand-edited control characters obvious instead of silently joining words.
pub(crate) fn sanitize_terminal_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

#[cfg(windows)]
fn run_set(id: Option<&str>) {
    let Some(id) = id else {
        eprintln!("usage: RemoteAudio set <device_id>");
        std::process::exit(2);
    };
    match audio::set_default(id) {
        Ok(()) => println!("OK: default endpoint switched to {id}"),
        Err(e) => {
            eprintln!("set failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn run_token(args: &[String]) {
    let sub = args.get(2).map(String::as_str).unwrap_or("list");
    let path = config::default_config_path();
    let (mut cfg, _) = match config::load_or_init(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cannot load config: {e}");
            std::process::exit(1);
        }
    };

    match sub {
        "list" => {
            // Masked by default: this listing is the one command people run over
            // a shared screen, and the values are long-lived credentials.
            let reveal = args.iter().skip(3).any(|a| a == "--show");
            println!("{:<20} {:<8} {:<12} token", "name", "state", "created");
            println!("{}", "-".repeat(96));
            for t in &cfg.auth.tokens {
                let st = if t.revoked { "revoked" } else { "active" };
                let created = if t.created_at == 0 {
                    "-".to_string()
                } else {
                    t.created_at.to_string()
                };
                let shown = if reveal {
                    t.token.clone()
                } else {
                    mask_token(&t.token)
                };
                println!(
                    "{:<20} {:<8} {:<12} {}",
                    truncate(&sanitize_terminal_text(&t.name), 20),
                    st,
                    created,
                    sanitize_terminal_text(&shown)
                );
            }
            if !reveal {
                println!();
                println!("tokens are masked - `RemoteAudio token list --show` prints them in full");
            }
        }
        "add" => {
            let Some(name) = args.get(3) else {
                eprintln!("usage: RemoteAudio token add <name>");
                std::process::exit(2);
            };
            let token = match config::add_named_token(&mut cfg, name) {
                Ok(token) => token,
                Err(e) => {
                    eprintln!("cannot add token: {e}");
                    std::process::exit(2);
                }
            };
            if let Err(e) = config::save(&path, &cfg) {
                eprintln!("failed to save config: {e}");
                std::process::exit(1);
            }
            println!("added token '{}':", sanitize_terminal_text(name.trim()));
            println!("  {token}");
            println!();
            println!("A running server picks this up within a second - no restart needed.");
        }
        "revoke" => {
            let Some(raw_target) = args.get(3) else {
                eprintln!("usage: RemoteAudio token revoke <name> | --by-value <token>");
                std::process::exit(2);
            };
            let (target, by_value) = if raw_target == "--by-value" {
                let Some(value) = args.get(4) else {
                    eprintln!("usage: RemoteAudio token revoke --by-value <token>");
                    std::process::exit(2);
                };
                (value.as_str(), true)
            } else {
                if raw_target.starts_with("ar_live_") {
                    eprintln!(
                        "token values require the explicit form: RemoteAudio token revoke --by-value <token>"
                    );
                    std::process::exit(2);
                }
                (raw_target.trim(), false)
            };
            if !by_value && target.is_empty() {
                eprintln!("token name must not be empty");
                std::process::exit(2);
            }
            let n = if by_value {
                config::revoke_token_value(&mut cfg, target)
            } else {
                config::revoke_named_token(&mut cfg, target)
            };
            if n == 0 {
                if by_value {
                    eprintln!("no active token matched the supplied value");
                } else {
                    eprintln!(
                        "no active token matched name '{}'",
                        sanitize_terminal_text(target)
                    );
                }
                std::process::exit(1);
            }
            // Never leave the server with zero usable tokens: reissue if the
            // last active token was just revoked.
            let reissued = if !cfg.auth.tokens.iter().any(|t| !t.revoked) {
                Some(
                    config::add_named_token(&mut cfg, "default")
                        .expect("the built-in default token name is valid"),
                )
            } else {
                None
            };
            // Report only after the write succeeds: "revoked" printed ahead of a
            // failed save is exactly the lie that makes a leaked token look dead.
            if let Err(e) = config::save(&path, &cfg) {
                eprintln!("failed to save config: {e}");
                eprintln!("nothing was revoked - the token(s) are still valid");
                std::process::exit(1);
            }
            match reissued {
                Some(token) => {
                    println!(
                        "revoked {n}; that was the last active token — issued a new 'default':"
                    );
                    println!("  {token}");
                }
                None if by_value => println!("revoked {n} token(s) by explicit value"),
                None => println!(
                    "revoked {n} token(s) named '{}'",
                    sanitize_terminal_text(target)
                ),
            }
            println!();
            println!("A running server stops accepting them within a second - no restart needed.");
        }
        other => {
            eprintln!("unknown token subcommand: {other}");
            eprintln!(
                "usage: RemoteAudio token [list [--show] | add <name> | revoke <name> | revoke --by-value <token>]"
            );
            std::process::exit(2);
        }
    }
}

// Non-Windows stubs. Signatures must match the calls in `main` exactly — the
// build for these targets is not covered by CI, so a mismatch here is only
// discovered by whoever tries to `cargo check` on macOS or Linux.
#[cfg(not(windows))]
fn run_serve(_no_open: bool) {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}
#[cfg(not(windows))]
fn run_supervise() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}
#[cfg(not(windows))]
fn run_install_autostart() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(2);
}
#[cfg(not(windows))]
fn run_uninstall_autostart() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(2);
}
#[cfg(not(windows))]
fn run_firewall_helper(_port: Option<&str>) {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(2);
}
#[cfg(not(windows))]
fn run_firewall_helper_uninstall() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(2);
}
#[cfg(not(windows))]
fn run_list() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}
#[cfg(not(windows))]
fn run_share() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}
#[cfg(not(windows))]
fn run_set(_: Option<&str>) {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}
#[cfg(not(windows))]
fn run_setup() {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}
#[cfg(not(windows))]
fn run_token(_: &[String]) {
    eprintln!("RemoteAudio only runs on Windows.");
    std::process::exit(1);
}

#[cfg(all(test, windows))]
mod tests {
    use super::{
        mask_share_url, mask_token, prints_to_a_terminal, sanitize_terminal_text, truncate,
        DEFAULT_COMMAND,
    };

    #[test]
    fn a_bare_launch_starts_the_resident_supervisor() {
        // The pairing that makes a double-clicked exe visible: the default lands
        // on the tray, and the tray never borrows a console.
        assert_eq!(DEFAULT_COMMAND, "supervise");
        assert!(!prints_to_a_terminal(DEFAULT_COMMAND));
    }

    #[test]
    fn only_the_printing_subcommands_borrow_a_console() {
        for cmd in [
            "--help",
            "-h",
            "help",
            "list",
            "set",
            "share",
            "token",
            "setup",
            "--setup",
            "--install-autostart",
            "--uninstall-autostart",
            // A typo has to be readable, or the user just sees nothing happen.
            "sevre",
        ] {
            assert!(prints_to_a_terminal(cmd), "{cmd} should reach a terminal");
        }

        for cmd in [
            "serve",
            "supervise",
            "--no-open",
            "--firewall-install",
            "--firewall-uninstall",
            "",
        ] {
            assert!(!prints_to_a_terminal(cmd), "{cmd:?} should stay quiet");
        }
    }

    #[test]
    fn mask_token_keeps_the_prefix_and_hides_the_secret() {
        let token = "ar_live_0123456789abcdef0123456789abcdef0123456789abcdef";
        let masked = mask_token(token);
        assert_eq!(masked, "ar_live_0123…cdef");
        assert!(!masked.contains("456789abcdef0123"));
        assert!(masked.len() < token.len());
    }

    #[test]
    fn mask_token_refuses_to_reveal_short_values() {
        for short in ["", "ar_live_", "ar_live_abcd", "0123456789abcdef"] {
            assert_eq!(mask_token(short), "…", "{short:?}");
        }
    }

    #[test]
    fn mask_share_url_hides_only_the_fragment() {
        let url = "http://203.0.113.5:17650/#t=ar_live_0123456789abcdef0123456789abcdef";
        let masked = mask_share_url(url);
        assert!(masked.starts_with("http://203.0.113.5:17650/#t=ar_live_0123…"));
        assert!(!masked.contains("89abcdef0123456789"));
        // A URL without a token fragment is passed through untouched.
        assert_eq!(
            mask_share_url("http://203.0.113.5:17650/"),
            "http://203.0.113.5:17650/"
        );
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("スピーカー", 40), "スピーカー");
        assert_eq!(truncate("スピーカー", 3), "スピ…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn terminal_control_characters_are_replaced() {
        assert_eq!(
            sanitize_terminal_text("name\x1b[31m\nnext\tpart"),
            "name�[31m�next�part"
        );
    }
}
