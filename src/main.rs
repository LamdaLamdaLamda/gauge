mod app;
mod cli;
mod config;
mod filelog;
mod net;
mod store;
mod theme;
mod ui;
mod util;

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::App;

const TICK_RATE: Duration = Duration::from_millis(200);

fn main() -> io::Result<()> {
    match cli::Cli::parse().command {
        Some(cli::Command::Client(args)) => std::process::exit(cli::run_client(args)),
        Some(cli::Command::Server(args)) => std::process::exit(cli::run_server(args)),
        None => run_tui(),
    }
}

fn run_tui() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::default();

    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if event::poll(TICK_RATE)? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.on_key(key);
            }
        } else {
            app.tick();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}
