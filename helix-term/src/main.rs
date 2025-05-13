use anyhow::{Context, Error, Result};
use crossterm::terminal::{tty_file, winch_signal_receiver, Terminal};
use helix_loader::VERSION_AND_GIT_HASH;
use helix_stdx::env::set_current_working_dir;
use helix_stdx::socket::{read_fd, write_fd};
use helix_term::application::{Application, ApplicationClient, ClientInfo};
use helix_term::args::Args;
use helix_term::config::{Config, ConfigLoadError};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::{
    net::{UnixListener, UnixSocket},
    task::spawn_blocking,
};
use tokio_stream::StreamNotifyClose;
use tokio_util::io::SyncIoBridge;
use {signal_hook::consts::signal, signal_hook_tokio::Signals};

fn setup_logging(verbosity: u64) -> Result<()> {
    let mut base_config = fern::Dispatch::new();

    base_config = match verbosity {
        0 => base_config.level(log::LevelFilter::Warn),
        1 => base_config.level(log::LevelFilter::Info),
        2 => base_config.level(log::LevelFilter::Debug),
        _3_or_more => base_config.level(log::LevelFilter::Trace),
    };

    // Separate file config so we can include year, month and day in file logs
    let file_config = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.target(),
                record.level(),
                message
            ))
        })
        .chain(fern::log_file(helix_loader::log_file())?);

    base_config.chain(file_config).apply()?;

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse_args().context("could not parse arguments")?;

    // Help has a higher priority and should be handled separately.
    if args.display_help {
        print!(
            "\
{} {}
{}
{}

USAGE:
    hx [FLAGS] [files]...

ARGS:
    <files>...    Sets the input file to use, position can also be specified via file[:row[:col]]

FLAGS:
    -h, --help                     Prints help information
    --tutor                        Loads the tutorial
    --health [CATEGORY]            Checks for potential errors in editor setup
                                   CATEGORY can be a language or one of 'clipboard', 'languages'
                                   or 'all'. 'all' is the default if not specified.
    -g, --grammar {{fetch|build}}    Fetches or builds tree-sitter grammars listed in languages.toml
    -c, --config <file>            Specifies a file to use for configuration
    -v                             Increases logging verbosity each use for up to 3 times
    --log <file>                   Specifies a file to use for logging
                                   (default file: {})
    -V, --version                  Prints version information
    --vsplit                       Splits all given files vertically into different windows
    --hsplit                       Splits all given files horizontally into different windows
    -w, --working-dir <path>       Specify an initial working directory
    +N                             Open the first given file at line number N
",
            env!("CARGO_PKG_NAME"),
            VERSION_AND_GIT_HASH,
            env!("CARGO_PKG_AUTHORS"),
            env!("CARGO_PKG_DESCRIPTION"),
            helix_loader::default_log_file().display(),
        );
        std::process::exit(0);
    }

    if args.display_version {
        println!("helix {}", VERSION_AND_GIT_HASH);
        std::process::exit(0);
    }

    if args.health {
        helix_loader::initialize_config_file(args.config_file.clone());
        helix_loader::initialize_log_file(args.log_file.clone());

        if let Err(err) = helix_term::health::print_health(args.health_arg) {
            // Piping to for example `head -10` requires special handling:
            // https://stackoverflow.com/a/65760807/7115678
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(err.into());
            }
        }

        std::process::exit(0);
    }

    if args.fetch_grammars {
        helix_loader::initialize_config_file(args.config_file.clone());
        helix_loader::initialize_log_file(args.log_file.clone());
        helix_loader::grammar::fetch_grammars()?;
        std::process::exit(0);
    }

    if args.build_grammars {
        helix_loader::initialize_config_file(args.config_file.clone());
        helix_loader::initialize_log_file(args.log_file.clone());
        helix_loader::grammar::build_grammars(None)?;
        std::process::exit(0);
    }

    let client_info = ClientInfo::from_args(&args);

    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or(std::env::temp_dir());
    // Change directory to $XDG_RUNTIME_DIR. This has three purposes:
    // 1) Flush out any unknown dependencies on the server's current directory.
    // 2) Avoids issues associated with the server retaining a handle to the initial
    //    client's current directory (e.g. failure to unmount filesystems).
    // 3) Avoids filename length limits on the socket (UNIX_PATH_MAX in sockaddr_un).
    set_current_working_dir(runtime_dir)?;

    let socket_path = Path::new("helix");

    if args.foreground_server {
        let _ = std::fs::remove_file(&socket_path);
        std::process::exit(server(args, &socket_path).unwrap());
    }

    let client_sock = UnixStream::connect(&socket_path);
    let mut client_sock = client_sock.unwrap_or_else(|_e| {
        // SAFETY: At this point we are running single threaded so fork() won't lead to deadlocks.
        unsafe {
            let _ = std::fs::remove_file(&socket_path);
            let pid = libc::fork();
            if pid == 0 {
                use std::os::fd::AsRawFd;
                libc::setsid();
                {
                    let devnull = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/null")
                        .unwrap();
                    libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
                    libc::dup2(devnull.as_raw_fd(), libc::STDOUT_FILENO);
                    libc::dup2(devnull.as_raw_fd(), libc::STDERR_FILENO);
                }
                std::process::exit(server(args, &socket_path).unwrap());
            } else {
                // We could have the server notify us when it's ready, but it's easiest to poll.
                for _ in 0..50 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    if let Ok(client_sock) = UnixStream::connect(&socket_path) {
                        return client_sock;
                    }
                }
                panic!("Server did not start");
            }
        }
    });

    rmp_serde::encode::write(&mut client_sock, &client_info)?;
    write_fd(&client_sock, &tty_file()?)?;
    if client_info.has_stdin {
        write_fd(&client_sock, std::io::stdin())?;
    }
    client_sock.set_nonblocking(true)?;
    std::process::exit(client(client_sock)?);
}

#[tokio::main]
async fn client(socket: UnixStream) -> Result<i32> {
    let mut socket = tokio::net::UnixStream::from_std(socket)?;
    let mut signals = Signals::new([signal::SIGTSTP, signal::SIGCONT, signal::SIGWINCH])?;

    use futures_util::StreamExt;

    loop {
        tokio::select! {
            Some(signal) = signals.next() => {
                socket.write_u8(signal as u8).await?;
            }
            _ = socket.readable() => {
                let mut buf = [0];
                match socket.try_read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        return Ok(buf[0] as i32);
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            }
        }
    }

    Ok(0)
}

#[tokio::main]
async fn server(args: Args, socket_path: &Path) -> Result<i32> {
    helix_loader::initialize_config_file(args.config_file.clone());
    helix_loader::initialize_log_file(args.log_file.clone());

    setup_logging(args.verbosity).context("failed to initialize logging")?;

    let socket = UnixSocket::new_stream()?;
    socket.bind(socket_path)?;
    let listener = socket.listen(1024)?;

    // NOTE: Set the working directory early so the correct configuration is loaded. Be aware that
    // Application::new() depends on this logic so it must be updated if this changes.
    if let Some(path) = &args.working_directory {
        helix_stdx::env::set_current_working_dir(path)?;
    } else if let Some((path, _)) = args.files.first().filter(|p| p.0.is_dir()) {
        // If the first file is a directory, it will be the working directory unless -w was specified
        helix_stdx::env::set_current_working_dir(path)?;
    }

    let config = match Config::load_default() {
        Ok(config) => config,
        Err(ConfigLoadError::Error(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            Config::default()
        }
        Err(ConfigLoadError::Error(err)) => return Err(Error::new(err)),
        Err(ConfigLoadError::BadConfig(err)) => {
            eprintln!("Bad config: {}", err);
            eprintln!("Press <ENTER> to continue with default config");
            use std::io::Read;
            let _ = std::io::stdin().read(&mut []);
            Config::default()
        }
    };

    let lang_loader = helix_core::config::user_lang_loader().unwrap_or_else(|err| {
        eprintln!("{}", err);
        eprintln!("Press <ENTER> to continue with default language config");
        use std::io::Read;
        // This waits for an enter press.
        let _ = std::io::stdin().read(&mut []);
        helix_core::config::default_lang_loader()
    });

    // TODO: use the thread local executor to spawn the application task separately from the work pool
    let mut app =
        Application::new(config.clone(), lang_loader, listener).context("unable to start Helix")?;

    app.run().await
}
