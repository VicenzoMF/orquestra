use tokio::sync::{broadcast, mpsc};

use crate::{AgentEvent, Command};

/// Fila de comandos: UI → hub. Sem limite para a UI nunca bloquear a thread de
/// render; comandos são pequenos e gerados por humanos.
pub type CommandSender = mpsc::UnboundedSender<Command>;
pub type CommandReceiver = mpsc::UnboundedReceiver<Command>;
pub type EventReceiver = broadcast::Receiver<AgentEvent>;

pub fn command_channel() -> (CommandSender, CommandReceiver) {
    mpsc::unbounded_channel()
}

/// Folga do canal principal. A UI drena numa task própria, então só atrasa se
/// a máquina travar; se atrasar, o `seq` do `Output` denuncia e a UI pede `Replay`.
pub const EVENT_CAPACITY: usize = 4096;
/// Quadros envelhecem rápido: fila curta, e quem atrasar pula para o mais novo.
pub const FRAME_CAPACITY: usize = 4;

/// Eventos: serviços → todos os assinantes (UI, recorder do store).
///
/// São dois broadcasts. `BrowserFrame` (volumoso e descartável) vai num canal
/// separado, para um assinante lento de quadros nunca empurrar `Output` ou
/// `StatusChanged` para fora da fila.
#[derive(Clone, Debug)]
pub struct EventBus {
    events: broadcast::Sender<AgentEvent>,
    frames: broadcast::Sender<AgentEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(EVENT_CAPACITY, FRAME_CAPACITY)
    }

    pub fn with_capacity(events: usize, frames: usize) -> Self {
        Self {
            events: broadcast::channel(events).0,
            frames: broadcast::channel(frames).0,
        }
    }

    /// Publica no canal certo. Sem assinantes, o evento é descartado.
    pub fn publish(&self, event: AgentEvent) {
        let tx = if event.is_frame() {
            &self.frames
        } else {
            &self.events
        };
        let _ = tx.send(event);
    }

    /// Todos os eventos, exceto `BrowserFrame`.
    pub fn subscribe(&self) -> EventReceiver {
        self.events.subscribe()
    }

    /// Só `BrowserFrame`.
    pub fn subscribe_frames(&self) -> EventReceiver {
        self.frames.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;

    use super::*;
    use crate::{AgentId, BrowserFrame, ImageFormat};

    #[test]
    fn frames_go_to_their_own_channel() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let mut frames = bus.subscribe_frames();
        let id = AgentId::generate();

        bus.publish(AgentEvent::Removed { id });
        bus.publish(AgentEvent::BrowserFrame {
            id,
            frame: Arc::new(BrowserFrame {
                seq: 0,
                format: ImageFormat::Jpeg,
                data: Bytes::new(),
                css_width: 1.0,
                css_height: 1.0,
                page_scale: 1.0,
                offset_top: 0.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
            }),
        });

        assert!(matches!(events.try_recv(), Ok(AgentEvent::Removed { .. })));
        assert!(events.try_recv().is_err());
        assert!(frames.try_recv().unwrap().is_frame());
    }
}
