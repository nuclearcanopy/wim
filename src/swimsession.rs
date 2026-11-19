//! The UI-free half of shellswim's host: everything the prompt and a
//! one-shot `-c` share that does not touch a terminal, so the mobile
//! front-ends can drive the same session.
//!
//! The bin's `Host` holds one of these and keeps what is terminal- or
//! process-bound: the TUI, the TorBox wiring, the lyrics pane, the
//! instance lock and the mpv child. Playback goes through `&dyn Player`
//! arguments, never an mpv handle owned here.

use crate::shellsong::{Console, LineKind, Program};
use crate::swimsurface::SwimSurface;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Where a submitted program goes: straight to the active slot, or
/// behind whatever is running or already waiting. `Some` means "run it
/// now".
pub fn place_submitted_program(
    active: bool,
    core_waiting: bool,
    waiting: &mut VecDeque<Box<Program>>,
    program: Box<Program>,
) -> Option<Box<Program>> {
    if active || core_waiting || !waiting.is_empty() {
        waiting.push_back(program);
        None
    } else {
        Some(program)
    }
}

pub struct SwimSession {
    /// What the core hands over to the front-end (`SwimSurface`).
    pub swim: SwimSurface,
    pub console: Console,
    /// The live `let` bindings and the per-identity settings, both
    /// read once here and written back by the ops that change them.
    pub vars: crate::shellsong::Scope,
    pub program: Option<Box<Program>>,
    queued_programs: VecDeque<Box<Program>>,
    pub start: Instant,
}

impl SwimSession {
    pub fn new() -> SwimSession {
        SwimSession {
            swim: SwimSurface::default(),
            console: Console::new(),
            vars: crate::shellsong_host::load_vars(),
            program: None,
            queued_programs: VecDeque::new(),
            start: Instant::now(),
        }
    }

    pub fn now(&self) -> Duration {
        self.start.elapsed()
    }

    /// Retain every submitted program. Shellsong programs may await a
    /// search or remote operation for several frames, so replacing the
    /// active slot would silently discard the user's earlier command.
    pub fn submit_program(&mut self, program: Box<Program>) {
        match place_submitted_program(
            self.program.is_some(),
            self.swim.queued.is_some(),
            &mut self.queued_programs,
            program,
        ) {
            Some(program) => self.program = Some(program),
            None => self.console.note(format!(
                "queued command ({} waiting)",
                self.queued_programs.len()
            )),
        }
    }

    pub fn has_program_work(&self) -> bool {
        self.program.is_some() || !self.queued_programs.is_empty() || self.swim.queued.is_some()
    }

    /// Drops everything waiting, for a one-shot that gave up.
    pub fn abandon_programs(&mut self) {
        self.program = None;
        self.queued_programs.clear();
        self.swim.queued = None;
    }

    /// Starts the next program if nothing is active: the one the core
    /// queued, else the oldest waiting submission.
    pub fn take_next_program(&mut self) {
        if self.program.is_none() {
            self.program = self
                .swim
                .queued
                .take()
                .or_else(|| self.queued_programs.pop_front());
        }
    }

    /// Parses a line against the live bindings and script directory,
    /// the same environment used by interactive submission.
    pub fn begin(&mut self, line: &str) {
        self.console.push(LineKind::Command, line.to_string());
        let read = |name: &str| crate::shellsong_host::read_script(name);
        let world = crate::shellsong::World {
            scope: &self.vars,
            read: Some(&read),
        };
        match Program::parse_in(line, &world) {
            Ok(p) => self.submit_program(Box::new(p)),
            Err(e) => self.console.fail(e.to_string()),
        }
    }
}

impl Default for SwimSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod program_queue_tests {
    use super::*;
    use crate::shellsong::Op;

    #[test]
    fn a_submission_waits_behind_the_active_program_without_replacing_it() {
        let mut waiting = VecDeque::new();
        let submitted = Box::new(Program::of(vec![Op::ClearConsole]));
        let submitted_ptr = (&*submitted) as *const Program;
        let active = place_submitted_program(true, false, &mut waiting, submitted);

        assert!(active.is_none());
        assert_eq!(waiting.len(), 1);
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            submitted_ptr
        );
    }

    #[test]
    fn a_submission_waits_behind_a_track_binding_even_when_idle() {
        let mut waiting = VecDeque::new();
        let submitted = Box::new(Program::of(vec![Op::ClearConsole]));

        assert!(place_submitted_program(false, true, &mut waiting, submitted).is_none());
        assert_eq!(waiting.len(), 1);
    }

    #[test]
    fn later_submissions_keep_fifo_order_behind_core_work() {
        let mut waiting = VecDeque::new();
        let first = Box::new(Program::of(vec![Op::ClearConsole]));
        let second = Box::new(Program::of(vec![Op::ClearQueue]));
        let first_ptr = (&*first) as *const Program;
        let second_ptr = (&*second) as *const Program;

        assert!(place_submitted_program(false, true, &mut waiting, first).is_none());
        assert!(place_submitted_program(false, false, &mut waiting, second).is_none());
        assert_eq!(waiting.len(), 2);
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            first_ptr
        );
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            second_ptr
        );
    }
}
