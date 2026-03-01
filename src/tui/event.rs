use anyhow::Result;
use crossterm::event::{self, Event, KeyEvent, MouseEvent};
use std::time::Duration;
use tokio::sync::mpsc;

/// Terminal events that the TUI processes.
#[derive(Debug)]
#[allow(dead_code)]
pub enum AppEvent {
    /// A key was pressed
    Key(KeyEvent),
    /// Mouse activity
    Mouse(MouseEvent),
    /// Terminal was resized
    Resize(u16, u16),
    /// Periodic tick for background refresh
    Tick,
}

/// Event handler that polls crossterm events on a background task
/// and sends them through an mpsc channel.
pub struct EventHandler {
    rx: mpsc::UnboundedReceiver<AppEvent>,
    _tx: mpsc::UnboundedSender<AppEvent>,
}

impl EventHandler {
    /// Create a new event handler with a given tick rate.
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let event_tx = tx.clone();

        tokio::spawn(async move {
            loop {
                // Poll for crossterm events with the tick rate as timeout
                let has_event = tokio::task::spawn_blocking({
                    let tick_rate = tick_rate;
                    move || event::poll(tick_rate).unwrap_or(false)
                })
                .await
                .unwrap_or(false);

                if has_event {
                    // Read the event (blocking but should be immediate since poll returned true)
                    let evt = tokio::task::spawn_blocking(|| event::read())
                        .await
                        .ok()
                        .and_then(|r| r.ok());

                    if let Some(evt) = evt {
                        let app_event = match evt {
                            Event::Key(key) => Some(AppEvent::Key(key)),
                            Event::Mouse(mouse) => Some(AppEvent::Mouse(mouse)),
                            Event::Resize(w, h) => Some(AppEvent::Resize(w, h)),
                            _ => None,
                        };
                        if let Some(e) = app_event {
                            if event_tx.send(e).is_err() {
                                break;
                            }
                        }
                    }
                } else {
                    // Tick event (no crossterm event within tick_rate)
                    if event_tx.send(AppEvent::Tick).is_err() {
                        break;
                    }
                }
            }
        });

        Self { rx, _tx: tx }
    }

    /// Get the next event, waiting asynchronously.
    pub async fn next(&mut self) -> Result<AppEvent> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("Event channel closed"))
    }
}
