//! shellswim's command line. A shell should also be a good command,
//! so one line is runnable without a terminal session and a file of
//! lines is runnable as a script.

use std::path::PathBuf;

/// What this invocation is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    /// Interactive: the prompt, until `quit()` or ctrl+c.
    Prompt,
    /// `-c "play(crest)"`: run it, print what happened, exit.
    Command(String),
    /// `shellswim set.shg`: run the file's lines, then exit.
    Script(PathBuf),
    Version,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub run: Run,
}

impl Cli {
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
        let mut run = Run::Prompt;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--version" | "-V" => return Ok(Cli { run: Run::Version }),
                "--help" | "-h" => return Ok(Cli { run: Run::Help }),
                "-c" | "--command" => {
                    let Some(line) = args.next() else {
                        return Err("-c wants a command, e.g. -c 'play(crest)'".into());
                    };
                    // checked in both orders: `-c` after a script
                    // silently replaced it, which would have run
                    // something other than what was asked for.
                    if run != Run::Prompt {
                        return Err(BOTH.into());
                    }
                    run = Run::Command(line);
                }
                // one positional, a script. anything starting with `-`
                // that got this far is a typo, and guessing at a typo
                // in something that controls playback is worse than
                // saying so.
                other if other.starts_with('-') => {
                    return Err(format!("unknown option '{other}'"));
                }
                other => {
                    if run != Run::Prompt {
                        return Err(BOTH.into());
                    }
                    run = Run::Script(PathBuf::from(other));
                }
            }
        }
        Ok(Cli { run })
    }
}

/// A script is lines of shellsong, `#` comments and blanks ignored.
/// Each line goes through the same parse a typed line does, so a
/// script cannot do anything the prompt cannot.
pub fn script_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

const BOTH: &str = "give either -c or a script, not both";

pub const USAGE: &str = "usage: shellswim [-c COMMAND | SCRIPT.shg] [--version] [--help]";

pub fn help() -> String {
    format!(
        "{USAGE}

shellswim is wim with nothing but the prompt: a full-screen shellsong
console and the playing track's scrubber. everything is a command.

  shellswim                 the prompt
  shellswim -c 'play(hex)'  run one command, print the result, exit
  shellswim notes.shg       run a file of commands, then exit
  shellswim --version       which build this is
  shellswim --help          this

at the prompt:
  tab / shift+tab   cycle the completions for what you are typing
  right / ctrl+f    accept the greyed-out completion
  up / down         walk back through what you have run
  ?                 the command list, short
  help()            the command list, full
  dry(...)          say what a command would do, run nothing
  quit() / ctrl+c   leave

shellswim has no panes, so the commands that open one (view(eq),
view(settings), view(keybinds), view(starred), view(suggestions))
say so and name wim instead. view(queue), view(history),
view(playlists), view(index) and view(help) print here, and
view(now) and view(lyrics) print instead of opening a pane.

  config   ~/.config/wim/config.toml, shared with wim
  trace    WIM_STARTUP_TRACE=1 shellswim 2>trace.log"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        Cli::parse(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn no_arguments_is_the_prompt() {
        assert_eq!(parse(&[]).unwrap().run, Run::Prompt);
    }

    #[test]
    fn version_and_help_win_over_everything() {
        // they have to work with no terminal, no config and no server,
        // so nothing may be parsed ahead of them.
        assert_eq!(
            parse(&["-c", "play(x)", "--version"]).unwrap().run,
            Run::Version
        );
        assert_eq!(parse(&["--help"]).unwrap().run, Run::Help);
        assert_eq!(parse(&["-V"]).unwrap().run, Run::Version);
        assert_eq!(parse(&["-h"]).unwrap().run, Run::Help);
    }

    #[test]
    fn dash_c_takes_one_command_and_exits() {
        let cli = parse(&["-c", "play(crest)"]).unwrap();
        assert_eq!(cli.run, Run::Command("play(crest)".into()));
    }

    #[test]
    fn dash_c_without_a_command_is_an_error_not_an_empty_run() {
        assert!(parse(&["-c"]).is_err());
    }

    #[test]
    fn a_positional_is_a_script() {
        let cli = parse(&["set.shg"]).unwrap();
        assert_eq!(cli.run, Run::Script(PathBuf::from("set.shg")));
    }

    #[test]
    fn a_command_and_a_script_together_are_refused() {
        assert!(parse(&["-c", "play(x)", "set.shg"]).is_err());
        assert!(parse(&["set.shg", "-c", "play(x)"]).is_err());
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_guessed() {
        // it controls playback; a typo should not be interpreted.
        let err = parse(&["--volume"]).unwrap_err();
        assert!(err.contains("--volume"), "{err}");
    }

    #[test]
    fn a_script_drops_comments_and_blanks() {
        let text = "# a set\n\nplay(hex)\n   \n  volume(40)  \n# done\n";
        assert_eq!(script_lines(text), vec!["play(hex)", "volume(40)"]);
    }

    #[test]
    fn an_empty_script_is_no_lines_not_one_empty_line() {
        assert!(script_lines("").is_empty());
        assert!(script_lines("\n\n# only comments\n").is_empty());
    }

    #[test]
    fn help_names_what_it_cannot_do() {
        // the help is where someone looks first when `view(eq)` is
        // refused, so the refusal has to be documented there.
        let h = help();
        for needed in ["view(eq)", "wim", "?", "dry(", "WIM_STARTUP_TRACE"] {
            assert!(h.contains(needed), "help must mention {needed}");
        }
    }
}
