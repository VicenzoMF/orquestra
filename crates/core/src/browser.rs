use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Porta de debug (CDP) do Chromium de um agente, em 127.0.0.1.
/// É o que o agente usa via Playwright MCP / chrome-devtools MCP.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrowserEndpoint {
    pub port: u16,
    /// WebSocket do browser (`ws://127.0.0.1:<porta>/devtools/browser/<id>`).
    pub ws_url: String,
}

impl BrowserEndpoint {
    /// `http://127.0.0.1:<porta>`: aceito por `--cdp-endpoint` (Playwright MCP)
    /// e `--browserUrl` (chrome-devtools MCP).
    pub fn http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Estado do Chromium de um agente. Publicado pelo browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserStatus {
    Launching,
    Ready { endpoint: BrowserEndpoint },
    Closed,
    Failed { reason: String },
}

/// Viewport da página, em pixels CSS.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
            device_scale_factor: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Jpeg,
    Png,
}

/// Um quadro do `Page.screencastFrame`, já decodificado do base64. A
/// decodificação da imagem é da UI, fora da thread de render.
#[derive(Clone, Debug, PartialEq)]
pub struct BrowserFrame {
    /// Crescente por agente; a UI descarta quadros mais velhos que o exibido.
    pub seq: u64,
    pub format: ImageFormat,
    /// Imagem codificada (JPEG/PNG).
    pub data: Bytes,
    /// Área da página coberta pela imagem, em pixels CSS (`deviceWidth`/`deviceHeight`).
    pub css_width: f64,
    pub css_height: f64,
    pub page_scale: f64,
    pub offset_top: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
}

impl BrowserFrame {
    /// Converte um ponto normalizado sobre a imagem (`u`, `v` em 0..=1) em
    /// coordenadas CSS do viewport, que é o que o `BrowserInput` espera.
    pub fn to_viewport(&self, u: f32, v: f32) -> (f64, f64) {
        let u = f64::from(u.clamp(0.0, 1.0));
        let v = f64::from(v.clamp(0.0, 1.0));
        (u * self.css_width, v * self.css_height)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Navigation {
    Url(String),
    Back,
    Forward,
    Reload,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub meta: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MouseButton {
    #[default]
    None,
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseAction {
    Move,
    Down,
    Up,
}

/// Coordenadas em pixels CSS a partir do canto superior esquerdo do viewport
/// (ver `BrowserFrame::to_viewport`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseInput {
    pub action: MouseAction,
    pub x: f64,
    pub y: f64,
    pub button: MouseButton,
    /// 1 = clique simples, 2 = duplo...
    pub click_count: u32,
    pub modifiers: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelInput {
    pub x: f64,
    pub y: f64,
    /// Em pixels CSS; positivo = rolar para baixo/direita.
    pub delta_x: f64,
    pub delta_y: f64,
    pub modifiers: Modifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyAction {
    Down,
    Up,
}

/// Tecla em termos DOM; o browser traduz para `Input.dispatchKeyEvent`
/// (incluindo `windowsVirtualKeyCode`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub action: KeyAction,
    /// `KeyboardEvent.key`: `"a"`, `"Enter"`, `"ArrowLeft"`...
    pub key: String,
    /// `KeyboardEvent.code`: `"KeyA"`, `"Enter"`... (vazio se desconhecido).
    pub code: String,
    /// Texto produzido (só em `Down` de tecla imprimível).
    pub text: Option<String>,
    pub modifiers: Modifiers,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrowserInput {
    Mouse(MouseInput),
    Wheel(WheelInput),
    Key(KeyInput),
    /// Inserção direta de texto (`Input.insertText`): colar, IME.
    Text(String),
}
