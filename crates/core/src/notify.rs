use crate::{
    AgentId, AgentStatus, Notification, NotificationConfig, NotificationKind, QueueReason,
};

/// Decide se uma transição de status vira notificação. Só a entrada num
/// estado notifica: o mesmo estado repetido não.
pub fn notification_for(
    id: AgentId,
    name: &str,
    prev: Option<&AgentStatus>,
    next: &AgentStatus,
    cfg: &NotificationConfig,
) -> Option<Notification> {
    if !cfg.enabled || prev.is_some_and(|p| p.same_state(next)) {
        return None;
    }
    let (kind, body) = match next {
        AgentStatus::WaitingForInput if cfg.on_waiting_for_input => (
            NotificationKind::NeedsInput,
            "Aguardando sua resposta".to_owned(),
        ),
        AgentStatus::Queued {
            reason: QueueReason::Approval,
        } if cfg.on_waiting_for_input => (
            NotificationKind::NeedsApproval,
            "O orquestrador quer criar este agente: aprove ou recuse".to_owned(),
        ),
        AgentStatus::Done { .. } if cfg.on_done => {
            (NotificationKind::Finished, "Terminou".to_owned())
        }
        AgentStatus::Failed { reason } if cfg.on_failed => {
            (NotificationKind::Failed, format!("Falhou: {reason}"))
        }
        _ => return None,
    };
    Some(Notification {
        agent: id,
        kind,
        title: name.to_owned(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifies_on_entering_waiting_only_once() {
        let id = AgentId::generate();
        let cfg = NotificationConfig::default();
        let waiting = AgentStatus::WaitingForInput;

        let first = notification_for(id, "a", Some(&AgentStatus::Running), &waiting, &cfg);
        assert_eq!(first.unwrap().kind, NotificationKind::NeedsInput);
        assert!(notification_for(id, "a", Some(&waiting), &waiting, &cfg).is_none());
    }

    #[test]
    fn respects_toggles() {
        let id = AgentId::generate();
        let cfg = NotificationConfig {
            on_done: false,
            ..NotificationConfig::default()
        };
        let done = AgentStatus::Done { exit_code: Some(0) };
        assert!(notification_for(id, "a", None, &done, &cfg).is_none());
        assert!(notification_for(id, "a", None, &AgentStatus::Running, &cfg).is_none());
    }
}
