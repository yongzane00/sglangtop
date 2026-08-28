//! Thin binary: parse CLI, load config, wire channels, run the app,
//! and guarantee the terminal is restored on every exit path.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use tokio::sync::mpsc;

use sglangtop::app::App;
use sglangtop::cli::Cli;
use sglangtop::event::AppEvent;
use sglangtop::ui::theme::Theme;

// musl's default allocator degrades badly under tokio's thread pool;
// mimalloc restores glibc-like performance in fully static builds.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "sglangtop", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }

    let config = match sglangtop::config::load(&cli, |k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sglangtop: {e}");
            return ExitCode::FAILURE;
        }
    };

    init_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sglangtop: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let theme = Theme::detect(config.no_color, |k| std::env::var(k).ok());

    // ratatui::init() enters the alternate screen + raw mode and installs a
    // panic hook that restores the terminal before the panic message prints.
    let mut terminal = ratatui::init();
    let result = runtime.block_on(run(config, theme, &mut terminal));
    ratatui::restore();
    // Drop the runtime after restore so any lingering collector tasks cannot
    // write to a raw terminal.
    drop(runtime);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sglangtop: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(
    config: sglangtop::config::Config,
    theme: Theme,
    terminal: &mut ratatui::DefaultTerminal,
) -> anyhow::Result<()> {
    // One bounded channel carries collector results and input events.
    let (tx, rx) = mpsc::channel::<AppEvent>(256);

    let control = sglangtop::collector::spawn_all(&config, tx.clone());
    sglangtop::logtail::spawn_all(&config, tx.clone());
    spawn_input_thread(tx.clone());

    let app = App::new(config, theme, control, tx);
    app.run(terminal, rx).await
}

/// Blocking crossterm reads on a plain OS thread; events are forwarded into
/// the async world over the same bounded channel the collectors use. The
/// thread exits when the channel closes (app shut down).
fn spawn_input_thread(tx: mpsc::Sender<AppEvent>) {
    use ratatui::crossterm::event::{self, Event, KeyEventKind};
    std::thread::Builder::new()
        .name("sglangtop-input".into())
        .spawn(move || {
            loop {
                // Poll with a timeout so a closed channel is noticed even
                // when no keys are pressed.
                match event::poll(std::time::Duration::from_millis(250)) {
                    Ok(true) => {
                        let app_event = match event::read() {
                            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                                Some(AppEvent::Key(k))
                            }
                            Ok(Event::Mouse(m)) => Some(AppEvent::Mouse(m)),
                            Ok(Event::Resize(_, _)) => Some(AppEvent::Resize),
                            Ok(_) => None,
                            Err(_) => Some(AppEvent::InputClosed),
                        };
                        if let Some(ev) = app_event {
                            let closed = matches!(ev, AppEvent::InputClosed);
                            if tx.blocking_send(ev).is_err() || closed {
                                return;
                            }
                        }
                    }
                    Ok(false) => {
                        if tx.is_closed() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.blocking_send(AppEvent::InputClosed);
                        return;
                    }
                }
            }
        })
        .expect("spawning the input thread cannot fail on supported platforms");
}

/// Tracing goes to a file only when SGLANGTOP_LOG points somewhere; the TUI
/// owns stdout/stderr, so logging there would corrupt the display.
fn init_tracing() {
    let Some(path) = std::env::var_os("SGLANGTOP_LOG") else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init();
}
