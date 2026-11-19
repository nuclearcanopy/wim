//! The UI-free half of shellswim's host: everything the prompt and a
//! one-shot `-c` share that does not touch a terminal, so the mobile
//! front-ends can drive the same session.
//!
//! The bin's `Host` holds one of these and keeps what is terminal- or
//! process-bound: the TUI, the TorBox wiring, the lyrics pane, the
//! instance lock and the mpv child. Playback goes through `&dyn Player`
//! arguments, never an mpv handle owned here.

use crate::shellsong::Program;
use std::collections::VecDeque;

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
