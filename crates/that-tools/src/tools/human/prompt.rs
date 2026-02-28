//! Terminal-based prompting using dialoguer.

use dialoguer::{Confirm, Input};
use std::sync::mpsc;
use std::time::Duration;

/// Ask for free-form input in the terminal with optional timeout.
pub fn ask_terminal(
    message: &str,
    timeout_secs: Option<u64>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(secs) = timeout_secs {
        // Use a channel + thread to implement timeout
        let msg = message.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result: Result<String, _> = Input::new().with_prompt(&msg).interact_text();
            let _ = tx.send(result);
        });
        match rx.recv_timeout(Duration::from_secs(secs)) {
            Ok(result) => Ok(result?),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("prompt timed out after {} seconds", secs).into())
            }
            Err(e) => Err(format!("prompt channel error: {}", e).into()),
        }
    } else {
        let response: String = Input::new().with_prompt(message).interact_text()?;
        Ok(response)
    }
}

/// Ask for yes/no confirmation in the terminal.
pub fn confirm_terminal(message: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let result = Confirm::new()
        .with_prompt(message)
        .default(false)
        .interact()?;
    Ok(result)
}
