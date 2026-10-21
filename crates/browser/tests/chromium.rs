//! Integração com um Chromium de verdade. Sem Chromium na máquina, os testes
//! avisam e passam. Só rede local: uma página servida num `TcpListener`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chromiumoxide::Connection;
use chromiumoxide::types::{CdpJsonEventMessage, Message};
use futures::StreamExt;
use orquestra_browser::{BrowserProcess, Chromium, find_executable};
use orquestra_core::{
    AgentEvent, AgentId, BrowserConfig, BrowserEndpoint, BrowserFrame, BrowserInput,
    BrowserManager, BrowserStatus, EventBus, EventReceiver, KeyAction, KeyInput, Modifiers,
    MouseAction, MouseButton, MouseInput, Navigation, Viewport,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

const WAIT: Duration = Duration::from_secs(15);

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Teste</title>
<style>
  body { margin: 0; font: 16px sans-serif; }
  #btn { position: absolute; left: 40px; top: 40px; width: 200px; height: 60px; }
  #out { position: absolute; left: 300px; top: 60px; }
  #inp { position: absolute; left: 40px; top: 160px; width: 300px; height: 32px; }
</style></head>
<body>
  <button id="btn" onclick="document.getElementById('out').textContent = 'clicado'">Clique</button>
  <div id="out">inicial</div>
  <input id="inp">
</body></html>"#;

const PAGE_B: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Segunda</title></head>
<body style="margin:0;background:#336;color:#fff">segunda página</body></html>"#;

/// `RUST_LOG=orquestra_browser=debug` liga os logs do crate nos testes.
fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn config_or_skip(test: &str) -> Option<BrowserConfig> {
    init_logs();
    let config = BrowserConfig::default();
    match find_executable(&config) {
        Some(exe) => {
            eprintln!("{test}: usando {}", exe.display());
            Some(config)
        }
        None => {
            eprintln!("{test}: PULADO, nenhum Chromium encontrado (defina ORQUESTRA_CHROMIUM)");
            None
        }
    }
}

// ---- servidor HTTP local ----

async fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&buf);
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let path = path.split('?').next().unwrap_or("/");
                let (status, body) = match path {
                    "/" => ("200 OK", PAGE),
                    "/b" => ("200 OK", PAGE_B),
                    _ => ("404 Not Found", ""),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

/// GET mínimo. O servidor de debug do Chromium ignora `Connection: close`:
/// lê pelo `Content-Length`.
async fn http_get(port: u16, path: &str) -> String {
    let get = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let length = head
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + length {
                    let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + length]);
                    return (head, body.into_owned());
                }
            }
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "GET {path}: conexão fechou antes do fim");
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    let (head, body) = tokio::time::timeout(WAIT, get)
        .await
        .unwrap_or_else(|_| panic!("GET {path} sem resposta"));
    assert!(head.starts_with("HTTP/1.1 200"), "{path}: {head}");
    body
}

// ---- cliente CDP do teste (faz o papel do agente) ----

struct Client {
    conn: Connection<CdpJsonEventMessage>,
}

impl Client {
    async fn connect(ws_url: &str) -> Self {
        let conn = tokio::time::timeout(WAIT, Connection::connect(ws_url))
            .await
            .unwrap_or_else(|_| panic!("WebSocket {ws_url} não conectou"))
            .unwrap();
        Self { conn }
    }

    /// Conecta direto numa aba (`/devtools/page/<id>`) cuja URL satisfaz `pred`.
    async fn page(port: u16, pred: impl Fn(&str) -> bool) -> Self {
        let deadline = Instant::now() + WAIT;
        loop {
            let list: Value = serde_json::from_str(&http_get(port, "/json/list").await).unwrap();
            let found = list
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["type"] == "page" && pred(t["url"].as_str().unwrap_or_default()));
            if let Some(ws) = found.and_then(|t| t["webSocketDebuggerUrl"].as_str()) {
                return Self::connect(ws).await;
            }
            assert!(
                Instant::now() < deadline,
                "aba não apareceu em /json/list: {list}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self
            .conn
            .submit_command(method.to_owned().into(), None, params)
            .unwrap();
        let wait = async {
            loop {
                match self.conn.next().await {
                    Some(Ok(Message::Response(r))) if r.id == id => {
                        if let Some(e) = r.error {
                            panic!("{method}: {e}");
                        }
                        return r.result.unwrap_or_default();
                    }
                    Some(_) => continue,
                    None => panic!("{method}: conexão fechou"),
                }
            }
        };
        tokio::time::timeout(WAIT, wait).await.expect(method)
    }

    async fn eval(&mut self, expression: &str) -> Value {
        let params = json!({"expression": expression, "returnByValue": true, "awaitPromise": true});
        self.call("Runtime.evaluate", params).await["result"]["value"].clone()
    }

    /// Avalia até o resultado bater (a página reage de forma assíncrona).
    async fn eval_until(&mut self, expression: &str, expected: Value) {
        let deadline = Instant::now() + WAIT;
        loop {
            let value = self.eval(expression).await;
            if value == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{expression} = {value}, esperado {expected}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Centro do elemento, em pixels CSS do viewport.
    async fn center(&mut self, selector: &str) -> (f64, f64) {
        let rect = self
            .eval(&format!(
                "(() => {{ const r = document.querySelector('{selector}').getBoundingClientRect(); \
                 return [r.x + r.width / 2, r.y + r.height / 2]; }})()"
            ))
            .await;
        (rect[0].as_f64().unwrap(), rect[1].as_f64().unwrap())
    }
}

// ---- eventos ----

async fn wait_event<T>(
    rx: &mut EventReceiver,
    what: &str,
    mut f: impl FnMut(&AgentEvent) -> Option<T>,
) -> T {
    let mut seen = Vec::new();
    let wait = async {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(found) = f(&event) {
                        return found;
                    }
                    if !event.is_frame() {
                        seen.push(format!("{event:?}"));
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => panic!("bus fechou esperando {what}"),
            }
        }
    };
    match tokio::time::timeout(WAIT, wait).await {
        Ok(found) => found,
        Err(_) => panic!("tempo esgotado esperando {what}; vistos: {seen:#?}"),
    }
}

async fn wait_status(rx: &mut EventReceiver, id: AgentId) -> BrowserStatus {
    wait_event(rx, "BrowserStatus", |e| match e {
        AgentEvent::BrowserStatus { id: got, status } if *got == id => Some(status.clone()),
        _ => None,
    })
    .await
}

async fn wait_navigated(rx: &mut EventReceiver, id: AgentId, url: &str, title: &str) {
    wait_event(
        rx,
        &format!("BrowserNavigated {url} \"{title}\""),
        |e| match e {
            AgentEvent::BrowserNavigated {
                id: got,
                url: u,
                title: t,
            } if *got == id && u == url && t == title => Some(()),
            _ => None,
        },
    )
    .await
}

async fn wait_frame(
    rx: &mut EventReceiver,
    id: AgentId,
    pred: impl Fn(&BrowserFrame) -> bool,
) -> BrowserFrame {
    wait_event(rx, "BrowserFrame", |e| match e {
        AgentEvent::BrowserFrame { id: got, frame } if *got == id && pred(frame) => {
            Some((**frame).clone())
        }
        _ => None,
    })
    .await
}

// ---- input ----

fn mouse(action: MouseAction, (x, y): (f64, f64)) -> BrowserInput {
    BrowserInput::Mouse(MouseInput {
        action,
        x,
        y,
        button: if action == MouseAction::Move {
            MouseButton::None
        } else {
            MouseButton::Left
        },
        click_count: 1,
        modifiers: Modifiers::default(),
    })
}

async fn click(chromium: &Chromium, id: AgentId, at: (f64, f64)) {
    for action in [MouseAction::Move, MouseAction::Down, MouseAction::Up] {
        chromium.input(id, mouse(action, at)).await.unwrap();
    }
}

async fn press(chromium: &Chromium, id: AgentId, key: &str, code: &str, text: Option<&str>) {
    for action in [KeyAction::Down, KeyAction::Up] {
        let input = KeyInput {
            action,
            key: key.to_owned(),
            code: code.to_owned(),
            text: (action == KeyAction::Down)
                .then(|| text.map(str::to_owned))
                .flatten(),
            modifiers: Modifiers::default(),
        };
        chromium.input(id, BrowserInput::Key(input)).await.unwrap();
    }
}

// ---- processos ----

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Processos cuja linha de comando cita o perfil (o Chromium e seus filhos).
fn processes_using(profile: &Path) -> Vec<u32> {
    let needle = profile.to_string_lossy().into_owned();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| {
        let e = e.ok()?;
        let pid: u32 = e.file_name().to_str()?.parse().ok()?;
        let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
        String::from_utf8_lossy(&cmdline)
            .contains(&needle)
            .then_some(pid)
    })
    .collect()
}

async fn assert_gone(process: &BrowserProcess) {
    assert!(!alive(process.pid), "processo {} ainda vivo", process.pid);
    assert!(!process.profile_dir.exists(), "perfil não foi apagado");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = processes_using(&process.profile_dir);
        if left.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "filhos do Chromium sobraram: {left:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn assert_endpoint(endpoint: &BrowserEndpoint) {
    let version: Value =
        serde_json::from_str(&http_get(endpoint.port, "/json/version").await).unwrap();
    assert_eq!(version["webSocketDebuggerUrl"], endpoint.ws_url.as_str());
    assert!(endpoint.ws_url.starts_with(&format!(
        "ws://127.0.0.1:{}/devtools/browser/",
        endpoint.port
    )));
}

// ---- testes ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_stream_click_type_resize_close() {
    let Some(config) = config_or_skip("launch_stream_click_type_resize_close") else {
        return;
    };
    let started = Instant::now();
    let site = serve().await;
    let home = format!("http://127.0.0.1:{site}/");
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let mut frames = bus.subscribe_frames();
    let chromium = Chromium::new(bus.clone(), config);
    let id = AgentId::generate();

    let endpoint = chromium.launch(id).await.unwrap();
    eprintln!("launch: {:?}", started.elapsed());
    assert_eq!(wait_status(&mut events, id).await, BrowserStatus::Launching);
    assert_eq!(
        wait_status(&mut events, id).await,
        BrowserStatus::Ready {
            endpoint: endpoint.clone()
        }
    );
    assert_endpoint(&endpoint).await;
    let process = chromium.process(id).unwrap();
    assert!(alive(process.pid));
    assert!(process.profile_dir.exists());

    // Idempotente: mesmo endpoint, nenhum processo novo.
    assert_eq!(chromium.launch(id).await.unwrap(), endpoint);
    assert_eq!(chromium.process(id).unwrap(), process);

    // Navegação → BrowserNavigated com URL e título.
    chromium
        .navigate(id, Navigation::Url(home.clone()))
        .await
        .unwrap();
    wait_navigated(&mut events, id, &home, "Teste").await;

    // Screencast: JPEG válido, no tamanho do viewport (moldura corrigida).
    chromium.set_streaming(id, true).await.unwrap();
    let frame = wait_frame(&mut frames, id, |_| true).await;
    assert_eq!(&frame.data[..2], &[0xFF, 0xD8], "não é JPEG");
    assert_eq!((frame.css_width, frame.css_height), (1280.0, 800.0));
    assert!(frame.seq >= 1);

    // Clique no botão por BrowserInput::Mouse.
    let mut page = Client::page(endpoint.port, |u| u == home).await;
    assert_eq!(
        page.eval("document.getElementById('out').textContent")
            .await,
        "inicial"
    );
    let button = page.center("#btn").await;
    click(&chromium, id, button).await;
    page.eval_until(
        "document.getElementById('out').textContent",
        json!("clicado"),
    )
    .await;

    // Digitação: teclas (com Backspace) e texto direto.
    let field = page.center("#inp").await;
    click(&chromium, id, field).await;
    page.eval_until("document.activeElement.id", json!("inp"))
        .await;
    press(&chromium, id, "a", "KeyA", Some("a")).await;
    press(&chromium, id, "b", "KeyB", Some("b")).await;
    press(&chromium, id, "Backspace", "Backspace", None).await;
    press(&chromium, id, "A", "KeyA", Some("A")).await;
    chromium
        .input(id, BrowserInput::Text("ção!".to_owned()))
        .await
        .unwrap();
    press(&chromium, id, "ArrowLeft", "ArrowLeft", None).await;
    press(&chromium, id, "Delete", "Delete", None).await;
    page.eval_until("document.getElementById('inp').value", json!("aAção"))
        .await;

    // Resize: o viewport segue e os quadros também.
    let viewport = Viewport {
        width: 1000,
        height: 640,
        device_scale_factor: 1.0,
    };
    chromium.resize(id, viewport).await.unwrap();
    page.eval_until("[innerWidth, innerHeight]", json!([1000, 640]))
        .await;
    let frame = wait_frame(&mut frames, id, |f| f.css_width == 1000.0).await;
    assert_eq!(frame.css_height, 640.0);

    // Histórico: vai para /b e volta.
    let second = format!("http://127.0.0.1:{site}/b");
    chromium
        .navigate(id, Navigation::Url(second.clone()))
        .await
        .unwrap();
    wait_navigated(&mut events, id, &second, "Segunda").await;
    chromium.navigate(id, Navigation::Back).await.unwrap();
    wait_navigated(&mut events, id, &home, "Teste").await;

    // Streaming desligado: nenhum quadro, mesmo com a página mudando.
    chromium.set_streaming(id, false).await.unwrap();
    while frames.try_recv().is_ok() {}
    let mut page = Client::page(endpoint.port, |u| u == home).await;
    page.eval("document.body.style.background = 'red'; document.getElementById('out').textContent = 'mudou'").await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(
        matches!(frames.try_recv(), Err(TryRecvError::Empty)),
        "quadro com streaming desligado"
    );

    // Close: processo morto, perfil apagado, Closed publicado; repetir é Ok.
    chromium.close(id).await.unwrap();
    assert_eq!(wait_status(&mut events, id).await, BrowserStatus::Closed);
    assert_gone(&process).await;
    assert!(chromium.process(id).is_none());
    chromium.close(id).await.unwrap();
    assert!(chromium.set_streaming(id, false).await.is_ok());
    assert!(chromium.set_streaming(id, true).await.is_err());
    eprintln!("total: {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_are_isolated() {
    let Some(config) = config_or_skip("agents_are_isolated") else {
        return;
    };
    let site = serve().await;
    let home = format!("http://127.0.0.1:{site}/");
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let chromium = Chromium::new(bus.clone(), config);
    let (a, b) = (AgentId::generate(), AgentId::generate());

    let (ea, eb) = tokio::join!(chromium.launch(a), chromium.launch(b));
    let (ea, eb) = (ea.unwrap(), eb.unwrap());
    let (pa, pb) = (chromium.process(a).unwrap(), chromium.process(b).unwrap());
    assert_ne!(ea.port, eb.port);
    assert_ne!(pa.pid, pb.pid);
    assert_ne!(pa.profile_dir, pb.profile_dir);
    let mut open = chromium.open();
    open.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(open, expected);

    let home_b = format!("{home}?b");
    chromium
        .navigate(a, Navigation::Url(home.clone()))
        .await
        .unwrap();
    chromium
        .navigate(b, Navigation::Url(home_b.clone()))
        .await
        .unwrap();
    let mut page_a = Client::page(ea.port, |u| u == home).await;
    let mut page_b = Client::page(eb.port, |u| u == home_b).await;

    // Mesma origem, perfis diferentes: nada vaza de A para B.
    page_a
        .eval("localStorage.setItem('k', 'a'); document.cookie = 'c=1; path=/'; 'ok'")
        .await;
    assert_eq!(page_a.eval("localStorage.getItem('k')").await, "a");
    assert_eq!(page_a.eval("document.cookie").await, "c=1");
    assert_eq!(page_b.eval("localStorage.getItem('k')").await, Value::Null);
    assert_eq!(page_b.eval("document.cookie").await, "");

    // A porta de A só lista as abas de A.
    let list_a = http_get(ea.port, "/json/list").await;
    assert!(
        list_a.contains(&home) && !list_a.contains(&home_b),
        "{list_a}"
    );

    chromium.shutdown().await;
    let mut closed = Vec::new();
    while closed.len() < 2 {
        if let AgentEvent::BrowserStatus {
            id,
            status: BrowserStatus::Closed,
        } = wait_event(&mut events, "Closed", |e| Some(e.clone())).await
        {
            closed.push(id);
        }
    }
    assert!(closed.contains(&a) && closed.contains(&b));
    assert_gone(&pa).await;
    assert_gone(&pb).await;
    assert!(chromium.open().is_empty());

    // Depois do shutdown, um launch novo sobe outro processo.
    let again = chromium.launch(a).await.unwrap();
    assert_endpoint(&again).await;
    let pa2 = chromium.process(a).unwrap();
    assert_ne!(pa2.pid, pa.pid);
    chromium.close(a).await.unwrap();
    assert_gone(&pa2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screencast_follows_the_active_tab() {
    let Some(config) = config_or_skip("screencast_follows_the_active_tab") else {
        return;
    };
    let site = serve().await;
    let home = format!("http://127.0.0.1:{site}/");
    let second = format!("http://127.0.0.1:{site}/b");
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let mut frames = bus.subscribe_frames();
    let chromium = Chromium::new(bus.clone(), config);
    let id = AgentId::generate();
    let endpoint = chromium.launch(id).await.unwrap();
    chromium
        .navigate(id, Navigation::Url(home.clone()))
        .await
        .unwrap();
    wait_navigated(&mut events, id, &home, "Teste").await;
    chromium.set_streaming(id, true).await.unwrap();
    wait_frame(&mut frames, id, |_| true).await;

    // O "agente" abre uma aba nova pela própria conexão CDP (como o
    // Playwright faz no `newPage`): ela vira a ativa.
    let mut agent = Client::connect(&endpoint.ws_url).await;
    let created = agent
        .call("Target.createTarget", json!({"url": second}))
        .await;
    let second_tab = created["targetId"].as_str().unwrap().to_owned();
    wait_navigated(&mut events, id, &second, "Segunda").await;
    while frames.try_recv().is_ok() {}
    wait_frame(&mut frames, id, |_| true).await;

    // A aba antiga volta para a frente (`bringToFront`): o screencast volta.
    let mut first = Client::page(endpoint.port, |u| u == home).await;
    first.call("Page.bringToFront", json!({})).await;
    wait_navigated(&mut events, id, &home, "Teste").await;

    // O input vai para a aba ativa.
    let button = first.center("#btn").await;
    click(&chromium, id, button).await;
    first
        .eval_until(
            "document.getElementById('out').textContent",
            json!("clicado"),
        )
        .await;

    // Uma terceira aba, depois fechada: volta para a mais recente que sobrou.
    let third = format!("{home}?3");
    let created = agent
        .call("Target.createTarget", json!({"url": third}))
        .await;
    wait_navigated(&mut events, id, &third, "Teste").await;
    agent
        .call(
            "Target.closeTarget",
            json!({"targetId": created["targetId"]}),
        )
        .await;
    wait_navigated(&mut events, id, &second, "Segunda").await;
    agent
        .call("Target.closeTarget", json!({"targetId": second_tab}))
        .await;
    wait_navigated(&mut events, id, &home, "Teste").await;

    chromium.shutdown().await;
}

/// O Chromium do Playwright não tem perfil do AppArmor: no Ubuntu 23.10+ o
/// sandbox falha e o lançamento repete com `--no-sandbox`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playwright_build_launches() {
    let Some(exe) = playwright_chromium() else {
        eprintln!(
            "playwright_build_launches: PULADO, sem Chromium do Playwright em ~/.cache/ms-playwright"
        );
        return;
    };
    eprintln!("playwright_build_launches: usando {}", exe.display());
    let config = BrowserConfig {
        executable: Some(exe),
        ..BrowserConfig::default()
    };
    let chromium = Chromium::new(EventBus::new(), config);
    let id = AgentId::generate();
    let endpoint = chromium.launch(id).await.unwrap();
    assert_endpoint(&endpoint).await;
    let process = chromium.process(id).unwrap();
    chromium.close(id).await.unwrap();
    assert_gone(&process).await;
}

fn playwright_chromium() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("HOME")?).join(".cache/ms-playwright");
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("chromium-"))
                .is_some_and(|rev| rev.parse::<u64>().is_ok())
        })
        .collect();
    dirs.sort();
    dirs.into_iter()
        .rev()
        .flat_map(|d| {
            [
                d.join("chrome-linux64/chrome"),
                d.join("chrome-linux/chrome"),
            ]
        })
        .find(|p| p.is_file())
}

/// Um "navegador" que sai na hora: `Launching` → `Failed`, erro no `launch`.
#[tokio::test]
async fn launch_failure_is_reported() {
    let config = BrowserConfig {
        executable: Some("/bin/false".into()),
        ..BrowserConfig::default()
    };
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let chromium = Chromium::new(bus.clone(), config);
    let id = AgentId::generate();
    let err = chromium.launch(id).await.unwrap_err();
    assert!(
        err.to_string().contains("saiu durante a inicialização"),
        "{err}"
    );
    assert_eq!(wait_status(&mut events, id).await, BrowserStatus::Launching);
    assert!(matches!(
        wait_status(&mut events, id).await,
        BrowserStatus::Failed { .. }
    ));
    assert!(chromium.process(id).is_none());
}

// ---- agente de verdade: Playwright via CDP ----

/// Roteiro do "agente": o mesmo caminho do Playwright MCP (`connectOverCDP`).
const PLAYWRIGHT_AGENT: &str = r#"
const { chromium } = require(process.env.PLAYWRIGHT_CORE);
(async () => {
  const [cdp, site] = process.argv.slice(2);
  const browser = await chromium.connectOverCDP(cdp);
  const context = browser.contexts()[0];
  const page = context.pages()[0];
  await page.goto(site + '/');
  const page2 = await context.newPage();
  await page2.goto(site + '/b');
  await new Promise(r => setTimeout(r, 300));
  await page.bringToFront();
  await page.click('#btn');
  if (await page.textContent('#out') !== 'clicado') throw new Error('clique não pegou');
  await page2.close();
  await new Promise(r => setTimeout(r, 300));
  await browser.close(); // em connectOverCDP só desconecta
})().catch(e => { console.error(e); process.exit(1); });
"#;

/// `playwright-core` local: `$ORQUESTRA_PLAYWRIGHT_CORE` ou o pacote que o
/// playwright-go baixa em `~/.cache/ms-playwright-go/<versão>/package`.
fn playwright_core() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ORQUESTRA_PLAYWRIGHT_CORE") {
        return Some(PathBuf::from(dir));
    }
    let root = PathBuf::from(std::env::var_os("HOME")?).join(".cache/ms-playwright-go");
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path().join("package")))
        .filter(|p| p.join("package.json").is_file())
        .collect();
    dirs.sort();
    dirs.pop()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playwright_agent_shares_the_browser() {
    let Some(config) = config_or_skip("playwright_agent_shares_the_browser") else {
        return;
    };
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let Some(core) = playwright_core().filter(|_| node_ok) else {
        eprintln!("playwright_agent_shares_the_browser: PULADO, sem node ou playwright-core");
        return;
    };
    let script = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
    std::fs::write(script.path(), PLAYWRIGHT_AGENT).unwrap();

    let site = serve().await;
    let home = format!("http://127.0.0.1:{site}/");
    let second = format!("http://127.0.0.1:{site}/b");
    let bus = EventBus::new();
    let mut events = bus.subscribe();
    let mut frames = bus.subscribe_frames();
    let chromium = Chromium::new(bus.clone(), config);
    let id = AgentId::generate();
    let endpoint = chromium.launch(id).await.unwrap();
    chromium.set_streaming(id, true).await.unwrap();

    let mut agent = tokio::process::Command::new("node")
        .arg(script.path())
        .arg(endpoint.http_url())
        .arg(format!("http://127.0.0.1:{site}"))
        .env("PLAYWRIGHT_CORE", &core)
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // O que o agente faz aparece para a UI: navegação, aba nova, volta.
    wait_navigated(&mut events, id, &home, "Teste").await;
    wait_navigated(&mut events, id, &second, "Segunda").await;
    wait_frame(&mut frames, id, |_| true).await;
    wait_navigated(&mut events, id, &home, "Teste").await;
    let status = tokio::time::timeout(Duration::from_secs(60), agent.wait())
        .await
        .expect("agente Playwright travou")
        .unwrap();
    assert!(status.success(), "agente Playwright falhou: {status}");

    // O agente desconectou; o navegador segue nosso.
    chromium
        .navigate(id, Navigation::Url(second.clone()))
        .await
        .unwrap();
    wait_navigated(&mut events, id, &second, "Segunda").await;
    chromium.shutdown().await;
}
