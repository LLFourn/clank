//! The public command is `tui`; retired spellings are parser errors.
use clank::cli::command::Cli;
use clap::Parser;

#[test]
fn tui_parses_and_the_retired_commands_do_not() {
    assert!(Cli::try_parse_from(["clank", "tui"]).is_ok());
    for args in [vec!["clank", "status", "--tui"], vec!["clank", "web"]] {
        assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
    }
}
