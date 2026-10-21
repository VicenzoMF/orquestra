//! Os métodos do gateway (`list_agents`, `spawn_agent`...), sempre dentro da
//! sessão da credencial.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use orquestra_core::{
    AgentId, AgentKind, AgentPreset, AgentStatus, Command, QueueReason, SessionConfig, TermSize,
    Workspace,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::gateway::{SHUTTING_DOWN, Shared};
use crate::protocol::{
    KillAgentParams, MAX_LINES, MAX_TIMEOUT_SECS, ReadOutputParams, SendInputParams,
    SpawnAgentParams, TakeNewsParams, WaitForParams, WaitUntil, kind_code, method, status_code,
    status_detail,
};
use crate::state::{Grant, State, WaitSpec};

/// Quanto o `spawn_agent` espera o hub responder com o primeiro status.
const SPAWN_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Pausa entre o texto e o Enter: TUIs (Claude Code) tratam texto + `\r` no
/// mesmo pedaço como colagem e não enviam.
const ENTER_DELAY: Duration = Duration::from_millis(100);
const MAX_NAME_CHARS: usize = 40;

const HUB_DOWN: &str = "o hub do orquestra não está rodando";

impl Shared {
    pub(crate) async fn dispatch(
        self: &Arc<Self>,
        grant: Grant,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        if self.lock().shutdown {
            return Err(SHUTTING_DOWN.to_owned());
        }
        match method {
            method::LIST_AGENTS => self.list_agents(grant),
            method::SPAWN_AGENT => self.spawn_agent(grant, parse(params)?).await,
            method::SEND_INPUT => self.send_input(grant, parse(params)?).await,
            method::READ_OUTPUT => self.read_output(grant, parse(params)?),
            method::WAIT_FOR => self.wait_for(grant, parse(params)?).await,
            method::KILL_AGENT => self.kill_agent(grant, parse(params)?),
            method::TAKE_NEWS => self.take_news(grant, parse(params)?).await,
            other => Err(format!("método desconhecido: {other}")),
        }
    }

    fn list_agents(&self, grant: Grant) -> Result<Value, String> {
        let st = self.lock();
        let session = st.sessions.get(&grant.session).map(|s| {
            json!({
                "name": s.name,
                "goal": s.goal,
                "repo": s.repo,
            })
        });
        let agents: Vec<_> = st
            .session_agents(grant.session)
            .into_iter()
            .filter_map(|id| st.summary(id))
            .collect();
        Ok(json!({
            "session": session,
            "you": st.name_of(grant.orchestrator),
            "agents": agents,
        }))
    }

    async fn spawn_agent(&self, grant: Grant, p: SpawnAgentParams) -> Result<Value, String> {
        let task = p.task.trim();
        if task.is_empty() {
            return Err("task vazia: descreva a tarefa do agente".to_owned());
        }
        let base = normalize_name(&p.name)
            .ok_or("nome inválido: use letras, números, '-' ou '_'".to_owned())?;
        let presets = self.config().presets;

        let (id, name, config, ack) = {
            let mut st = self.lock();
            let session = st
                .sessions
                .get(&grant.session)
                .cloned()
                .ok_or("a sessão desta orquestração foi encerrada".to_owned())?;
            let preset = choose_preset(&session, &presets, p.kind.as_deref())?;
            let browser = match p.browser {
                Some(true) if !session.workers_may_browse => {
                    return Err("esta sessão não permite navegador para os agentes".to_owned());
                }
                Some(browser) => browser,
                None => preset.browser && session.workers_may_browse,
            };
            let name = st.unique_name(grant.session, &base);
            let workspace = Workspace::Worktree {
                repo: session.repo.clone(),
                base: None,
                branch: None,
            };
            let mut config =
                preset.to_config(name.clone(), grant.session, workspace, TermSize::default());
            config.parent = Some(grant.orchestrator);
            config.initial_prompt = Some(task.to_owned());
            config.browser = browser;
            let id = AgentId::generate();
            let ack = st.insert_pending(id, config.clone());
            (id, name, config, ack)
        };

        let sent = self.commands.send(Command::Spawn {
            id,
            config: Box::new(config),
        });
        if sent.is_err() {
            self.lock().cancel_pending(id, true);
            return Err(HUB_DOWN.to_owned());
        }
        let status = match tokio::time::timeout(SPAWN_ACK_TIMEOUT, ack).await {
            Ok(Ok(Ok(status))) => status,
            Ok(Ok(Err(message))) => return Err(format!("o hub recusou o spawn: {message}")),
            Ok(Err(_)) | Err(_) => {
                self.lock().cancel_pending(id, false);
                return Ok(json!({
                    "agent": name,
                    "id": id.short(),
                    "status": "pending",
                    "note": "o hub ainda não confirmou; acompanhe com list_agents",
                }));
            }
        };
        let note = match &status {
            AgentStatus::Queued {
                reason: QueueReason::Capacity,
            } => Some("na fila: limite de agentes simultâneos atingido; sobe quando abrir vaga"),
            AgentStatus::Queued {
                reason: QueueReason::Approval,
            } => Some("aguardando o humano aprovar na UI"),
            _ => None,
        };
        Ok(with_note(
            json!({
                "agent": name,
                "id": id.short(),
                "status": status_code(&status),
                "detail": status_detail(&status),
            }),
            note,
        ))
    }

    async fn send_input(&self, grant: Grant, p: SendInputParams) -> Result<Value, String> {
        let (id, name, bracketed) = {
            let mut st = self.lock();
            let id = st.resolve(grant.session, &p.agent)?;
            if id == grant.orchestrator {
                return Err("send_input é para os seus agentes, não para você mesmo".to_owned());
            }
            let name = st.name_of(id);
            let agent = st
                .agents
                .get_mut(&id)
                .ok_or(format!("agente desconhecido: {name}"))?;
            match &agent.status {
                None | Some(AgentStatus::Queued { .. }) => {
                    return Err(format!("{name} ainda não iniciou (está na fila)"));
                }
                Some(status) if status.is_terminal() => {
                    return Err(format!("{name} já terminou ({})", status_code(status)));
                }
                Some(_) => {}
            }
            agent.input_at = Some(Instant::now());
            (id, name, st.bracketed_paste(id))
        };
        if !p.text.is_empty() {
            // Com bracketed paste, texto de várias linhas não é enviado no meio.
            let data = if bracketed && p.text.contains('\n') {
                format!("\x1b[200~{}\x1b[201~", p.text)
            } else {
                p.text.clone()
            };
            self.send(Command::SendInput {
                id,
                data: Bytes::from(data),
            })?;
            if p.submit {
                tokio::time::sleep(ENTER_DELAY).await;
            }
        }
        if p.submit {
            self.send(Command::SendInput {
                id,
                data: Bytes::from_static(b"\r"),
            })?;
        }
        Ok(json!({ "agent": name, "sent": true, "submitted": p.submit }))
    }

    fn read_output(&self, grant: Grant, p: ReadOutputParams) -> Result<Value, String> {
        let st = self.lock();
        let id = st.resolve(grant.session, &p.agent)?;
        let brief = st.brief(id);
        let lines = st.screen_lines(id, p.lines.clamp(1, MAX_LINES));
        let note = lines.is_none().then_some("sem saída registrada ainda");
        let lines = lines.unwrap_or_default();
        Ok(with_note(
            json!({
                "agent": brief.agent,
                "id": brief.id,
                "status": brief.status,
                "detail": brief.detail,
                "lines": lines.len(),
                "text": lines.join("\n"),
            }),
            note,
        ))
    }

    async fn wait_for(&self, grant: Grant, p: WaitForParams) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(p.timeout_secs.min(MAX_TIMEOUT_SECS));
        let mut changes = self.changes.subscribe();
        let (spec, names) = {
            let st = self.lock();
            let (targets, fresh_only) = match &p.agents {
                Some(refs) if !refs.is_empty() => {
                    let mut ids = Vec::new();
                    for r in refs {
                        let id = st.resolve(grant.session, r)?;
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    (ids, false)
                }
                _ => (st.children(grant.orchestrator), !p.all),
            };
            if targets.is_empty() {
                return Err("você ainda não tem agentes; crie com spawn_agent".to_owned());
            }
            let names: HashMap<AgentId, String> =
                targets.iter().map(|id| (*id, st.name_of(*id))).collect();
            let spec = WaitSpec {
                baseline: st.wait_baseline(&targets, p.until, Instant::now()),
                targets,
                until: p.until,
                all: p.all,
                fresh_only,
            };
            (spec, names)
        };

        // Ninguém vivo: nada pode mudar.
        let live = spec.baseline.values().any(|m| !m.terminal);
        if !live && (spec.fresh_only || spec.until == WaitUntil::Change) {
            let note = "nenhum desses agentes está vivo; nada a esperar";
            return Ok(self.wait_result(grant, &spec, &names, &[], false, Some(note)));
        }

        loop {
            let now = Instant::now();
            let (triggered, next_grace) = {
                let st = self.lock();
                if st.shutdown {
                    return Err(SHUTTING_DOWN.to_owned());
                }
                let triggered = st.wait_triggered(&spec, now);
                let done = if spec.all {
                    triggered.len() == spec.targets.len()
                } else {
                    !triggered.is_empty()
                };
                (
                    done.then_some(triggered),
                    st.next_grace_expiry(&spec.targets),
                )
            };
            if let Some(triggered) = triggered {
                return Ok(self.wait_result(grant, &spec, &names, &triggered, false, None));
            }
            if now >= deadline {
                return Ok(self.wait_result(grant, &spec, &names, &[], true, None));
            }
            let wake = next_grace
                .filter(|t| *t > now)
                .map_or(deadline, |t| t.min(deadline));
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Err(SHUTTING_DOWN.to_owned());
                    }
                }
                () = tokio::time::sleep_until(wake.into()) => {}
            }
        }
    }

    fn wait_result(
        &self,
        grant: Grant,
        spec: &WaitSpec,
        names: &HashMap<AgentId, String>,
        triggered: &[AgentId],
        timed_out: bool,
        note: Option<&str>,
    ) -> Value {
        let mut st = self.lock();
        // O orquestra já contou: o hook `Stop` não repete.
        st.consume_news(grant.orchestrator, triggered);
        let brief = |st: &State, id: &AgentId| {
            let mut b = st.brief(*id);
            if let Some(name) = names.get(id) {
                b.agent.clone_from(name);
            }
            b
        };
        let agents: Vec<_> = spec.targets.iter().map(|id| brief(&st, id)).collect();
        let triggered: Vec<_> = triggered.iter().map(|id| brief(&st, id).agent).collect();
        with_note(
            json!({
                "timed_out": timed_out,
                "triggered": triggered,
                "agents": agents,
            }),
            note,
        )
    }

    fn kill_agent(&self, grant: Grant, p: KillAgentParams) -> Result<Value, String> {
        let (id, name, status) = {
            let st = self.lock();
            let id = st.resolve(grant.session, &p.agent)?;
            if id == grant.orchestrator {
                return Err("kill_agent não encerra o próprio orquestrador".to_owned());
            }
            let status = st.agents.get(&id).and_then(|a| a.status.clone());
            (id, st.name_of(id), status)
        };
        if let Some(status) = status.filter(AgentStatus::is_terminal) {
            return Ok(json!({
                "agent": name,
                "killed": false,
                "status": status_code(&status),
                "note": "já tinha terminado",
            }));
        }
        self.send(Command::Kill { id })?;
        Ok(json!({
            "agent": name,
            "killed": true,
            "note": "o status final chega em seguida (wait_for until=done)",
        }))
    }

    async fn take_news(&self, grant: Grant, p: TakeNewsParams) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(p.wait_secs.min(MAX_TIMEOUT_SECS));
        let mut changes = self.changes.subscribe();
        loop {
            {
                let mut st = self.lock();
                let ready = st.has_news(grant.orchestrator)
                    || st.shutdown
                    || !st.has_working_children(grant.orchestrator)
                    || Instant::now() >= deadline;
                if ready {
                    return to_value(st.take_news(grant.orchestrator));
                }
            }
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return to_value(self.lock().take_news(grant.orchestrator));
                    }
                }
                () = tokio::time::sleep_until(deadline.into()) => {}
            }
        }
    }

    fn send(&self, cmd: Command) -> Result<(), String> {
        self.commands.send(cmd).map_err(|_| HUB_DOWN.to_owned())
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, String> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|err| format!("parâmetros inválidos: {err}"))
}

fn to_value(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| err.to_string())
}

/// Tira campos `null` e acrescenta `note` se houver.
fn with_note(mut value: Value, note: Option<&str>) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, v| !v.is_null());
        if let Some(note) = note {
            map.insert("note".to_owned(), Value::from(note));
        }
    }
    value
}

/// Nome seguro para branch e para referência: letras, números, `-` e `_`.
pub(crate) fn normalize_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(MAX_NAME_CHARS).collect();
    let out = out.trim_matches('-');
    (!out.is_empty()).then(|| out.to_owned())
}

/// Minúsculas, só letras e números ("Claude Code" → "claudecode").
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_kind(raw: &str) -> Option<AgentKind> {
    match squash(raw).as_str() {
        "claude" | "claudecode" => Some(AgentKind::ClaudeCode),
        "codex" => Some(AgentKind::Codex),
        "shell" | "bash" | "sh" => Some(AgentKind::Shell),
        "custom" => Some(AgentKind::Custom),
        _ => None,
    }
}

/// Preset do tipo pedido (ou do primeiro permitido), respeitando
/// `allowed_kinds`. `kind` também aceita o nome de um preset.
pub(crate) fn choose_preset(
    session: &SessionConfig,
    presets: &[AgentPreset],
    kind: Option<&str>,
) -> Result<AgentPreset, String> {
    let allowed = &session.allowed_kinds;
    let Some(first) = allowed.first() else {
        return Err("esta sessão não permite criar agentes".to_owned());
    };
    let allowed_list = allowed
        .iter()
        .map(|k| kind_code(*k))
        .collect::<Vec<_>>()
        .join(", ");
    let (kind, preset) = match kind.map(str::trim).filter(|k| !k.is_empty()) {
        None => (*first, None),
        Some(raw) => match parse_kind(raw) {
            Some(kind) => (kind, None),
            None => {
                let wanted = squash(raw);
                let preset = presets
                    .iter()
                    .find(|p| squash(&p.name) == wanted)
                    .ok_or_else(|| format!("tipo desconhecido: {raw} (use: {allowed_list})"))?;
                (preset.kind, Some(preset.clone()))
            }
        },
    };
    if !allowed.contains(&kind) {
        return Err(format!(
            "o tipo {} não é permitido nesta sessão (permitidos: {allowed_list})",
            kind_code(kind)
        ));
    }
    match preset {
        Some(preset) => Ok(preset),
        None => presets
            .iter()
            .find(|p| p.kind == kind)
            .cloned()
            .ok_or_else(|| format!("nenhum preset configurado para o tipo {}", kind_code(kind))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(normalize_name("  API login ").as_deref(), Some("API-login"));
        assert_eq!(normalize_name("a/b..c").as_deref(), Some("a-b-c"));
        assert_eq!(normalize_name("ção_1").as_deref(), Some("ção_1"));
        assert_eq!(normalize_name(" !!! "), None);
        assert_eq!(
            normalize_name(&"x".repeat(99)).unwrap().len(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn kind_aliases_presets_and_allowed_kinds() {
        let presets = AgentPreset::defaults();
        let session = SessionConfig::new("s", "/r", "g");
        let pick = |k: Option<&str>| choose_preset(&session, &presets, k).map(|p| p.kind);
        assert_eq!(pick(None), Ok(AgentKind::ClaudeCode));
        assert_eq!(pick(Some("Claude")), Ok(AgentKind::ClaudeCode));
        assert_eq!(pick(Some("claude-code")), Ok(AgentKind::ClaudeCode));
        assert_eq!(pick(Some("codex")), Ok(AgentKind::Codex));
        assert!(pick(Some("shell")).unwrap_err().contains("não é permitido"));
        assert!(pick(Some("gemini")).unwrap_err().contains("desconhecido"));

        let only_codex = SessionConfig {
            allowed_kinds: vec![AgentKind::Codex],
            ..session.clone()
        };
        assert_eq!(
            choose_preset(&only_codex, &presets, None).map(|p| p.program),
            Ok("codex".to_owned())
        );
        let none = SessionConfig {
            allowed_kinds: vec![],
            ..session
        };
        assert!(choose_preset(&none, &presets, None).is_err());
    }
}
