//! Both front-ends' command lines. A shell should also be a good
//! command, so one line is runnable without a terminal session and a
//! file of lines is runnable as a script.
//!
//! `wim`'s half lives here too, because the half that matters is the
//! same for both: `--version`/`--help` have to answer with no
//! terminal, no config and no server, and an unrecognised `-x` is
//! refused rather than guessed. `wim` answered neither and ignored
//! every argument it was given in silence (`wim --help` and
//! `wim song.mp3` both just started the panes).
//!
//! This file's home is `src/cli.rs` in the library, not a private
//! module of one binary; see docs/library-seam.md. It is here only
//! because the move is the caller's to make.

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

/// One argument, classified by the part both front-ends share.
enum Arg<'a> {
    Version,
    Help,
    /// Looks like an option and is not one this binary knows.
    Unknown(&'a str),
    /// Not an option, so whatever this binary's positional means.
    Positional(&'a str),
}

/// The shared half of both parsers: the two options that have to
/// answer with no terminal, no config and no server, and the rule
/// that an unrecognised `-x` is refused. Guessing at a typo in
/// something that controls playback is worse than saying so, and
/// spelling the rule once is what stops the two drifting.
fn classify(arg: &str) -> Arg<'_> {
    match arg {
        "--version" | "-V" => Arg::Version,
        "--help" | "-h" => Arg::Help,
        other if other.starts_with('-') => Arg::Unknown(other),
        other => Arg::Positional(other),
    }
}

fn unknown(arg: &str) -> String {
    format!("unknown option '{arg}'")
}

impl Cli {
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
        let mut run = Run::Prompt;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            // `-c` is shellswim's own and takes a value, so it is
            // answered before the shared classification sees a `-`.
            if arg == "-c" || arg == "--command" {
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
                continue;
            }
            match classify(&arg) {
                Arg::Version => return Ok(Cli { run: Run::Version }),
                Arg::Help => return Ok(Cli { run: Run::Help }),
                Arg::Unknown(other) => return Err(unknown(other)),
                // one positional, a script.
                Arg::Positional(other) => {
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

// `wim`'s half, which the binary hosting this file does not call.
// The allows go when it moves to `src/cli.rs` and `main.rs` calls it;
// see the module doc.
/// What `wim`'s argv asked for. Its own enum rather than a shape bent
/// to fit `Run`: wim has no `-c` and takes any number of files, where
/// a second script is an error.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WimRun {
    /// No arguments: the panes, resuming what was playing.
    Panes,
    /// `wim a.flac b.mp3`: play the first and queue the rest, instead
    /// of resuming. A media player plays what it is handed; that is
    /// what `xdg-open` and a desktop file association send, and what
    /// mpv, vlc and mplayer all do.
    Play(Vec<PathBuf>),
    Version,
    Help,
}

/// `wim`'s argv. Several files are a queue rather than an error,
/// which is the one place the two front-ends deliberately differ.
#[allow(dead_code)]
pub fn parse_wim<I: IntoIterator<Item = String>>(args: I) -> Result<WimRun, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for arg in args {
        match classify(&arg) {
            // ahead of the files on purpose, so these answer whatever
            // else is on the line.
            Arg::Version => return Ok(WimRun::Version),
            Arg::Help => return Ok(WimRun::Help),
            Arg::Unknown(other) => return Err(unknown(other)),
            Arg::Positional(other) => files.push(PathBuf::from(other)),
        }
    }
    Ok(if files.is_empty() {
        WimRun::Panes
    } else {
        WimRun::Play(files)
    })
}

/// The command line's files as playable items, or the first one that
/// is not playable, named. `persistence::item_from_path` is the same
/// resolver the saved session uses, so a file from argv and a resumed
/// one are the same kind of thing downstream.
///
/// Refused rather than skipped: being handed a path and then starting
/// the panes with nothing playing is exactly the silence this file
/// exists to remove.
#[allow(dead_code)]
pub fn resolve_files(files: &[PathBuf]) -> Result<Vec<wim::search::SearchItem>, String> {
    files
        .iter()
        .map(|p| wim::persistence::item_from_path(&p.to_string_lossy()).ok_or_else(|| unplayable(p)))
        .collect()
}

/// Why a path was refused, in the terms the user used. A url gets its
/// own sentence because mpv could play one and this cannot yet, so
/// "does not exist" would be a wrong answer rather than a short one.
fn unplayable(path: &std::path::Path) -> String {
    let what = path.to_string_lossy();
    if what.contains("://") {
        format!("'{what}' is a url; wim takes local music and video files")
    } else {
        format!("cannot play '{what}': not a music or video file that exists")
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

#[allow(dead_code)]
pub const WIM_USAGE: &str = "usage: wim [FILE...] [--version] [--help]";

/// What a file argument does to the resumed session, spelled once:
/// the help says it and `main.rs` does it.
#[allow(dead_code)]
pub const PLAY_REPLACES_RESUME: &str =
    "a file argument plays instead of resuming; the rest are queued behind it";

#[allow(dead_code)]
pub fn wim_help() -> String {
    format!(
        "{WIM_USAGE}

wim is a terminal omniplayer: navidrome/subsonic and local files in one
search, played through mpv.

  wim                 the panes, resuming what was playing
  wim song.flac       play it now ({PLAY_REPLACES_RESUME})
  wim a.mp3 b.mp3     play the first, queue the rest
  wim --version       which build this is
  wim --help          this

a file has to be a local music or video file that exists; a url is not
one yet, and neither is refused in silence. shellswim is the same
player with nothing but the shellsong prompt.

in the app:
  ?        every key, with whatever they are bound to in your config
  :        the shellsong console
  tab      move focus between the panes

  config   ~/.config/wim/config.toml, shared with shellswim
  trace    WIM_STARTUP_TRACE=1 wim 2>trace.log"
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

    fn wim(args: &[&str]) -> Result<WimRun, String> {
        parse_wim(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn wim_with_no_arguments_is_the_panes() {
        assert_eq!(wim(&[]).unwrap(), WimRun::Panes);
    }

    /// The whole defect: `wim --help` started the panes and said
    /// nothing, so a binary that controls playback had no way to tell
    /// you what it took.
    #[test]
    fn wim_answers_help_and_version_instead_of_starting() {
        for arg in ["--help", "-h"] {
            assert_eq!(wim(&[arg]).unwrap(), WimRun::Help, "{arg}");
        }
        for arg in ["--version", "-V"] {
            assert_eq!(wim(&[arg]).unwrap(), WimRun::Version, "{arg}");
        }
        // and they win over a file, so both still answer with no
        // terminal, no config and no server.
        assert_eq!(wim(&["song.mp3", "--version"]).unwrap(), WimRun::Version);
        assert_eq!(wim(&["song.mp3", "-h"]).unwrap(), WimRun::Help);
    }

    #[test]
    fn a_file_argument_is_something_to_play() {
        assert_eq!(
            wim(&["song.mp3"]).unwrap(),
            WimRun::Play(vec![PathBuf::from("song.mp3")])
        );
        // several files are a queue, not an error: the one place the
        // two front-ends deliberately differ.
        assert_eq!(
            wim(&["a.flac", "b.mp3"]).unwrap(),
            WimRun::Play(vec![PathBuf::from("a.flac"), PathBuf::from("b.mp3")])
        );
    }

    #[test]
    fn wim_refuses_an_unknown_option_rather_than_ignoring_it() {
        // it used to ignore every one of these and start the panes,
        // which is the same class of silence as guessing.
        for bad in ["--volume", "-x", "--shuffle"] {
            let err = wim(&[bad]).unwrap_err();
            assert!(err.contains(bad), "{bad}: {err}");
        }
        // and the file that followed it is not quietly played either.
        assert!(wim(&["--volume", "song.mp3"]).is_err());
    }

    /// Both parsers answer `-h`/`-V` and refuse an unknown option the
    /// same way, because one function decides it.
    #[test]
    fn both_front_ends_answer_the_same_two_options() {
        for arg in ["--help", "-h"] {
            assert_eq!(parse(&[arg]).unwrap().run, Run::Help);
            assert_eq!(wim(&[arg]).unwrap(), WimRun::Help);
        }
        for arg in ["--version", "-V"] {
            assert_eq!(parse(&[arg]).unwrap().run, Run::Version);
            assert_eq!(wim(&[arg]).unwrap(), WimRun::Version);
        }
        assert_eq!(
            parse(&["--nope"]).unwrap_err(),
            wim(&["--nope"]).unwrap_err()
        );
    }

    /// Real files, since `item_from_path` asks the filesystem: that
    /// is the point of it, and a test against a made-up path would
    /// only pin the refusal.
    #[test]
    fn files_resolve_to_playable_items_or_say_why_not() {
        let dir = std::env::temp_dir().join(format!("wim-clitest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let song = dir.join("song.mp3");
        let video = dir.join("clip.mkv");
        let junk = dir.join("notes.txt");
        for p in [&song, &video, &junk] {
            std::fs::write(p, b"x").expect("write");
        }

        let ok = resolve_files(&[song.clone(), video.clone()]).expect("both are playable");
        assert_eq!(ok.len(), 2);

        // a file that exists but is not media, named rather than
        // skipped into an empty session.
        let err = resolve_files(&[song.clone(), junk.clone()]).unwrap_err();
        assert!(err.contains("notes.txt"), "{err}");

        let missing = dir.join("gone.flac");
        let err = resolve_files(&[missing]).unwrap_err();
        assert!(err.contains("gone.flac"), "{err}");

        // a url is not a local file and gets its own sentence, so the
        // answer is not the wrong "does not exist".
        let err = resolve_files(&[PathBuf::from("https://example.com/a.mp3")]).unwrap_err();
        assert!(err.contains("url"), "{err}");

        assert!(resolve_files(&[]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wim_help_says_what_a_file_argument_does() {
        let h = wim_help();
        for needed in [
            WIM_USAGE,
            PLAY_REPLACES_RESUME,
            "wim --version",
            "shellswim",
            "url",
            "WIM_STARTUP_TRACE",
        ] {
            assert!(h.contains(needed), "wim --help must mention {needed:?}");
        }
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
