//! The seam between code that calls module functions and the driver.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::error::Error;

/// Something that can call functions of one loaded PawnIO module.
///
/// [`crate::PawnIo`] is the real implementation; [`ScriptedExecutor`] is a
/// test double. Code that talks to a module takes `&dyn Executor` so its
/// register logic is testable without the driver.
pub trait Executor: Send + Sync {
    /// Calls `function` with `input` cells and fills `output`.
    ///
    /// The slice lengths are the cell counts the function sees; functions
    /// with a fixed signature fail with [`Error::InvalidParameter`] on any
    /// other length. On success only the cells the function documents are
    /// meaningful; see the crate documentation for why the rest may hold
    /// stale bytes.
    fn execute(&self, function: &str, input: &[u64], output: &mut [u64]) -> Result<(), Error>;

    /// Calls a function that returns exactly one cell.
    fn execute_scalar(&self, function: &str, input: &[u64]) -> Result<u64, Error> {
        let mut output = [0u64; 1];
        self.execute(function, input, &mut output)?;
        Ok(output[0])
    }
}

/// One call recorded by [`ScriptedExecutor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    /// The function name.
    pub function: String,
    /// The input cells.
    pub input: Vec<u64>,
    /// The number of output cells the caller asked for.
    pub output_cells: usize,
}

#[derive(Debug, Default)]
struct ScriptState {
    replies: VecDeque<Result<Vec<u64>, Error>>,
    repeat_when_empty: Option<Result<Vec<u64>, Error>>,
    calls: Vec<RecordedCall>,
}

/// An [`Executor`] that records every call and replays scripted replies in
/// order.
///
/// A reply is either the output cells to copy back or an error. When the
/// script runs out the executor repeats the reply set with
/// [`ScriptedExecutor::repeat_when_empty`], and panics with the offending
/// call if there is none: a test that issues an unplanned call must fail.
#[derive(Debug, Default)]
pub struct ScriptedExecutor {
    state: Mutex<ScriptState>,
}

impl ScriptedExecutor {
    /// An executor with an empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a successful reply. Cells beyond the caller's output length
    /// are dropped; missing cells stay zero.
    pub fn reply_ok(&self, cells: &[u64]) -> &Self {
        self.lock().replies.push_back(Ok(cells.to_vec()));
        self
    }

    /// Appends a failing reply.
    pub fn reply_err(&self, error: Error) -> &Self {
        self.lock().replies.push_back(Err(error));
        self
    }

    /// Reply to give after the script is exhausted, forever.
    pub fn repeat_when_empty(&self, reply: Result<Vec<u64>, Error>) -> &Self {
        self.lock().repeat_when_empty = Some(reply);
        self
    }

    /// Every call made so far, oldest first.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.lock().calls.clone()
    }

    /// Replies not yet consumed.
    pub fn remaining_replies(&self) -> usize {
        self.lock().replies.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScriptState> {
        // A panic while the lock is held only happens in `execute` after the
        // guard was dropped, so poisoning can be ignored.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Executor for ScriptedExecutor {
    fn execute(&self, function: &str, input: &[u64], output: &mut [u64]) -> Result<(), Error> {
        let reply = {
            let mut state = self.lock();
            state.calls.push(RecordedCall {
                function: function.to_owned(),
                input: input.to_vec(),
                output_cells: output.len(),
            });
            let call_number = state.calls.len();
            state.replies.pop_front().or_else(|| state.repeat_when_empty.clone()).ok_or(call_number)
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(call_number) => panic!(
                "(scripted-executor): expected a scripted reply for call #{call_number} | \
                 received: {function}({input:#x?}) with the script exhausted"
            ),
        };
        let cells = reply?;
        output.fill(0);
        for (slot, cell) in output.iter_mut().zip(cells) {
            *slot = cell;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_replayed_in_order_and_calls_are_recorded() {
        let script = ScriptedExecutor::new();
        script.reply_ok(&[0x11]).reply_ok(&[0x22, 0x33]);

        let mut first = [0u64; 1];
        script.execute("ioctl_read_msr", &[0x1A2], &mut first).unwrap();
        let mut second = [0u64; 2];
        script.execute("ioctl_x", &[1, 2], &mut second).unwrap();

        assert_eq!(first, [0x11]);
        assert_eq!(second, [0x22, 0x33]);
        assert_eq!(
            script.calls(),
            [
                RecordedCall {
                    function: "ioctl_read_msr".into(),
                    input: vec![0x1A2],
                    output_cells: 1
                },
                RecordedCall { function: "ioctl_x".into(), input: vec![1, 2], output_cells: 2 },
            ]
        );
        assert_eq!(script.remaining_replies(), 0);
    }

    #[test]
    fn scripted_errors_are_returned() {
        let script = ScriptedExecutor::new();
        script.reply_err(Error::NoSuchDevice { function: "ioctl_smbus_xfer".into() });
        let error = script.execute("ioctl_smbus_xfer", &[0x50], &mut [0u64; 1]).unwrap_err();
        assert_eq!(error, Error::NoSuchDevice { function: "ioctl_smbus_xfer".into() });
    }

    #[test]
    fn output_is_zero_filled_beyond_the_reply() {
        let script = ScriptedExecutor::new();
        script.reply_ok(&[7]);
        let mut output = [u64::MAX; 3];
        script.execute("ioctl_x", &[], &mut output).unwrap();
        assert_eq!(output, [7, 0, 0]);
    }

    #[test]
    fn execute_scalar_returns_the_first_cell() {
        let script = ScriptedExecutor::new();
        script.reply_ok(&[0xDEAD]);
        assert_eq!(script.execute_scalar("ioctl_pio_inb", &[0x295]), Ok(0xDEAD));
        assert_eq!(script.calls()[0].output_cells, 1);
    }

    #[test]
    fn repeat_when_empty_serves_unlimited_calls() {
        let script = ScriptedExecutor::new();
        script.repeat_when_empty(Ok(vec![5]));
        for _ in 0..3 {
            assert_eq!(script.execute_scalar("ioctl_x", &[]), Ok(5));
        }
    }

    #[test]
    #[should_panic(expected = "expected a scripted reply for call #1")]
    fn an_unplanned_call_fails_the_test() {
        ScriptedExecutor::new().execute("ioctl_x", &[1], &mut []).unwrap();
    }
}
